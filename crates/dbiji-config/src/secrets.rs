//! Secret storage (FR-C4): passwords, passphrases and API keys never touch
//! the configuration files. Two backends:
//!
//! * [`KeychainStore`] — the OS credential store through `keyring` 4
//!   (macOS Keychain, Windows Credential Manager, Secret Service), service
//!   `dev.dbiji.DBiji`, account = secret key.
//! * [`VaultStore`] — `vault.bin`, encrypted with a key derived from a master
//!   password (Argon2id) using XChaCha20-Poly1305.
//!
//! [`MemoryStore`] is an in-process store for tests. [`SecretsManager`]
//! chooses the backend from the settings and migrates secrets between them.
//!
//! Keys: `conn:<id>:password|ssh-password|ssh-passphrase|proxy-password`
//! ([`dbiji_core::config::secret_key`]), `profile:<id>:<slot>` for network
//! profiles and `ai:<provider>:api-key` ([`ai_key`]).

use crate::paths::{AppPaths, write_secure};
use crate::settings::{SecretBackend, SecuritySettings};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use dbiji_core::config::{SecretSlot, secret_key};
use dbiji_core::{DbError, DbResult, ErrorKind, Secret};
use parking_lot::{Mutex, RwLock};
use rand::TryRng;
use rand::rngs::SysRng;
use serde::ser::SerializeMap;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// A place where secrets are kept.
pub trait SecretStore: Send + Sync {
    /// The secret stored under `key`, if any.
    fn get(&self, key: &str) -> DbResult<Option<Secret>>;
    /// Stores (or replaces) a secret.
    fn set(&self, key: &str, value: &Secret) -> DbResult<()>;
    /// Removes a secret; removing a missing key succeeds.
    fn delete(&self, key: &str) -> DbResult<()>;
    /// Short backend identifier (`keychain`, `vault`, `memory`).
    fn backend_name(&self) -> &'static str;
    /// True when the store must be unlocked before use.
    fn is_locked(&self) -> bool;
    /// Keys present in the store, when the backend can enumerate them
    /// (the OS keychain cannot).
    fn list_keys(&self) -> DbResult<Vec<String>> {
        Err(DbError::unsupported(format!(
            "The {} secret store cannot list its keys",
            self.backend_name()
        )))
    }
}

/// Key of an AI provider API key: `ai:<provider>:api-key`.
pub fn ai_key(provider: &str) -> String {
    format!("ai:{provider}:api-key")
}

// ------------------------------------------------------------------ memory

/// In-memory store (tests, or a session-only store).
#[derive(Default)]
pub struct MemoryStore {
    entries: RwLock<BTreeMap<String, Secret>>,
    locked: AtomicBool,
}

impl MemoryStore {
    /// An empty, unlocked store.
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }

    /// Simulates a locked store (operations fail with [`ErrorKind::Auth`]).
    pub fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::SeqCst);
    }

    /// Number of stored secrets.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    fn check(&self) -> DbResult<()> {
        if self.is_locked() {
            Err(DbError::auth("The secret store is locked"))
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("entries", &self.len())
            .finish()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        self.check()?;
        Ok(self.entries.read().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.check()?;
        self.entries.write().insert(key.to_string(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        self.check()?;
        self.entries.write().remove(key);
        Ok(())
    }
    fn backend_name(&self) -> &'static str {
        "memory"
    }
    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }
    fn list_keys(&self) -> DbResult<Vec<String>> {
        self.check()?;
        Ok(self.entries.read().keys().cloned().collect())
    }
}

// ---------------------------------------------------------------- keychain

/// OS credential store via `keyring` 4 (its `v1` feature selects the
/// platform store: `apple-native-keyring-store` on macOS, Windows Credential
/// Manager, or the Secret Service).
#[derive(Debug, Clone)]
pub struct KeychainStore {
    service: String,
}

impl Default for KeychainStore {
    fn default() -> Self {
        KeychainStore::new()
    }
}

impl KeychainStore {
    /// Service name under which DBiji stores its entries.
    pub const SERVICE: &'static str = "dev.dbiji.DBiji";

    /// Store using [`KeychainStore::SERVICE`].
    pub fn new() -> KeychainStore {
        KeychainStore {
            service: Self::SERVICE.to_string(),
        }
    }

    /// Store using another service name (tests).
    pub fn with_service(service: impl Into<String>) -> KeychainStore {
        KeychainStore {
            service: service.into(),
        }
    }

    /// True when the platform credential store could be initialised.
    pub fn is_available() -> bool {
        keyring::Entry::store_status().is_ok()
    }

    fn entry(&self, key: &str) -> DbResult<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(|e| map_keyring_error(e, "open"))
    }
}

fn map_keyring_error(e: keyring::Error, op: &str) -> DbError {
    match e {
        keyring::Error::NoStorageAccess(inner) => DbError::auth(format!(
            "Access to the system keychain was denied or it is locked ({op}): {inner}"
        )),
        keyring::Error::NoDefaultStore => {
            DbError::unsupported("No system keychain is available on this computer")
        }
        keyring::Error::BadEncoding(_) => {
            DbError::config("A keychain entry does not contain valid UTF-8 text")
        }
        keyring::Error::TooLong(attr, limit) => DbError::config(format!(
            "The keychain rejected a value: '{attr}' is longer than {limit} characters"
        )),
        other => DbError::io(format!("Keychain error ({op}): {other}")),
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        match self.entry(key)?.get_password() {
            Ok(pw) => Ok(Some(Secret::from(pw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring_error(e, "read")),
        }
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.entry(key)?
            .set_password(value.expose())
            .map_err(|e| map_keyring_error(e, "write"))
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(e, "delete")),
        }
    }
    fn backend_name(&self) -> &'static str {
        "keychain"
    }
    fn is_locked(&self) -> bool {
        false
    }
}

