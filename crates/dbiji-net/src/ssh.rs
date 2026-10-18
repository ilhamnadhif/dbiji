//! SSH client sessions built on russh: key exchange with host key
//! verification, authentication (password, keyboard-interactive with the
//! password, private key files, ssh-agent) and `direct-tcpip` channels.

use crate::known_hosts::{HostKeyStatus, KnownHosts, fingerprint};
use crate::stream::{BoxedStream, display_addr};
use dbiji_core::config::{HostKeyPolicy, SshAuth, SshConfig};
use dbiji_core::{DbError, DbResult, ErrorKind, Secret};
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{MethodKind, MethodSet, Preferred};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::Zeroizing;

/// Everything needed to open one SSH session.
#[derive(Debug, Clone)]
pub(crate) struct SshParams {
    pub cfg: SshConfig,
    pub password: Option<Secret>,
    pub key_passphrase: Option<Secret>,
    /// ssh-agent socket; `None` uses `SSH_AUTH_SOCK`.
    pub agent_socket: Option<PathBuf>,
}

impl SshParams {
    pub fn label(&self) -> String {
        format!(
            "{}@{}",
            self.cfg.user,
            display_addr(&self.cfg.host, self.cfg.port)
        )
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.cfg.connect_timeout_secs.max(1)))
    }
}

/// Error type of the russh handler: either a protocol error or our host key verdict.
#[derive(Debug)]
pub(crate) enum HandlerError {
    Russh(russh::Error),
    HostKey(DbError),
}

impl From<russh::Error> for HandlerError {
    fn from(e: russh::Error) -> Self {
        HandlerError::Russh(e)
    }
}

pub(crate) struct ClientHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    known_hosts: Arc<KnownHosts>,
    disconnect_reason: Arc<Mutex<Option<String>>>,
}

impl client::Handler for ClientHandler {
    type Error = HandlerError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        verify_host_key(&self.known_hosts, &self.host, self.port, &key, self.policy)
            .map_err(HandlerError::HostKey)?;
        Ok(true)
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let text = match &reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                if info.message.is_empty() {
                    format!("the server closed the connection ({:?})", info.reason_code)
                } else {
                    format!("the server closed the connection: {}", info.message)
                }
            }
            client::DisconnectReason::Error(HandlerError::Russh(e)) => describe_russh_error(e),
            client::DisconnectReason::Error(HandlerError::HostKey(e)) => e.message.clone(),
        };
        if let Ok(mut slot) = self.disconnect_reason.lock() {
            *slot = Some(text);
        }
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}

/// Applies the host key policy. On success with [`HostKeyPolicy::AcceptNew`]
/// an unknown key is recorded in the writable known_hosts file.
pub(crate) fn verify_host_key(
    known_hosts: &KnownHosts,
    host: &str,
    port: u16,
    key: &PublicKey,
    policy: HostKeyPolicy,
) -> DbResult<()> {
    let addr = display_addr(host, port);
    let fp = fingerprint(key);
    let alg = key.algorithm();
    if policy == HostKeyPolicy::AcceptAll {
        tracing::warn!(
            "accepting SSH host key of {addr} ({} {fp}) without verification",
            alg.as_str()
        );
        return Ok(());
    }
    let own = known_hosts.own_file().display().to_string();
    match known_hosts.check(host, port, key) {
        HostKeyStatus::Known { .. } => Ok(()),
        HostKeyStatus::Revoked { file, line } => Err(DbError::ssh(format!(
            "The SSH host key of {addr} ({} {fp}) is marked as revoked in {} line {line}",
            alg.as_str(),
            file.display()
        ))),
        HostKeyStatus::Changed { file, line } => Err(DbError::ssh(format!(
            "HOST KEY MISMATCH for {addr}: the server presented the {} key {fp}, but {} line {line} \
             records a different key for this host. Someone could be intercepting the connection \
             (man-in-the-middle), or the server key was replaced.",
            alg.as_str(),
            file.display()
        ))
        .with_hint(format!(
            "If the new key is expected, remove the old entry for this host from {} and connect again.",
            file.display()
        ))),
        HostKeyStatus::OtherTypesKnown { types, file, line } => Err(DbError::ssh(format!(
            "HOST KEY MISMATCH for {addr}: the server presented a {} key ({fp}) but {} line {line} \
             only records keys of type {} for this host.",
            alg.as_str(),
            file.display(),
            types.join(", ")
        ))
        .with_hint(format!(
            "If the server's keys changed, remove the old entries for this host from {} and connect again.",
            file.display()
        ))),
        HostKeyStatus::Unknown => match policy {
            HostKeyPolicy::Strict => Err(DbError::ssh(format!(
                "The SSH host {addr} is not known (presented {} key {fp}) and the host key policy is Strict",
            alg.as_str()
            ))
            .with_hint(format!(
                "Verify the fingerprint with the server administrator, then add the host to {own} or use the \"accept new\" policy."
            ))),
            _ => {
                if let Err(e) = known_hosts.learn(host, port, key) {
                    tracing::warn!("could not record the host key of {addr}: {e}");
                } else {
                    tracing::info!("trusting new SSH host {addr} ({} {fp}); recorded in {own}", alg.as_str());
                }
                Ok(())
            }
        },
    }
}

