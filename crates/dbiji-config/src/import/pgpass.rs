//! PostgreSQL password file (`~/.pgpass`, `PGPASSFILE`,
//! `%APPDATA%\postgresql\pgpass.conf`) with libpq semantics:
//! `hostname:port:database:username:password` per line, `*` matches
//! anything, `\:` and `\\` escape, lines starting with `#` are comments, the
//! first matching line wins. On Unix a file readable by group or others is
//! ignored by [`lookup`], as libpq does.

use super::ImportResult;
use dbiji_core::config::{AuthMethod, ConnectionConfig, DbKind, SecretSlot, secret_key};
use dbiji_core::{DbError, DbResult, Secret};
use std::path::{Path, PathBuf};

/// One line of a pgpass file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgPassEntry {
    /// Host name or `*`.
    pub host: String,
    /// Port or `*`.
    pub port: String,
    /// Database or `*`.
    pub database: String,
    /// User name or `*`.
    pub user: String,
    /// Password (escapes removed).
    pub password: Secret,
    /// 1-based line number.
    pub line: usize,
}

fn field_matches(pattern: &str, value: &str) -> bool {
    pattern == "*" || pattern == value
}

impl PgPassEntry {
    /// libpq matching; `host` "localhost" also matches Unix socket connections.
    pub fn matches(&self, host: &str, port: u16, database: &str, user: &str) -> bool {
        let host = if host.is_empty() || host.starts_with('/') {
            "localhost"
        } else {
            host
        };
        field_matches(&self.host, host)
            && field_matches(&self.port, &port.to_string())
            && field_matches(&self.database, database)
            && field_matches(&self.user, user)
    }
}

/// Parsed pgpass file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgPass {
    /// Entries in file order.
    pub entries: Vec<PgPassEntry>,
    /// Problems found while reading (malformed lines, insecure permissions).
    pub warnings: Vec<String>,
    /// The file is readable by group / others (libpq ignores such files).
    pub insecure_permissions: bool,
}

/// Splits one line into its five fields, removing escapes.
fn split_fields(line: &str) -> Option<[String; 5]> {
    let mut fields: Vec<String> = Vec::with_capacity(5);
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => cur.push(n),
                None => cur.push('\\'),
            },
            ':' if fields.len() < 4 => fields.push(std::mem::take(&mut cur)),
            // The password ends at the first unescaped ':' (libpq).
            ':' => break,
            _ => cur.push(c),
        }
    }
    if fields.len() < 4 {
        return None;
    }
    fields.push(cur);
    fields.try_into().ok()
}

impl PgPass {
    /// Parses the text of a pgpass file.
    pub fn parse(text: &str) -> PgPass {
        let mut out = PgPass::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim_end_matches(['\r', '\n']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match split_fields(line) {
                Some([host, port, database, user, password]) => out.entries.push(PgPassEntry {
                    host,
                    port,
                    database,
                    user,
                    password: Secret::from(password),
                    line: i + 1,
                }),
                None => out.warnings.push(format!(
                    "pgpass line {} is malformed and was ignored",
                    i + 1
                )),
            }
        }
        out
    }

    /// Reads and parses `path`, checking its permissions.
    pub fn load(path: &Path) -> DbResult<PgPass> {
        let text = zeroize::Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|e| DbError::io(format!("Cannot read {}: {e}", path.display())))?,
        );
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut p = PgPass::parse(&text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path)
                && meta.permissions().mode() & 0o077 != 0
            {
                p.insecure_permissions = true;
                p.warnings.push(format!(
                    "{} is readable by other users; permissions should be 0600 (libpq ignores this file)",
                    path.display()
                ));
            }
        }
        Ok(p)
    }

    /// The password file libpq would use: `PGPASSFILE`, else `~/.pgpass`
    /// (`%APPDATA%\postgresql\pgpass.conf` on Windows).
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("PGPASSFILE").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(p));
        }
        #[cfg(windows)]
        {
            std::env::var_os("APPDATA")
                .map(|a| PathBuf::from(a).join("postgresql").join("pgpass.conf"))
        }
        #[cfg(not(windows))]
        {
            directories::BaseDirs::new().map(|b| b.home_dir().join(".pgpass"))
        }
    }

    /// Password of the first matching entry.
    pub fn lookup(&self, host: &str, port: u16, database: &str, user: &str) -> Option<Secret> {
        self.entries
            .iter()
            .find(|e| e.matches(host, port, database, user))
            .map(|e| e.password.clone())
    }
}

/// Looks a password up in the default pgpass file, like libpq (an insecure
/// or missing file yields `None`).
pub fn lookup(host: &str, port: u16, database: &str, user: &str) -> Option<Secret> {
    let path = PgPass::default_path()?;
    lookup_in(&path, host, port, database, user)
}

/// Looks a password up in `path` (libpq semantics).
pub fn lookup_in(path: &Path, host: &str, port: u16, database: &str, user: &str) -> Option<Secret> {
    let p = PgPass::load(path).ok()?;
    if p.insecure_permissions {
        tracing::warn!(
            "ignoring {} because it is readable by other users",
            path.display()
        );
        return None;
    }
    p.lookup(host, port, database, user)
}