// ----------------------------------------------------------------- bundled

/// Keeps every secret in one entry of another store, cached in memory.
///
/// With the macOS Keychain each entry has its own access list: one entry per
/// password meant one permission prompt per password, again after every
/// rebuild of an unsigned app and on every read when the user picked "Allow"
/// rather than "Always Allow". One bundle entry read once per process means
/// at most one prompt per launch. Entries of earlier versions (one per key)
/// are moved into the bundle the first time they are read.
pub struct BundledStore<S: SecretStore> {
    inner: S,
    state: Mutex<BundleState>,
}

#[derive(Default)]
struct BundleState {
    /// The bundle, once read.
    secrets: Option<BTreeMap<String, Secret>>,
    /// Keys looked up in the old per-key entries and not found.
    no_legacy: std::collections::HashSet<String>,
}

impl<S: SecretStore> BundledStore<S> {
    /// Key of the bundle entry.
    pub const BUNDLE_KEY: &'static str = "dbiji:secrets";

    /// Bundle the secrets of `inner`.
    pub fn new(inner: S) -> BundledStore<S> {
        BundledStore {
            inner,
            state: Mutex::new(BundleState::default()),
        }
    }

    /// The underlying store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    fn load(&self, state: &mut BundleState) -> DbResult<()> {
        if state.secrets.is_some() {
            return Ok(());
        }
        let mut map = BTreeMap::new();
        if let Some(blob) = self.inner.get(Self::BUNDLE_KEY)? {
            let parsed: BTreeMap<String, String> = serde_json::from_str(blob.expose())
                .map_err(|e| DbError::config(format!("The stored secrets cannot be read: {e}")))?;
            for (k, v) in parsed {
                map.insert(k, Secret::new(v));
            }
        }
        state.secrets = Some(map);
        Ok(())
    }

    fn save(&self, map: &BTreeMap<String, Secret>) -> DbResult<()> {
        let plain: BTreeMap<&str, &str> =
            map.iter().map(|(k, v)| (k.as_str(), v.expose())).collect();
        let json = Zeroizing::new(
            serde_json::to_string(&plain).map_err(|e| DbError::internal(e.to_string()))?,
        );
        self.inner
            .set(Self::BUNDLE_KEY, &Secret::new(json.as_str()))
    }
}

impl<S: SecretStore> SecretStore for BundledStore<S> {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        let mut state = self.state.lock();
        self.load(&mut state)?;
        if let Some(v) = state.secrets.as_ref().and_then(|m| m.get(key)) {
            return Ok(Some(v.clone()));
        }
        if state.no_legacy.contains(key) {
            return Ok(None);
        }
        // An entry of an earlier version: move it into the bundle.
        match self.inner.get(key)? {
            Some(v) => {
                let map = state.secrets.get_or_insert_with(BTreeMap::new);
                map.insert(key.to_string(), v.clone());
                self.save(map)?;
                let _ = self.inner.delete(key);
                Ok(Some(v))
            }
            None => {
                state.no_legacy.insert(key.to_string());
                Ok(None)
            }
        }
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        let mut state = self.state.lock();
        self.load(&mut state)?;
        let map = state.secrets.get_or_insert_with(BTreeMap::new);
        map.insert(key.to_string(), value.clone());
        self.save(map)?;
        state.no_legacy.insert(key.to_string());
        let _ = self.inner.delete(key);
        Ok(())
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        let mut state = self.state.lock();
        self.load(&mut state)?;
        let map = state.secrets.get_or_insert_with(BTreeMap::new);
        if map.remove(key).is_some() {
            self.save(map)?;
        }
        state.no_legacy.insert(key.to_string());
        self.inner.delete(key)
    }
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }
    fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
    fn list_keys(&self) -> DbResult<Vec<String>> {
        let mut state = self.state.lock();
        self.load(&mut state)?;
        Ok(state
            .secrets
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }
}

// ------------------------------------------------------------------- vault

const VAULT_MAGIC: &[u8; 8] = b"DBIJIVLT";
const VAULT_VERSION: u8 = 1;
const KDF_ARGON2ID: u8 = 1;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24;
/// Upper bounds accepted when reading a vault, protecting against files
/// that would make us allocate absurd amounts of memory.
const MAX_M_COST_KIB: u32 = 4 * 1024 * 1024;
const MAX_T_COST: u32 = 64;
const MAX_P_COST: u32 = 64;