fn is_rsa(a: &Algorithm) -> bool {
    matches!(a, Algorithm::Rsa { .. })
}

/// Host key algorithms with the ones already known for this host first, so
/// the server proves the identity we can verify (prevents downgrades to an
/// unknown key type).
fn preferred_with_known_first(known: &[Algorithm]) -> Preferred {
    let mut pref = Preferred::default();
    if known.is_empty() {
        return pref;
    }
    let all: Vec<Algorithm> = pref.key.iter().cloned().collect();
    let is_known = |a: &Algorithm| known.iter().any(|k| k == a || (is_rsa(k) && is_rsa(a)));
    let mut ordered: Vec<Algorithm> = all.iter().filter(|a| is_known(a)).cloned().collect();
    ordered.extend(all.iter().filter(|a| !is_known(a)).cloned());
    pref.key = Cow::Owned(ordered);
    pref
}

/// An authenticated SSH session shared by all forwarded connections of a tunnel.
pub(crate) struct SshSession {
    handle: Handle<ClientHandler>,
    label: String,
    disconnect_reason: Arc<Mutex<Option<String>>>,
}

/// Why opening a channel failed.
pub(crate) enum ChannelError {
    /// The server refused the channel (e.g. the target is unreachable from it).
    Rejected(DbError),
    /// The session is gone; a new one may succeed.
    SessionLost(DbError),
}

impl SshSession {
    /// Performs the SSH handshake and authentication over `stream`.
    pub async fn connect(
        stream: BoxedStream,
        params: &SshParams,
        known_hosts: Arc<KnownHosts>,
    ) -> DbResult<SshSession> {
        let timeout = params.connect_timeout();
        let label = params.label();
        match tokio::time::timeout(timeout, Self::connect_inner(stream, params, known_hosts)).await
        {
            Ok(r) => r,
            Err(_) => Err(DbError::new(
                ErrorKind::Timeout,
                format!(
                    "Timed out after {}s establishing the SSH session to {label}",
                    timeout.as_secs()
                ),
            )),
        }
    }