/// Creates one PostgreSQL connection per concrete pgpass entry, using
/// pgpass authentication (passwords stay in the pgpass file).
pub fn import_pgpass(path: &Path) -> DbResult<ImportResult> {
    import_pgpass_with(path, false)
}

/// Like [`import_pgpass`]; with `store_passwords` the connections use
/// password authentication and the passwords are returned in
/// [`ImportResult::secrets`].
pub fn import_pgpass_with(path: &Path, store_passwords: bool) -> DbResult<ImportResult> {
    let p = PgPass::load(path)?;
    let mut result = ImportResult {
        warnings: p.warnings.clone(),
        ..Default::default()
    };
    let mut seen: Vec<(String, String, String, String)> = Vec::new();
    for e in &p.entries {
        if e.host == "*" || e.user == "*" {
            result.warnings.push(format!(
                "pgpass line {}: entries with a wildcard host or user cannot be imported as connections",
                e.line
            ));
            continue;
        }
        let ident = (
            e.host.clone(),
            e.port.clone(),
            e.database.clone(),
            e.user.clone(),
        );
        if seen.contains(&ident) {
            continue;
        }
        seen.push(ident);
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, String::new());
        cfg.host = e.host.clone();
        cfg.port = match e.port.as_str() {
            "*" => DbKind::Postgres.default_port(),
            p => match p.parse::<u16>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    result
                        .warnings
                        .push(format!("pgpass line {}: invalid port '{p}'", e.line));
                    continue;
                }
            },
        };
        cfg.database = (e.database != "*").then(|| e.database.clone());
        cfg.user = Some(e.user.clone());
        cfg.name = match &cfg.database {
            Some(db) => format!("{}@{}/{db}", e.user, e.host),
            None => format!("{}@{}", e.user, e.host),
        };
        if store_passwords {
            cfg.auth = AuthMethod::Password;
            cfg.save_password = true;
            result.secrets.push((
                secret_key(&cfg.id, SecretSlot::Password),
                e.password.clone(),
            ));
        } else {
            cfg.auth = AuthMethod::Pgpass;
            cfg.save_password = false;
        }
        result.connections.push(cfg);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment\n\
        db.example.com:5432:shop:alice:s3cr\\:et\\\\x\n\
        *:*:*:bob:bobpw\n\
        localhost:6543:*:carol:carol:with:colons\n\
        broken line without fields\n\
        db.example.com:5432:shop:alice:shadowed\n";

    #[test]
    fn parse_and_lookup() {
        let p = PgPass::parse(SAMPLE);
        assert_eq!(p.entries.len(), 4);
        assert_eq!(p.warnings.len(), 1);
        assert_eq!(
            p.lookup("db.example.com", 5432, "shop", "alice")
                .unwrap()
                .expose(),
            "s3cr:et\\x"
        );
        assert_eq!(
            p.lookup("anything", 1, "x", "bob").unwrap().expose(),
            "bobpw"
        );
        // Password stops at the first unescaped colon.
        assert_eq!(
            p.lookup("localhost", 6543, "any", "carol")
                .unwrap()
                .expose(),
            "carol"
        );
        // Unix sockets match "localhost".
        assert_eq!(
            p.lookup("/var/run/postgresql", 6543, "x", "carol")
                .unwrap()
                .expose(),
            "carol"
        );
        assert!(p.lookup("db.example.com", 5433, "shop", "alice").is_none());
        assert!(!format!("{p:?}").contains("bobpw"));
    }

    #[test]
    fn file_permissions_and_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pgpass");
        std::fs::write(&path, SAMPLE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                lookup_in(&path, "db.example.com", 5432, "shop", "alice").is_none(),
                "insecure file ignored"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            lookup_in(&path, "db.example.com", 5432, "shop", "alice")
                .unwrap()
                .expose(),
            "s3cr:et\\x"
        );

        let r = import_pgpass(&path).unwrap();
        assert_eq!(r.connections.len(), 2, "{:?}", r.connections);
        assert!(r.secrets.is_empty());
        let alice = &r.connections[0];
        assert_eq!(alice.auth, AuthMethod::Pgpass);
        assert_eq!(alice.database.as_deref(), Some("shop"));
        assert_eq!(alice.port, Some(5432));
        let carol = &r.connections[1];
        assert_eq!(carol.database, None);
        assert_eq!(carol.port, Some(6543));
        assert!(r.warnings.iter().any(|w| w.contains("wildcard")));
        assert!(r.warnings.iter().any(|w| w.contains("malformed")));

        let r = import_pgpass_with(&path, true).unwrap();
        assert_eq!(r.secrets.len(), 2);
        assert_eq!(
            r.secrets[0].0,
            format!("conn:{}:password", r.connections[0].id)
        );
        assert_eq!(r.secrets[0].1.expose(), "s3cr:et\\x");
        assert_eq!(r.connections[0].auth, AuthMethod::Password);
    }
}