/// Argon2id cost parameters, stored in the vault header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_cost_kib: u32,
    /// Iterations.
    pub t_cost: u32,
    /// Lanes.
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// Production defaults: 64 MiB, 3 iterations, 1 lane.
    fn default() -> Self {
        KdfParams {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

impl KdfParams {
    /// Explicit parameters (validated when used).
    pub const fn new(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> KdfParams {
        KdfParams {
            m_cost_kib,
            t_cost,
            p_cost,
        }
    }

    fn validate(&self) -> DbResult<()> {
        if self.p_cost == 0
            || self.p_cost > MAX_P_COST
            || self.t_cost == 0
            || self.t_cost > MAX_T_COST
        {
            return Err(DbError::config(
                "Invalid key derivation parameters in the vault",
            ));
        }
        if self.m_cost_kib < 8 * self.p_cost || self.m_cost_kib > MAX_M_COST_KIB {
            return Err(DbError::config(
                "Invalid key derivation memory parameter in the vault",
            ));
        }
        Ok(())
    }
}

fn random_bytes(buf: &mut [u8]) -> DbResult<()> {
    SysRng.try_fill_bytes(buf).map_err(|e| {
        DbError::internal(format!(
            "The operating system random number generator failed: {e}"
        ))
    })
}

fn derive_key(master: &Secret, salt: &[u8], params: KdfParams) -> DbResult<Zeroizing<[u8; 32]>> {
    params.validate()?;
    let p = argon2::Params::new(params.m_cost_kib, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| DbError::config(format!("Invalid key derivation parameters: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, p);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(master.expose().as_bytes(), salt, &mut key[..])
        .map_err(|e| DbError::internal(format!("Key derivation failed: {e}")))?;
    Ok(key)
}

struct VaultHeader {
    params: KdfParams,
    salt: Vec<u8>,
    nonce: [u8; NONCE_LEN],
}

impl VaultHeader {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.salt.len() + NONCE_LEN);
        out.extend_from_slice(VAULT_MAGIC);
        out.push(VAULT_VERSION);
        out.push(KDF_ARGON2ID);
        out.extend_from_slice(&self.params.m_cost_kib.to_le_bytes());
        out.extend_from_slice(&self.params.t_cost.to_le_bytes());
        out.extend_from_slice(&self.params.p_cost.to_le_bytes());
        out.push(self.salt.len() as u8);
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&self.nonce);
        out
    }

    /// Parses the header; returns it with its encoded length.
    fn decode(bytes: &[u8]) -> DbResult<(VaultHeader, usize)> {
        let bad = || DbError::config("The vault file is not a DBiji vault or is damaged");
        if bytes.len() < 23 || &bytes[..8] != VAULT_MAGIC {
            return Err(bad());
        }
        if bytes[8] != VAULT_VERSION {
            return Err(DbError::config(format!(
                "The vault file has version {} which this DBiji version cannot read",
                bytes[8]
            )));
        }
        if bytes[9] != KDF_ARGON2ID {
            return Err(bad());
        }
        let u32_at = |i: usize| -> DbResult<u32> {
            let b: [u8; 4] = bytes
                .get(i..i + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(bad)?;
            Ok(u32::from_le_bytes(b))
        };
        let params = KdfParams {
            m_cost_kib: u32_at(10)?,
            t_cost: u32_at(14)?,
            p_cost: u32_at(18)?,
        };
        let salt_len = usize::from(bytes[22]);
        if !(16..=64).contains(&salt_len) {
            return Err(bad());
        }
        let salt = bytes.get(23..23 + salt_len).ok_or_else(bad)?.to_vec();
        let nonce_start = 23 + salt_len;
        let nonce: [u8; NONCE_LEN] = bytes
            .get(nonce_start..nonce_start + NONCE_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(bad)?;
        Ok((
            VaultHeader {
                params,
                salt,
                nonce,
            },
            nonce_start + NONCE_LEN,
        ))
    }
}

/// Serializes the entries without intermediate copies of the secret values.
struct ExposedEntries<'a>(&'a BTreeMap<String, Secret>);

impl serde::Serialize for ExposedEntries<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v.expose())?;
        }
        m.end()
    }
}

fn seal(
    key: &[u8; 32],
    params: KdfParams,
    salt: &[u8],
    entries: &BTreeMap<String, Secret>,
) -> DbResult<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce)?;
    let header = VaultHeader {
        params,
        salt: salt.to_vec(),
        nonce,
    }
    .encode();
    let capacity = 2 + entries
        .iter()
        .map(|(k, v)| k.len() + v.expose().len() * 2 + 8)
        .sum::<usize>();
    let mut plaintext = Zeroizing::new(Vec::with_capacity(capacity));
    serde_json::to_writer(&mut *plaintext, &ExposedEntries(entries))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| DbError::internal("Invalid vault key length"))?;
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: &header,
            },
        )
        .map_err(|_| DbError::internal("Encrypting the vault failed"))?;
    let mut out = header;
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

struct Opened {
    key: Zeroizing<[u8; 32]>,
    params: KdfParams,
    salt: Vec<u8>,
    entries: BTreeMap<String, Secret>,
}

fn open_vault(bytes: &[u8], master: &Secret) -> DbResult<Opened> {
    let (header, header_len) = VaultHeader::decode(bytes)?;
    let key = derive_key(master, &header.salt, header.params)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key[..])
        .map_err(|_| DbError::internal("Invalid vault key length"))?;
    let aad = &bytes[..header_len];
    let ciphertext = &bytes[header_len..];
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &XNonce::from(header.nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| DbError::auth("Wrong master password (or the vault file was modified)"))?,
    );
    let raw: BTreeMap<String, String> = serde_json::from_slice(&plaintext)
        .map_err(|_| DbError::config("The vault content is damaged"))?;
    let entries = raw.into_iter().map(|(k, v)| (k, Secret::from(v))).collect();
    Ok(Opened {
        key,
        params: header.params,
        salt: header.salt,
        entries,
    })
}

/// Encrypted file store protected by a master password.
///
/// File format (`vault.bin`): `"DBIJIVLT"`, version byte, KDF id (1 =
/// Argon2id v0x13), m/t/p costs (u32 LE), salt length + salt, 24 byte nonce,
/// then the XChaCha20-Poly1305 ciphertext of a JSON object `{key: secret}`.
/// The header is authenticated as associated data; every write uses a fresh
/// random nonce. The derived key is wiped when the vault is locked or dropped.
pub struct VaultStore {
    path: PathBuf,
    state: Mutex<Option<Opened>>,
    last_used: Mutex<Instant>,
}