    async fn connect_inner(
        stream: BoxedStream,
        params: &SshParams,
        known_hosts: Arc<KnownHosts>,
    ) -> DbResult<SshSession> {
        let cfg = &params.cfg;
        let label = params.label();
        let addr = display_addr(&cfg.host, cfg.port);
        let known_algs = known_hosts.known_algorithms(&cfg.host, cfg.port);
        let config = client::Config {
            keepalive_interval: (cfg.keepalive_secs > 0)
                .then(|| Duration::from_secs(u64::from(cfg.keepalive_secs))),
            keepalive_max: 3,
            inactivity_timeout: None,
            nodelay: true,
            preferred: preferred_with_known_first(&known_algs),
            ..Default::default()
        };
        let disconnect_reason = Arc::new(Mutex::new(None));
        let handler = ClientHandler {
            host: cfg.host.clone(),
            port: cfg.port,
            policy: cfg.host_key_policy,
            known_hosts,
            disconnect_reason: disconnect_reason.clone(),
        };
        let mut handle = match client::connect_stream(Arc::new(config), stream, handler).await {
            Ok(h) => h,
            Err(HandlerError::HostKey(e)) => return Err(e),
            Err(HandlerError::Russh(e)) => {
                return Err(DbError::ssh(format!(
                    "SSH handshake with {addr} failed: {}",
                    describe_russh_error(&e)
                )));
            }
        };
        authenticate(&mut handle, params).await?;
        tracing::info!("SSH session established: {label}");
        Ok(SshSession {
            handle,
            label,
            disconnect_reason,
        })
    }

    /// The connection is still up (best effort: a dead peer is noticed by keepalives or I/O errors).
    pub fn is_alive(&self) -> bool {
        !self.handle.is_closed()
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Why the session ended, when known.
    pub fn disconnect_reason(&self) -> Option<String> {
        self.disconnect_reason.lock().ok().and_then(|r| r.clone())
    }

    /// Opens a `direct-tcpip` channel to `host:port` as seen from the SSH server.
    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
        originator: std::net::SocketAddr,
    ) -> Result<BoxedStream, ChannelError> {
        let target = display_addr(host, port);
        let host_only = crate::stream::unbracket(host).to_string();
        match self
            .handle
            .channel_open_direct_tcpip(
                host_only,
                u32::from(port),
                originator.ip().to_string(),
                u32::from(originator.port()),
            )
            .await
        {
            Ok(channel) => Ok(Box::new(channel.into_stream())),
            Err(russh::Error::ChannelOpenFailure(reason)) => {
                Err(ChannelError::Rejected(DbError::connection(format!(
                    "The SSH server {} could not open a connection to {target}: {}",
                    self.label,
                    reason.description()
                ))))
            }
            Err(e) => {
                let why = self
                    .disconnect_reason()
                    .unwrap_or_else(|| describe_russh_error(&e));
                Err(ChannelError::SessionLost(DbError::ssh(format!(
                    "The SSH session {} is no longer usable: {why}",
                    self.label
                ))))
            }
        }
    }

    /// Round trip to the server (keepalive with reply). False when the
    /// session is gone or does not answer within `timeout`.
    pub async fn probe(&self, timeout: Duration) -> bool {
        if !self.is_alive() {
            return false;
        }
        matches!(
            tokio::time::timeout(timeout, self.handle.send_ping()).await,
            Ok(Ok(()))
        )
    }

    /// Sends a disconnect message (best effort).
    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "closed by DBiji", "en")
            .await;
    }
}

fn describe_russh_error(e: &russh::Error) -> String {
    match e {
        russh::Error::Version => {
            "the remote side is not an SSH server (invalid version string)".into()
        }
        russh::Error::Disconnect | russh::Error::HUP => {
            "the connection was closed by the remote side".into()
        }
        russh::Error::KeepaliveTimeout => "the server stopped answering keepalives".into(),
        russh::Error::InactivityTimeout => "the connection was idle for too long".into(),
        russh::Error::ConnectionTimeout => "the connection timed out".into(),
        russh::Error::IO(io) => format!("I/O error: {io}"),
        russh::Error::NoCommonAlgo { kind, ours, theirs } => {
            format!(
                "no common {kind:?} algorithm (ours: {}; server: {})",
                ours.join(", "),
                theirs.join(", ")
            )
        }
        other => other.to_string(),
    }
}

