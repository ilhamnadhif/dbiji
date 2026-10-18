//! OpenSSH compatible `known_hosts` handling.
//!
//! DBiji keeps its own known_hosts file (normally `<data dir>/known_hosts`)
//! that it reads and writes, and additionally *reads* the user's
//! `~/.ssh/known_hosts` so hosts already trusted by OpenSSH are accepted.
//!
//! Supported syntax: plain and wildcard host patterns (`*`, `?`), negated
//! patterns (`!host`), comma separated pattern lists, `[host]:port` for
//! non-standard ports, hashed host names (`|1|salt|hash`, HMAC-SHA1) and the
//! `@revoked` marker. `@cert-authority` lines are ignored (host certificates
//! are not trusted through a CA). Malformed lines are skipped.

use crate::stream::unbracket;
use base64::Engine;
use dbiji_core::{DbError, DbResult};
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};
use sha1::Sha1;
use std::path::{Path, PathBuf};

/// Result of checking a server host key against the known_hosts files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is recorded for this host.
    Known {
        /// File containing the matching entry.
        file: PathBuf,
        /// 1-based line number of the entry.
        line: usize,
    },
    /// Nothing is recorded for this host.
    Unknown,
    /// Keys of other types are recorded for this host, but not this one.
    OtherTypesKnown {
        /// Key types recorded for the host (e.g. `ssh-rsa`).
        types: Vec<String>,
        /// File of the first such entry.
        file: PathBuf,
        /// 1-based line number of that entry.
        line: usize,
    },
    /// A different key of the same type is recorded: possible man-in-the-middle.
    Changed {
        /// File containing the conflicting entry.
        file: PathBuf,
        /// 1-based line number of the entry.
        line: usize,
    },
    /// The key is explicitly marked `@revoked`.
    Revoked {
        /// File containing the `@revoked` entry.
        file: PathBuf,
        /// 1-based line number of the entry.
        line: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

#[derive(Debug, Clone)]
struct Entry {
    marker: Marker,
    patterns: String,
    key: PublicKey,
    line: usize,
}

/// Set of known_hosts files: one writable file owned by DBiji plus optional
/// read-only files (the user's OpenSSH file).
#[derive(Debug, Clone)]
pub struct KnownHosts {
    own: PathBuf,
    read_only: Vec<PathBuf>,
}

impl KnownHosts {
    /// Uses `own_file` (read/write) and, if it exists, `~/.ssh/known_hosts` (read only).
    pub fn new(own_file: impl Into<PathBuf>) -> Self {
        let mut read_only = Vec::new();
        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
            read_only.push(home.join(".ssh").join("known_hosts"));
        }
        KnownHosts {
            own: own_file.into(),
            read_only,
        }
    }

    /// Only `own_file`, without consulting the user's OpenSSH files.
    pub fn own_only(own_file: impl Into<PathBuf>) -> Self {
        KnownHosts {
            own: own_file.into(),
            read_only: Vec::new(),
        }
    }

    /// Explicit file list (tests, custom setups).
    pub fn with_files(own_file: impl Into<PathBuf>, read_only: Vec<PathBuf>) -> Self {
        KnownHosts {
            own: own_file.into(),
            read_only,
        }
    }

    /// The writable file.
    pub fn own_file(&self) -> &Path {
        &self.own
    }

    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.own).chain(self.read_only.iter())
    }

    /// Checks `key` presented by `host:port`.
    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
        let name = lookup_name(host, port);
        let mut changed: Option<(PathBuf, usize)> = None;
        let mut other: Option<(PathBuf, usize)> = None;
        let mut other_types: Vec<String> = Vec::new();
        let mut known: Option<(PathBuf, usize)> = None;
        for file in self.files() {
            for e in read_entries(file) {
                if !host_matches(&name, &e.patterns) {
                    continue;
                }
                let same = e.key.key_data() == key.key_data();
                match e.marker {
                    Marker::Revoked if same => {
                        return HostKeyStatus::Revoked {
                            file: file.clone(),
                            line: e.line,
                        };
                    }
                    Marker::Revoked | Marker::CertAuthority => {}
                    Marker::None => {
                        if same {
                            known.get_or_insert((file.clone(), e.line));
                        } else if e.key.algorithm() == key.algorithm() {
                            changed.get_or_insert((file.clone(), e.line));
                        } else {
                            let t = e.key.algorithm().as_str().to_string();
                            if !other_types.contains(&t) {
                                other_types.push(t);
                            }
                            other.get_or_insert((file.clone(), e.line));
                        }
                    }
                }
            }
        }
        if let Some((file, line)) = known {
            return HostKeyStatus::Known { file, line };
        }
        if let Some((file, line)) = changed {
            return HostKeyStatus::Changed { file, line };
        }
        if let Some((file, line)) = other {
            return HostKeyStatus::OtherTypesKnown {
                types: other_types,
                file,
                line,
            };
        }
        HostKeyStatus::Unknown
    }

    /// Key algorithms recorded for `host:port` (used to prefer them during key exchange).
    pub fn known_algorithms(&self, host: &str, port: u16) -> Vec<Algorithm> {
        let name = lookup_name(host, port);
        let mut out: Vec<Algorithm> = Vec::new();
        for file in self.files() {
            for e in read_entries(file) {
                if e.marker == Marker::None && host_matches(&name, &e.patterns) {
                    let a = e.key.algorithm();
                    if !out.contains(&a) {
                        out.push(a);
                    }
                }
            }
        }
        out
    }

    /// Records `key` for `host:port` in the writable file (atomic rewrite, mode 0600).
    pub fn learn(&self, host: &str, port: u16, key: &PublicKey) -> DbResult<()> {
        let mut content = std::fs::read_to_string(&self.own).unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let key_text = key
            .to_openssh()
            .map_err(|e| DbError::internal(format!("Cannot encode host key: {e}")))?;
        content.push_str(&lookup_name(host, port));
        content.push(' ');
        content.push_str(key_text.trim());
        content.push('\n');
        write_private_file(&self.own, content.as_bytes())
    }

    /// Removes every entry for `host:port` from the writable file; returns the number removed.
    pub fn forget(&self, host: &str, port: u16) -> DbResult<usize> {
        let Ok(content) = std::fs::read_to_string(&self.own) else {
            return Ok(0);
        };
        let name = lookup_name(host, port);
        let mut removed = 0;
        let mut out = String::with_capacity(content.len());
        for line in content.lines() {
            let matches = parse_line(line, 0)
                .is_some_and(|e| e.marker == Marker::None && host_matches(&name, &e.patterns));
            if matches {
                removed += 1;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        if removed > 0 {
            write_private_file(&self.own, out.as_bytes())?;
        }
        Ok(removed)
    }
}

/// `SHA256:...` fingerprint of a key, as printed by OpenSSH.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Name looked up in known_hosts: `host` for port 22, else `[host]:port` (lower case).
pub fn lookup_name(host: &str, port: u16) -> String {
    let h = unbracket(host).to_ascii_lowercase();
    if port == 22 {
        h
    } else {
        format!("[{h}]:{port}")
    }
}

fn read_entries(path: &Path) -> Vec<Entry> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .enumerate()
            .filter_map(|(i, l)| parse_line(l, i + 1))
            .collect(),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("cannot read known_hosts file {}: {e}", path.display());
            }
            Vec::new()
        }
    }
}