impl std::fmt::Debug for VaultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultStore")
            .field("path", &self.path)
            .field("locked", &self.is_locked())
            .finish()
    }
}

impl VaultStore {
    /// A locked vault backed by `path` (the file may not exist yet).
    pub fn new(path: impl Into<PathBuf>) -> VaultStore {
        VaultStore {
            path: path.into(),
            state: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        }
    }

    /// Path of the vault file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the vault file exists.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Creates a new empty vault with the default (strong) KDF parameters and unlocks it.
    pub fn create(&self, master: &Secret) -> DbResult<()> {
        self.create_with_params(master, KdfParams::default())
    }

    /// Creates a new empty vault with explicit KDF parameters and unlocks it.
    pub fn create_with_params(&self, master: &Secret, params: KdfParams) -> DbResult<()> {
        if self.exists() {
            return Err(DbError::config(format!(
                "A vault already exists at {}",
                self.path.display()
            )));
        }
        if master.is_empty() {
            return Err(DbError::config("The master password must not be empty"));
        }
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        let key = derive_key(master, &salt, params)?;
        let entries = BTreeMap::new();
        let bytes = seal(&key, params, &salt, &entries)?;
        write_secure(&self.path, &bytes)?;
        *self.state.lock() = Some(Opened {
            key,
            params,
            salt,
            entries,
        });
        self.touch();
        Ok(())
    }

    /// Decrypts the vault. A wrong password is an [`ErrorKind::Auth`] error.
    pub fn unlock(&self, master: &Secret) -> DbResult<()> {
        let bytes = self.read_file()?;
        let opened = open_vault(&bytes, master)?;
        *self.state.lock() = Some(opened);
        self.touch();
        Ok(())
    }

    /// Forgets the key and the decrypted secrets (wiped from memory).
    pub fn lock(&self) {
        self.state.lock().take();
    }

    /// Locks the vault when it was not used for `idle`; returns true when it locked it.
    pub fn lock_if_idle(&self, idle: Duration) -> bool {
        if self.is_locked() || idle.is_zero() {
            return false;
        }
        if self.last_used.lock().elapsed() >= idle {
            self.lock();
            return true;
        }
        false
    }

    /// Re-encrypts the vault under a new master password (new salt, default
    /// KDF parameters). The old password is verified against the file.
    pub fn change_master_password(&self, old: &Secret, new: &Secret) -> DbResult<()> {
        self.change_master_password_with_params(old, new, KdfParams::default())
    }

    /// Like [`VaultStore::change_master_password`] with explicit KDF parameters.
    pub fn change_master_password_with_params(
        &self,
        old: &Secret,
        new: &Secret,
        params: KdfParams,
    ) -> DbResult<()> {
        if new.is_empty() {
            return Err(DbError::config("The master password must not be empty"));
        }
        let bytes = self.read_file()?;
        let opened = open_vault(&bytes, old)?;
        let mut salt = vec![0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        let key = derive_key(new, &salt, params)?;
        let out = seal(&key, params, &salt, &opened.entries)?;
        write_secure(&self.path, &out)?;
        *self.state.lock() = Some(Opened {
            key,
            params,
            salt,
            entries: opened.entries,
        });
        self.touch();
        Ok(())
    }

    /// KDF parameters of the unlocked vault.
    pub fn params(&self) -> Option<KdfParams> {
        self.state.lock().as_ref().map(|o| o.params)
    }

    fn read_file(&self) -> DbResult<Vec<u8>> {
        std::fs::read(&self.path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DbError::config("No secret vault has been created yet")
            } else {
                DbError::io(format!("Cannot read {}: {e}", self.path.display()))
            }
        })
    }

    fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    fn with_open<T>(&self, f: impl FnOnce(&mut Opened) -> DbResult<T>) -> DbResult<T> {
        let mut guard = self.state.lock();
        let opened = guard
            .as_mut()
            .ok_or_else(|| DbError::auth("The secret vault is locked"))?;
        let r = f(opened);
        drop(guard);
        self.touch();
        r
    }

    fn persist(&self, opened: &Opened) -> DbResult<()> {
        let bytes = seal(&opened.key, opened.params, &opened.salt, &opened.entries)?;
        write_secure(&self.path, &bytes)
    }
}

impl SecretStore for VaultStore {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        self.with_open(|o| Ok(o.entries.get(key).cloned()))
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.with_open(|o| {
            let previous = o.entries.insert(key.to_string(), value.clone());
            if let Err(e) = self.persist(o) {
                match previous {
                    Some(p) => o.entries.insert(key.to_string(), p),
                    None => o.entries.remove(key),
                };
                return Err(e);
            }
            Ok(())
        })
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        self.with_open(|o| match o.entries.remove(key) {
            None => Ok(()),
            Some(previous) => {
                if let Err(e) = self.persist(o) {
                    o.entries.insert(key.to_string(), previous);
                    return Err(e);
                }
                Ok(())
            }
        })
    }
    fn backend_name(&self) -> &'static str {
        "vault"
    }
    fn is_locked(&self) -> bool {
        self.state.lock().is_none()
    }
    fn list_keys(&self) -> DbResult<Vec<String>> {
        self.with_open(|o| Ok(o.entries.keys().cloned().collect()))
    }
}

// ----------------------------------------------------------------- manager