fn method_names(methods: &MethodSet) -> String {
    let names: Vec<&'static str> = methods
        .iter()
        .filter(|m| **m != MethodKind::None)
        .map(<&'static str>::from)
        .collect();
    if names.is_empty() {
        "none advertised".into()
    } else {
        names.join(", ")
    }
}

fn auth_failed(params: &SshParams, tried: &[String], remaining: Option<&MethodSet>) -> DbError {
    let mut msg = format!("SSH authentication failed for {}", params.label());
    if tried.is_empty() {
        msg.push_str(": no authentication method could be tried");
    } else {
        msg.push_str(&format!(". Tried: {}", tried.join("; ")));
    }
    if let Some(m) = remaining {
        msg.push_str(&format!(". The server accepts: {}", method_names(m)));
    }
    DbError::auth(msg)
}

fn send_failed(e: impl std::fmt::Display) -> DbError {
    DbError::ssh(format!("SSH connection lost during authentication: {e}"))
}

async fn authenticate(handle: &mut Handle<ClientHandler>, params: &SshParams) -> DbResult<()> {
    let user = params.cfg.user.trim();
    if user.is_empty() {
        return Err(DbError::config("The SSH user name is empty"));
    }
    let mut tried: Vec<String> = Vec::new();
    let mut remaining: Option<MethodSet> = None;
    let mut rsa_cache = None;
    match &params.cfg.auth {
        SshAuth::Password => {
            let Some(password) = params.password.as_ref().filter(|p| !p.is_empty()) else {
                return Err(DbError::auth(format!(
                    "A password is required for SSH user {}",
                    params.label()
                )));
            };
            let r = handle
                .authenticate_password(user, password.expose())
                .await
                .map_err(send_failed)?;
            tried.push("password".into());
            match r {
                AuthResult::Success => return Ok(()),
                AuthResult::Failure {
                    remaining_methods,
                    partial_success,
                } => {
                    if partial_success {
                        tried.push("(server requires an additional authentication factor)".into());
                    }
                    let kbd = remaining_methods.contains(&MethodKind::KeyboardInteractive);
                    remaining = Some(remaining_methods);
                    if kbd && keyboard_interactive(handle, user, password).await? {
                        return Ok(());
                    }
                    if kbd {
                        tried.push("keyboard-interactive".into());
                    }
                }
            }
        }
        SshAuth::PublicKey { key_path } => {
            let key = Arc::new(load_private_key(key_path, params.key_passphrase.as_ref())?);
            let alg = key.algorithm();
            for hash in hash_candidates(handle, &alg, &mut rsa_cache).await {
                let r = handle
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key.clone(), hash))
                    .await
                    .map_err(send_failed)?;
                tried.push(format!(
                    "public key {} ({})",
                    key_path,
                    algorithm_label(&alg, hash)
                ));
                match r {
                    AuthResult::Success => return Ok(()),
                    AuthResult::Failure {
                        remaining_methods, ..
                    } => remaining = Some(remaining_methods),
                }
            }
        }
        SshAuth::Agent => {
            let mut agent = connect_agent(params.agent_socket.as_deref()).await?;
            let identities = agent.request_identities().await.map_err(|e| {
                DbError::auth(format!("Cannot list the keys of the SSH agent: {e}"))
            })?;
            if identities.is_empty() {
                return Err(DbError::auth(
                    "The SSH agent holds no keys (add one with ssh-add)",
                ));
            }
            for identity in identities {
                let (key, comment): (PublicKey, String) = match &identity {
                    AgentIdentity::PublicKey { key, comment } => (key.clone(), comment.clone()),
                    AgentIdentity::Certificate { .. } => continue,
                };
                let alg = key.algorithm();
                for hash in hash_candidates(handle, &alg, &mut rsa_cache).await {
                    let label = format!(
                        "agent key {} ({})",
                        if comment.is_empty() {
                            fingerprint(&key)
                        } else {
                            comment.clone()
                        },
                        algorithm_label(&alg, hash)
                    );
                    match handle
                        .authenticate_publickey_with(user, key.clone(), hash, &mut agent)
                        .await
                    {
                        Ok(AuthResult::Success) => return Ok(()),
                        Ok(AuthResult::Failure {
                            remaining_methods, ..
                        }) => {
                            tried.push(label);
                            remaining = Some(remaining_methods);
                        }
                        Err(e) => {
                            tried.push(format!("{label}: agent error {e}"));
                            if handle.is_closed() {
                                let mut err = auth_failed(params, &tried, remaining.as_ref());
                                err.message.push_str(
                                    " (the server closed the connection; it may limit the number of attempts)",
                                );
                                return Err(err);
                            }
                        }
                    }
                }
            }
        }
    }
    Err(auth_failed(params, &tried, remaining.as_ref()))
}