fn parse_line(line: &str, number: usize) -> Option<Entry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let mut first = fields.next()?;
    let marker = if let Some(m) = first.strip_prefix('@') {
        let marker = match m {
            "revoked" => Marker::Revoked,
            "cert-authority" => Marker::CertAuthority,
            _ => return None,
        };
        first = fields.next()?;
        marker
    } else {
        Marker::None
    };
    let key_type = fields.next()?;
    let blob = fields.next()?;
    let key = PublicKey::from_openssh(&format!("{key_type} {blob}")).ok()?;
    Some(Entry {
        marker,
        patterns: first.to_string(),
        key,
        line: number,
    })
}

/// OpenSSH `match_hostfile` semantics: the name must match at least one
/// positive pattern and no negated pattern.
fn host_matches(name: &str, patterns: &str) -> bool {
    if patterns.starts_with("|1|") {
        return hashed_matches(name, patterns);
    }
    let mut positive = false;
    for pat in patterns.split(',') {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(&neg.to_ascii_lowercase(), name) {
                return false;
            }
        } else if glob_match(&pat.to_ascii_lowercase(), name) {
            positive = true;
        }
    }
    positive
}

fn hashed_matches(name: &str, entry: &str) -> bool {
    let mut parts = entry.split('|').skip(2);
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(hash)) = (b64.decode(salt), b64.decode(hash)) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(name.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

/// Case-sensitive glob match with `*` and `?` (inputs are lower-cased by the caller).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Writes `bytes` to `path` atomically (temporary file + rename) with mode
/// 0600, creating missing parent directories with mode 0700.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> DbResult<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{file_name}.{}.{nanos}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(DbError::io(format!("Cannot write {}: {e}", path.display())));
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> DbResult<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
        .map_err(|e| DbError::io(format!("Cannot create directory {}: {e}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::PrivateKey;

    fn key() -> PublicKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .clone()
    }

    fn line_for(patterns: &str, k: &PublicKey) -> String {
        format!("{patterns} {}\n", k.to_openssh().unwrap())
    }

    #[test]
    fn lookup_names() {
        assert_eq!(lookup_name("Example.COM", 22), "example.com");
        assert_eq!(lookup_name("db", 2222), "[db]:2222");
        assert_eq!(lookup_name("[::1]", 2200), "[::1]:2200");
    }

    #[test]
    fn patterns() {
        assert!(host_matches("db.example.com", "*.example.com"));
        assert!(host_matches("db.example.com", "other,db.example.com"));
        assert!(!host_matches(
            "db.example.com",
            "*.example.com,!db.example.com"
        ));
        assert!(host_matches("[db]:2222", "[db]:2222"));
        assert!(!host_matches("db", "[db]:2222"));
        assert!(host_matches("db1", "db?"));
        assert!(!host_matches("db12", "db?"));
    }

    #[test]
    fn hashed_entries() {
        // Generated with the OpenSSH algorithm: HMAC-SHA1(salt, name).
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update(b"[bastion]:2022")
            .finalize();
        let b64 = base64::engine::general_purpose::STANDARD;
        let entry = format!("|1|{}|{}", b64.encode(salt), b64.encode(mac.into_bytes()));
        assert!(host_matches("[bastion]:2022", &entry));
        assert!(!host_matches("bastion", &entry));
    }

    #[test]
    fn check_learn_change_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("sub").join("known_hosts");
        let kh = KnownHosts::own_only(&own);
        let k1 = key();
        assert_eq!(kh.check("bastion", 2022, &k1), HostKeyStatus::Unknown);
        kh.learn("bastion", 2022, &k1).unwrap();
        assert!(matches!(
            kh.check("bastion", 2022, &k1),
            HostKeyStatus::Known { line: 1, .. }
        ));
        assert_eq!(kh.check("bastion", 22, &k1), HostKeyStatus::Unknown);
        let k2 = key();
        assert!(matches!(
            kh.check("bastion", 2022, &k2),
            HostKeyStatus::Changed { line: 1, .. }
        ));
        assert_eq!(
            kh.known_algorithms("bastion", 2022),
            vec![Algorithm::Ed25519]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&own).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dmode = std::fs::metadata(own.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dmode, 0o700);
        }
        assert_eq!(kh.forget("bastion", 2022).unwrap(), 1);
        assert_eq!(kh.check("bastion", 2022, &k1), HostKeyStatus::Unknown);
    }

    #[test]
    fn read_only_files_revoked_and_other_types() {
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro_known_hosts");
        let k1 = key();
        let k_revoked = key();
        let ecdsa = PrivateKey::random(
            &mut rand::rng(),
            Algorithm::Ecdsa {
                curve: russh::keys::EcdsaCurve::NistP256,
            },
        )
        .unwrap()
        .public_key()
        .clone();
        let text = format!(
            "# comment\n\ngarbage line\n{}@revoked * {}\n{}",
            line_for("host1,host2", &k1),
            k_revoked.to_openssh().unwrap(),
            line_for("ecdsa-only", &ecdsa)
        );
        std::fs::write(&ro, text).unwrap();
        let kh = KnownHosts::with_files(dir.path().join("own"), vec![ro.clone()]);
        assert!(matches!(
            kh.check("HOST2", 22, &k1),
            HostKeyStatus::Known { line: 4, .. }
        ));
        assert!(matches!(
            kh.check("anything", 22, &k_revoked),
            HostKeyStatus::Revoked { .. }
        ));
        match kh.check("ecdsa-only", 22, &k1) {
            HostKeyStatus::OtherTypesKnown { types, .. } => {
                assert_eq!(types, vec!["ecdsa-sha2-nistp256".to_string()])
            }
            other => panic!("unexpected {other:?}"),
        }
        // Learning never touches read-only files.
        kh.learn("new-host", 22, &k1).unwrap();
        assert!(!std::fs::read_to_string(&ro).unwrap().contains("new-host"));
    }

    #[test]
    fn fingerprint_format() {
        assert!(fingerprint(&key()).starts_with("SHA256:"));
    }
}