/// Outcome of copying secrets between stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Keys copied to the destination.
    pub migrated: Vec<String>,
    /// Keys absent from the source.
    pub missing: Vec<String>,
    /// Keys that failed, with the reason.
    pub failed: Vec<(String, String)>,
}

/// Copies `keys` from `from` to `to`. Secrets are deleted from the source
/// only when `delete_from_source` is set and the copy succeeded.
pub fn migrate_secrets(
    from: &dyn SecretStore,
    to: &dyn SecretStore,
    keys: &[String],
    delete_from_source: bool,
) -> DbResult<MigrationReport> {
    if from.is_locked() {
        return Err(DbError::auth(format!(
            "The {} secret store is locked",
            from.backend_name()
        )));
    }
    if to.is_locked() {
        return Err(DbError::auth(format!(
            "The {} secret store is locked",
            to.backend_name()
        )));
    }
    let mut report = MigrationReport::default();
    for key in keys {
        let value = match from.get(key) {
            Ok(Some(v)) => v,
            Ok(None) => {
                report.missing.push(key.clone());
                continue;
            }
            Err(e) => {
                report.failed.push((key.clone(), e.message));
                continue;
            }
        };
        if let Err(e) = to.set(key, &value) {
            report.failed.push((key.clone(), e.message));
            continue;
        }
        if delete_from_source && let Err(e) = from.delete(key) {
            report.failed.push((
                key.clone(),
                format!("copied, but not removed from the old store: {}", e.message),
            ));
            continue;
        }
        report.migrated.push(key.clone());
    }
    Ok(report)
}

/// Facade used by the application: the active backend plus helpers for
/// connection and API key secrets.
pub struct SecretsManager {
    backend: SecretBackend,
    store: Arc<dyn SecretStore>,
    vault: Option<Arc<VaultStore>>,
}

impl std::fmt::Debug for SecretsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsManager")
            .field("backend", &self.backend)
            .field("store", &self.store.backend_name())
            .finish()
    }
}

impl SecretsManager {
    /// Backend chosen in the settings: the OS keychain or `vault.bin`
    /// (initially locked; see [`SecretsManager::vault`]).
    pub fn new(paths: &AppPaths, security: &SecuritySettings) -> SecretsManager {
        match security.secret_backend {
            SecretBackend::Keychain => SecretsManager {
                backend: SecretBackend::Keychain,
                store: Arc::new(BundledStore::new(KeychainStore::new())),
                vault: None,
            },
            SecretBackend::EncryptedFile => {
                let vault = Arc::new(VaultStore::new(paths.vault_file()));
                SecretsManager {
                    backend: SecretBackend::EncryptedFile,
                    store: vault.clone(),
                    vault: Some(vault),
                }
            }
        }
    }

    /// Uses an explicit store (tests, embedding).
    pub fn with_store(backend: SecretBackend, store: Arc<dyn SecretStore>) -> SecretsManager {
        SecretsManager {
            backend,
            store,
            vault: None,
        }
    }

    /// Uses a vault store.
    pub fn with_vault(vault: Arc<VaultStore>) -> SecretsManager {
        SecretsManager {
            backend: SecretBackend::EncryptedFile,
            store: vault.clone(),
            vault: Some(vault),
        }
    }

    /// The configured backend.
    pub fn backend(&self) -> SecretBackend {
        self.backend
    }

    /// The active store.
    pub fn store(&self) -> Arc<dyn SecretStore> {
        self.store.clone()
    }

    /// The vault, when the encrypted file backend is active.
    pub fn vault(&self) -> Option<&Arc<VaultStore>> {
        self.vault.as_ref()
    }

    /// True when the active store must be unlocked first.
    pub fn is_locked(&self) -> bool {
        self.store.is_locked()
    }