/// Answers every keyboard-interactive prompt with the password (servers that
/// only allow passwords through PAM / keyboard-interactive).
async fn keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    password: &Secret,
) -> DbResult<bool> {
    let mut resp = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(send_failed)?;
    for _ in 0..8 {
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers: Vec<String> = prompts
                    .iter()
                    .map(|_| password.expose().to_string())
                    .collect();
                resp = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(send_failed)?;
            }
        }
    }
    Ok(false)
}

/// RSA signature hashes to try, best first, according to the server's `server-sig-algs`.
/// `rsa_cache` keeps the server's answer: asking can take up to a second.
async fn hash_candidates(
    handle: &Handle<ClientHandler>,
    alg: &Algorithm,
    rsa_cache: &mut Option<Vec<Option<HashAlg>>>,
) -> Vec<Option<HashAlg>> {
    if !is_rsa(alg) {
        return vec![None];
    }
    if let Some(c) = rsa_cache {
        return c.clone();
    }
    let candidates = match handle.best_supported_rsa_hash().await {
        Ok(Some(Some(h))) => vec![Some(h)],
        Ok(Some(None)) => vec![None],
        // Server did not announce its algorithms: try SHA-256, then legacy SHA-1.
        _ => vec![Some(HashAlg::Sha256), None],
    };
    *rsa_cache = Some(candidates.clone());
    candidates
}

fn algorithm_label(alg: &Algorithm, hash: Option<HashAlg>) -> String {
    if is_rsa(alg) {
        match hash {
            Some(HashAlg::Sha512) => "rsa-sha2-512".into(),
            Some(HashAlg::Sha256) => "rsa-sha2-256".into(),
            _ => "ssh-rsa".into(),
        }
    } else {
        alg.as_str().to_string()
    }
}

/// Expands a leading `~` to the home directory.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        if path == "~" {
            return home;
        }
        if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Loads an OpenSSH, PEM (PKCS#1 / SEC1), PKCS#8 or PuTTY private key,
/// decrypting it with `passphrase` when needed.
pub(crate) fn load_private_key(
    key_path: &str,
    passphrase: Option<&Secret>,
) -> DbResult<PrivateKey> {
    if key_path.trim().is_empty() {
        return Err(DbError::config(
            "No private key file is configured for SSH public key authentication",
        ));
    }
    let path = expand_tilde(key_path.trim());
    let text = Zeroizing::new(std::fs::read_to_string(&path).map_err(|e| {
        DbError::new(
            ErrorKind::Config,
            format!("Cannot read the private key file {}: {e}", path.display()),
        )
    })?);
    let pass = passphrase.map(|p| p.expose()).filter(|p| !p.is_empty());
    match russh::keys::decode_secret_key(&text, pass) {
        Ok(k) => Ok(k),
        Err(russh::keys::Error::KeyIsEncrypted) => Err(DbError::auth(format!(
            "The private key {} is encrypted; a passphrase is required",
            path.display()
        ))),
        Err(e) if pass.is_some() => Err(DbError::auth(format!(
            "Cannot decrypt the private key {}: wrong passphrase or unsupported format ({e})",
            path.display()
        ))),
        Err(e) => {
            let looks_encrypted = text.contains("ENCRYPTED") || is_encrypted_openssh(&text);
            if looks_encrypted {
                Err(DbError::auth(format!(
                    "The private key {} is encrypted; a passphrase is required",
                    path.display()
                )))
            } else {
                Err(DbError::config(format!(
                    "Cannot read the private key {}: unsupported or corrupt key ({e})",
                    path.display()
                )))
            }
        }
    }
}

fn is_encrypted_openssh(text: &str) -> bool {
    PrivateKey::from_openssh(text)
        .map(|k| k.is_encrypted())
        .unwrap_or(false)
}

type DynAgent =
    AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin + 'static>>;

#[cfg(unix)]
async fn connect_agent(socket: Option<&Path>) -> DbResult<DynAgent> {
    let path: PathBuf = match socket {
        Some(p) => p.to_path_buf(),
        None => match std::env::var_os("SSH_AUTH_SOCK") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => {
                return Err(DbError::auth(
                    "SSH agent authentication is selected but no agent is running (SSH_AUTH_SOCK is not set)",
                ));
            }
        },
    };
    let client = AgentClient::connect_uds(&path).await.map_err(|e| {
        DbError::auth(format!(
            "Cannot connect to the SSH agent at {}: {e}",
            path.display()
        ))
    })?;
    Ok(client.dynamic())
}