    /// Reads a secret from the active store.
    pub fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        self.store.get(key)
    }

    /// Writes a secret to the active store.
    pub fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.store.set(key, value)
    }

    /// Deletes a secret from the active store.
    pub fn delete(&self, key: &str) -> DbResult<()> {
        self.store.delete(key)
    }

    /// Secret of a connection slot.
    pub fn connection_secret(
        &self,
        connection_id: &str,
        slot: SecretSlot,
    ) -> DbResult<Option<Secret>> {
        self.store.get(&secret_key(connection_id, slot))
    }

    /// Stores (`Some`) or deletes (`None` / empty) a connection secret.
    pub fn set_connection_secret(
        &self,
        connection_id: &str,
        slot: SecretSlot,
        value: Option<&Secret>,
    ) -> DbResult<()> {
        let key = secret_key(connection_id, slot);
        match value {
            Some(v) if !v.is_empty() => self.store.set(&key, v),
            _ => self.store.delete(&key),
        }
    }

    /// Deletes every secret of a connection.
    pub fn delete_connection_secrets(&self, connection_id: &str) -> DbResult<()> {
        for key in crate::connections::ConnectionStore::secret_keys_for(connection_id) {
            self.store.delete(&key)?;
        }
        Ok(())
    }

    /// Copies the secrets of one connection to another (duplicate). Returns the number copied.
    pub fn copy_connection_secrets(&self, from_id: &str, to_id: &str) -> DbResult<usize> {
        let mut n = 0;
        for (from, to) in crate::connections::ConnectionStore::secret_key_pairs(from_id, to_id) {
            if let Some(v) = self.store.get(&from)? {
                self.store.set(&to, &v)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// API key of an AI provider ([`crate::settings::AiProvider::id`]).
    pub fn ai_api_key(&self, provider: &str) -> DbResult<Option<Secret>> {
        self.store.get(&ai_key(provider))
    }

    /// Stores or deletes an AI provider API key.
    pub fn set_ai_api_key(&self, provider: &str, value: Option<&Secret>) -> DbResult<()> {
        match value {
            Some(v) if !v.is_empty() => self.store.set(&ai_key(provider), v),
            _ => self.store.delete(&ai_key(provider)),
        }
    }

    /// Stores secrets returned by an importer. Returns the number stored.
    pub fn store_all(&self, secrets: &[(String, Secret)]) -> DbResult<usize> {
        for (k, v) in secrets {
            self.store.set(k, v)?;
        }
        Ok(secrets.len())
    }

    /// Moves `keys` to a new backend. The switch only happens when every
    /// present secret was copied; then (optionally) the old copies are deleted.
    pub fn switch_backend(
        &mut self,
        backend: SecretBackend,
        store: Arc<dyn SecretStore>,
        keys: &[String],
        delete_from_old: bool,
    ) -> DbResult<MigrationReport> {
        let mut report = migrate_secrets(self.store.as_ref(), store.as_ref(), keys, false)?;
        if !report.failed.is_empty() {
            let (k, why) = &report.failed[0];
            return Err(DbError::new(
                ErrorKind::Io,
                format!(
                    "{} secret(s) could not be copied (first: {k}: {why}); the backend was not changed",
                    report.failed.len()
                ),
            ));
        }
        let old = std::mem::replace(&mut self.store, store);
        self.backend = backend;
        self.vault = None;
        if delete_from_old {
            for key in &report.migrated {
                if let Err(e) = old.delete(key) {
                    report.failed.push((
                        key.clone(),
                        format!("not removed from the old store: {}", e.message),
                    ));
                }
            }
        }
        Ok(report)
    }

    /// Switches to a vault store (see [`SecretsManager::switch_backend`]).
    pub fn switch_to_vault(
        &mut self,
        vault: Arc<VaultStore>,
        keys: &[String],
        delete_from_old: bool,
    ) -> DbResult<MigrationReport> {
        let report = self.switch_backend(
            SecretBackend::EncryptedFile,
            vault.clone(),
            keys,
            delete_from_old,
        )?;
        self.vault = Some(vault);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small parameters keep unit tests fast; production defaults are much stronger.
    const TEST_PARAMS: KdfParams = KdfParams::new(64, 1, 1);

    fn vault() -> (tempfile::TempDir, VaultStore) {
        let dir = tempfile::tempdir().unwrap();
        let v = VaultStore::new(dir.path().join("data").join("vault.bin"));
        (dir, v)
    }

    #[test]
    fn memory_store_basics() {
        let m = MemoryStore::new();
        assert!(m.get("k").unwrap().is_none());
        m.set("k", &Secret::new("v")).unwrap();
        assert_eq!(m.get("k").unwrap().unwrap().expose(), "v");
        assert_eq!(m.list_keys().unwrap(), vec!["k".to_string()]);
        m.delete("k").unwrap();
        m.delete("k").unwrap();
        assert!(m.is_empty());
        m.set_locked(true);
        assert_eq!(m.get("k").unwrap_err().kind, ErrorKind::Auth);
        assert!(!format!("{m:?}").contains('v'));
    }

    #[test]
    fn vault_lifecycle() {
        let (_d, v) = vault();
        assert!(!v.exists());
        assert!(v.is_locked());
        assert_eq!(
            v.unlock(&Secret::new("x")).unwrap_err().kind,
            ErrorKind::Config
        );
        assert_eq!(
            v.create_with_params(&Secret::new(""), TEST_PARAMS)
                .unwrap_err()
                .kind,
            ErrorKind::Config
        );
        v.create_with_params(&Secret::new("master pw"), TEST_PARAMS)
            .unwrap();
        assert!(!v.is_locked());
        assert!(
            v.create_with_params(&Secret::new("again"), TEST_PARAMS)
                .is_err(),
            "no overwrite"
        );
        v.set("conn:1:password", &Secret::new("db-secret-123"))
            .unwrap();
        v.set("ai:anthropic:api-key", &Secret::new("sk-test"))
            .unwrap();
        assert_eq!(
            v.get("conn:1:password").unwrap().unwrap().expose(),
            "db-secret-123"
        );

        // On disk: magic header, no plaintext, mode 0600.
        let raw = std::fs::read(v.path()).unwrap();
        assert_eq!(&raw[..8], b"DBIJIVLT");
        assert!(!raw.windows(13).any(|w| w == b"db-secret-123"));
        assert!(
            !raw.windows(15).any(|w| w == b"conn:1:password"),
            "keys are encrypted too"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(v.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        v.lock();
        assert!(v.is_locked());
        assert_eq!(v.get("conn:1:password").unwrap_err().kind, ErrorKind::Auth);
        assert_eq!(
            v.set("x", &Secret::new("y")).unwrap_err().kind,
            ErrorKind::Auth
        );
        let err = v.unlock(&Secret::new("wrong")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert!(v.is_locked());

        v.unlock(&Secret::new("master pw")).unwrap();
        assert_eq!(
            v.get("ai:anthropic:api-key").unwrap().unwrap().expose(),
            "sk-test"
        );
        let mut keys = v.list_keys().unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "ai:anthropic:api-key".to_string(),
                "conn:1:password".to_string()
            ]
        );
        v.delete("ai:anthropic:api-key").unwrap();
        v.delete("missing").unwrap();
        assert_eq!(v.params(), Some(TEST_PARAMS));

        // A fresh instance reads the same file.
        let other = VaultStore::new(v.path());
        other.unlock(&Secret::new("master pw")).unwrap();
        assert!(other.get("ai:anthropic:api-key").unwrap().is_none());
        assert_eq!(
            other.get("conn:1:password").unwrap().unwrap().expose(),
            "db-secret-123"
        );
    }

    #[test]
    fn nonce_changes_on_every_write() {
        let (_d, v) = vault();
        v.create_with_params(&Secret::new("m"), TEST_PARAMS)
            .unwrap();
        v.set("a", &Secret::new("1")).unwrap();
        let first = std::fs::read(v.path()).unwrap();
        v.set("a", &Secret::new("1")).unwrap();
        let second = std::fs::read(v.path()).unwrap();
        assert_ne!(first, second);
        let (h1, l1) = VaultHeader::decode(&first).unwrap();
        let (h2, _) = VaultHeader::decode(&second).unwrap();
        assert_ne!(h1.nonce, h2.nonce);
        assert_eq!(h1.salt, h2.salt);
        assert_eq!(h1.params, TEST_PARAMS);
        assert!(first.len() > l1 + 16);
    }

    #[test]
    fn tampering_is_detected() {
        let (_d, v) = vault();
        v.create_with_params(&Secret::new("m"), TEST_PARAMS)
            .unwrap();
        v.set("a", &Secret::new("1")).unwrap();
        let mut raw = std::fs::read(v.path()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        std::fs::write(v.path(), &raw).unwrap();
        v.lock();
        assert_eq!(
            v.unlock(&Secret::new("m")).unwrap_err().kind,
            ErrorKind::Auth
        );
        // Header tampering (KDF cost) is rejected too: it is authenticated.
        let mut raw2 = raw.clone();
        raw2[last] ^= 0x01;
        raw2[14] = 2; // t_cost
        std::fs::write(v.path(), &raw2).unwrap();
        assert_eq!(
            v.unlock(&Secret::new("m")).unwrap_err().kind,
            ErrorKind::Auth
        );
        std::fs::write(v.path(), b"garbage").unwrap();
        assert_eq!(
            v.unlock(&Secret::new("m")).unwrap_err().kind,
            ErrorKind::Config
        );
        // Absurd KDF parameters are refused before allocating.
        let mut raw3 = raw.clone();
        raw3[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(v.path(), &raw3).unwrap();
        assert_eq!(
            v.unlock(&Secret::new("m")).unwrap_err().kind,
            ErrorKind::Config
        );
    }

    #[test]
    fn change_master_password() {
        let (_d, v) = vault();
        v.create_with_params(&Secret::new("old"), TEST_PARAMS)
            .unwrap();
        v.set("k", &Secret::new("value")).unwrap();
        let before = VaultHeader::decode(&std::fs::read(v.path()).unwrap())
            .unwrap()
            .0
            .salt;
        assert_eq!(
            v.change_master_password_with_params(
                &Secret::new("bad"),
                &Secret::new("new"),
                TEST_PARAMS
            )
            .unwrap_err()
            .kind,
            ErrorKind::Auth
        );
        v.change_master_password_with_params(&Secret::new("old"), &Secret::new("new"), TEST_PARAMS)
            .unwrap();
        let after = VaultHeader::decode(&std::fs::read(v.path()).unwrap())
            .unwrap()
            .0
            .salt;
        assert_ne!(before, after, "new salt");
        v.lock();
        assert!(v.unlock(&Secret::new("old")).is_err());
        v.unlock(&Secret::new("new")).unwrap();
        assert_eq!(v.get("k").unwrap().unwrap().expose(), "value");
    }

    #[test]
    fn idle_lock() {
        let (_d, v) = vault();
        v.create_with_params(&Secret::new("m"), TEST_PARAMS)
            .unwrap();
        assert!(!v.lock_if_idle(Duration::from_secs(3600)));
        assert!(!v.lock_if_idle(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(30));
        assert!(v.lock_if_idle(Duration::from_millis(10)));
        assert!(v.is_locked());
    }

    #[test]
    fn migration_between_backends() {
        let mem = Arc::new(MemoryStore::new());
        mem.set("conn:a:password", &Secret::new("pa")).unwrap();
        mem.set("conn:b:password", &Secret::new("pb")).unwrap();
        let mut mgr = SecretsManager::with_store(SecretBackend::Keychain, mem.clone());
        let (_d, v) = vault();
        let v = Arc::new(v);
        v.create_with_params(&Secret::new("m"), TEST_PARAMS)
            .unwrap();
        let keys = vec![
            "conn:a:password".to_string(),
            "conn:b:password".to_string(),
            "conn:c:password".to_string(),
        ];
        let report = mgr.switch_to_vault(v.clone(), &keys, true).unwrap();
        assert_eq!(report.migrated.len(), 2);
        assert_eq!(report.missing, vec!["conn:c:password".to_string()]);
        assert!(report.failed.is_empty());
        assert_eq!(mgr.backend(), SecretBackend::EncryptedFile);
        assert!(mgr.vault().is_some());
        assert_eq!(mgr.get("conn:a:password").unwrap().unwrap().expose(), "pa");
        assert!(mem.is_empty(), "old copies deleted");

        // Destination locked: nothing changes.
        let other = Arc::new(MemoryStore::new());
        other.set_locked(true);
        assert!(
            mgr.switch_backend(SecretBackend::Keychain, other, &keys, true)
                .is_err()
        );
        assert_eq!(mgr.backend(), SecretBackend::EncryptedFile);
    }

    #[test]
    fn manager_helpers() {
        let mem = Arc::new(MemoryStore::new());
        let mgr = SecretsManager::with_store(SecretBackend::Keychain, mem.clone());
        mgr.set_connection_secret("c1", SecretSlot::Password, Some(&Secret::new("p")))
            .unwrap();
        mgr.set_connection_secret("c1", SecretSlot::SshPassphrase, Some(&Secret::new("k")))
            .unwrap();
        assert_eq!(
            mgr.connection_secret("c1", SecretSlot::Password)
                .unwrap()
                .unwrap()
                .expose(),
            "p"
        );
        assert_eq!(mgr.copy_connection_secrets("c1", "c2").unwrap(), 2);
        assert_eq!(
            mgr.connection_secret("c2", SecretSlot::SshPassphrase)
                .unwrap()
                .unwrap()
                .expose(),
            "k"
        );
        mgr.set_connection_secret("c1", SecretSlot::Password, None)
            .unwrap();
        assert!(
            mgr.connection_secret("c1", SecretSlot::Password)
                .unwrap()
                .is_none()
        );
        mgr.delete_connection_secrets("c2").unwrap();
        assert!(
            mgr.connection_secret("c2", SecretSlot::SshPassphrase)
                .unwrap()
                .is_none()
        );
        mgr.set_ai_api_key("anthropic", Some(&Secret::new("sk")))
            .unwrap();
        assert!(mem.get("ai:anthropic:api-key").unwrap().is_some());
        assert_eq!(mgr.store_all(&[("x".into(), Secret::new("1"))]).unwrap(), 1);
        assert!(!format!("{mgr:?}").contains("sk"));
    }

    #[test]
    fn manager_from_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        let vault_mgr = SecretsManager::new(
            &paths,
            &SecuritySettings {
                secret_backend: SecretBackend::EncryptedFile,
                ..Default::default()
            },
        );
        assert_eq!(vault_mgr.store().backend_name(), "vault");
        assert!(vault_mgr.is_locked());
        assert_eq!(vault_mgr.vault().unwrap().path(), paths.vault_file());
        let kc = SecretsManager::new(&paths, &SecuritySettings::default());
        assert_eq!(kc.store().backend_name(), "keychain");
        assert!(kc.vault().is_none());
    }

    /// Counts the reads of the store below (one bundle read per process).
    struct Counting(MemoryStore, std::sync::atomic::AtomicUsize);

    impl SecretStore for Counting {
        fn get(&self, key: &str) -> DbResult<Option<Secret>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.get(key)
        }
        fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
            self.0.set(key, value)
        }
        fn delete(&self, key: &str) -> DbResult<()> {
            self.0.delete(key)
        }
        fn backend_name(&self) -> &'static str {
            "counting"
        }
        fn is_locked(&self) -> bool {
            false
        }
    }

    #[test]
    fn bundled_store_one_entry_read_once_and_migration() {
        let inner = Counting(MemoryStore::new(), Default::default());
        // An entry of an earlier version, one per key.
        inner
            .0
            .set("conn:a:password", &Secret::new("old-a"))
            .unwrap();
        let store = BundledStore::new(inner);
        assert_eq!(
            store.get("conn:a:password").unwrap().unwrap().expose(),
            "old-a"
        );
        // Moved into the bundle; the old entry is gone.
        assert!(store.inner().0.get("conn:a:password").unwrap().is_none());
        store.set("conn:b:password", &Secret::new("pw-b")).unwrap();
        store
            .set("ai:anthropic:api-key", &Secret::new("k\"ey"))
            .unwrap();
        let reads = store.inner().1.load(Ordering::SeqCst);
        for _ in 0..20 {
            assert_eq!(
                store.get("conn:b:password").unwrap().unwrap().expose(),
                "pw-b"
            );
            assert!(store.get("conn:missing:password").unwrap().is_none());
        }
        assert!(
            store.inner().1.load(Ordering::SeqCst) <= reads + 1,
            "cached: the store below is not read again"
        );
        assert_eq!(
            store.inner().0.list_keys().unwrap(),
            vec![BundledStore::<MemoryStore>::BUNDLE_KEY.to_string()]
        );
        // A new process reads the single bundle entry.
        let again = BundledStore::new(MemoryStore::new());
        again
            .inner()
            .set(
                BundledStore::<MemoryStore>::BUNDLE_KEY,
                &store
                    .inner()
                    .0
                    .get(BundledStore::<MemoryStore>::BUNDLE_KEY)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            again.get("ai:anthropic:api-key").unwrap().unwrap().expose(),
            "k\"ey"
        );
        assert_eq!(
            again.get("conn:a:password").unwrap().unwrap().expose(),
            "old-a"
        );
        again.delete("conn:a:password").unwrap();
        assert!(again.get("conn:a:password").unwrap().is_none());
        let mut keys = again.list_keys().unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "ai:anthropic:api-key".to_string(),
                "conn:b:password".to_string()
            ]
        );
    }

    /// Touches the real OS keychain; run manually with `--ignored`.
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let store = KeychainStore::with_service("dev.dbiji.DBiji.test");
        let key = format!("test:{}", uuid::Uuid::new_v4());
        store
            .set(&key, &Secret::new("keychain-test-value"))
            .unwrap();
        assert_eq!(
            store.get(&key).unwrap().unwrap().expose(),
            "keychain-test-value"
        );
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.delete(&key).unwrap();
    }
}