#[cfg(windows)]
async fn connect_agent(socket: Option<&Path>) -> DbResult<DynAgent> {
    let pipe = socket
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from(r"\\.\pipe\openssh-ssh-agent"));
    if let Ok(c) = AgentClient::connect_named_pipe(&pipe).await {
        return Ok(c.dynamic());
    }
    match AgentClient::connect_pageant().await {
        Ok(c) => Ok(c.dynamic()),
        Err(e) => Err(DbError::auth(format!(
            "Cannot connect to the OpenSSH agent or Pageant: {e}"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent(_socket: Option<&Path>) -> DbResult<DynAgent> {
    Err(DbError::unsupported(
        "SSH agent authentication is not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_algorithms_are_preferred() {
        let p = preferred_with_known_first(&[Algorithm::Rsa { hash: None }]);
        assert!(is_rsa(&p.key[0]), "{:?}", p.key);
        assert_eq!(p.key.len(), Preferred::default().key.len());
        let p = preferred_with_known_first(&[]);
        assert_eq!(p.key[0], Algorithm::Ed25519);
    }

    #[test]
    fn tilde_expansion() {
        let p = expand_tilde("~/.ssh/id_ed25519");
        assert!(p.ends_with(".ssh/id_ed25519"));
        assert!(!p.starts_with("~"));
        assert_eq!(expand_tilde("/abs/key"), PathBuf::from("/abs/key"));
    }

    #[test]
    fn key_loading_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_private_key(dir.path().join("missing").to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
        let garbage = dir.path().join("garbage");
        std::fs::write(&garbage, "not a key").unwrap();
        let err = load_private_key(garbage.to_str().unwrap(), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);

        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let enc = key.encrypt(&mut rand::rng(), "correct horse").unwrap();
        let path = dir.path().join("enc");
        std::fs::write(
            &path,
            enc.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let p = path.to_str().unwrap();
        let err = load_private_key(p, None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert!(
            err.message.contains("passphrase is required"),
            "{}",
            err.message
        );
        let err = load_private_key(p, Some(&Secret::new("wrong"))).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert!(err.message.contains("wrong passphrase"), "{}", err.message);
        let ok = load_private_key(p, Some(&Secret::new("correct horse"))).unwrap();
        assert_eq!(ok.public_key().key_data(), key.public_key().key_data());
    }

    #[test]
    fn auth_failure_message_lists_methods() {
        let params = SshParams {
            cfg: SshConfig {
                host: "bastion".into(),
                port: 2222,
                user: "deploy".into(),
                ..Default::default()
            },
            password: Some(Secret::new("pw")),
            key_passphrase: None,
            agent_socket: None,
        };
        let set = MethodSet::from(&[MethodKind::PublicKey, MethodKind::Password][..]);
        let e = auth_failed(&params, &["password".into()], Some(&set));
        assert_eq!(e.kind, ErrorKind::Auth);
        assert!(e.message.contains("deploy@bastion:2222"), "{}", e.message);
        assert!(e.message.contains("Tried: password"), "{}", e.message);
        assert!(e.message.contains("publickey, password"), "{}", e.message);
        assert!(!e.full_text().contains("pw\""));
    }
}
