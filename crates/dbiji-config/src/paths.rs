//! Application directories and secure file helpers.
//!
//! Layout (standard mode, via `directories::ProjectDirs::from("dev", "dbiji", "DBiji")`):
//!
//! | Directory | Contents |
//! | --- | --- |
//! | config | `settings.json`, `connections.json`, `keymap.json`, `templates.json` |
//! | data | `history.db`, `projects/`, `tasks/`, `vault.bin`, `known_hosts` |
//! | logs | log files |
//! | cache | disposable caches |
//!
//! Setting the environment variable `DBIJI_HOME` (portable mode, tests) or
//! using [`AppPaths::with_root`] places everything under one root:
//! `<root>/config`, `<root>/data`, `<root>/logs`, `<root>/cache`.
//!
//! All directories are created with mode 0700 and every configuration or
//! secret file is written atomically (temporary file + rename) with mode 0600.

use dbiji_core::{DbError, DbResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Environment variable overriding the root directory (portable mode).
pub const ENV_HOME: &str = "DBIJI_HOME";

/// Resolved application directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    logs_dir: PathBuf,
    cache_dir: PathBuf,
    portable: bool,
}

impl AppPaths {
    /// Standard per-user locations, or `$DBIJI_HOME` when it is set and not empty.
    pub fn new() -> DbResult<AppPaths> {
        if let Some(root) = std::env::var_os(ENV_HOME).filter(|v| !v.is_empty()) {
            return Ok(AppPaths::with_root(PathBuf::from(root)));
        }
        let dirs = directories::ProjectDirs::from("dev", "dbiji", "DBiji").ok_or_else(|| {
            DbError::config("Cannot determine the home directory of the current user")
        })?;
        let logs_dir = default_logs_dir(&dirs);
        Ok(AppPaths {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            logs_dir,
            cache_dir: dirs.cache_dir().to_path_buf(),
            portable: false,
        })
    }

    /// Everything under `root` (portable installs, tests).
    pub fn with_root(root: impl Into<PathBuf>) -> AppPaths {
        let root = root.into();
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            logs_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            portable: true,
        }
    }

    /// True when the paths come from `DBIJI_HOME` / [`AppPaths::with_root`].
    pub fn is_portable(&self) -> bool {
        self.portable
    }

    /// Directory of the JSON configuration files.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Directory of databases, projects, the vault and known_hosts.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Directory for log files.
    pub fn logs_dir(&self) -> &Path {
        &self.logs_dir
    }

    /// Directory for disposable caches.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `config/settings.json`.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    /// `config/connections.json`.
    pub fn connections_file(&self) -> PathBuf {
        self.config_dir.join("connections.json")
    }

    /// `config/keymap.json`.
    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join("keymap.json")
    }

    /// `config/templates.json`.
    pub fn templates_file(&self) -> PathBuf {
        self.config_dir.join("templates.json")
    }

    /// `data/history.db` (query manager).
    pub fn history_db(&self) -> PathBuf {
        self.data_dir.join("history.db")
    }

    /// `data/projects/`.
    pub fn projects_dir(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    /// `data/tasks/`.
    pub fn tasks_dir(&self) -> PathBuf {
        self.data_dir.join("tasks")
    }

    /// `data/ai/`: the AI assistant's saved conversations.
    pub fn ai_conversations_dir(&self) -> PathBuf {
        self.data_dir.join("ai")
    }

    /// `data/vault.bin` (encrypted secret store).
    pub fn vault_file(&self) -> PathBuf {
        self.data_dir.join("vault.bin")
    }

    /// `data/known_hosts` (SSH host keys trusted by DBiji).
    pub fn known_hosts_file(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }

    /// Creates every directory (mode 0700) and tightens the permissions of
    /// existing ones.
    pub fn ensure_dirs(&self) -> DbResult<()> {
        for d in [
            &self.config_dir,
            &self.data_dir,
            &self.logs_dir,
            &self.cache_dir,
            &self.projects_dir(),
            &self.tasks_dir(),
        ] {
            ensure_private_dir(d)?;
        }
        Ok(())
    }
}

fn default_logs_dir(dirs: &directories::ProjectDirs) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(base) = directories::BaseDirs::new() {
            return base.home_dir().join("Library").join("Logs").join("DBiji");
        }
    }
    match dirs.state_dir() {
        Some(state) => state.join("logs"),
        None => dirs.data_local_dir().join("logs"),
    }
}

/// Creates `dir` (and parents) with mode 0700; an existing directory gets
/// its mode set to 0700 (Unix).
pub fn ensure_private_dir(dir: &Path) -> DbResult<()> {
    if !dir.exists() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)
            .map_err(|e| DbError::io(format!("Cannot create directory {}: {e}", dir.display())))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir)
            .map_err(|e| DbError::io(format!("Cannot read directory {}: {e}", dir.display())))?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
                DbError::io(format!(
                    "Cannot restrict permissions of {}: {e}",
                    dir.display()
                ))
            })?;
        }
    }
    Ok(())
}

/// Writes `bytes` to `path` atomically: a temporary file with mode 0600 is
/// written and synced in the same directory, then renamed over `path`.
/// Missing parent directories are created with mode 0700.
pub fn write_secure(path: &Path, bytes: &[u8]) -> DbResult<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if !dir.exists() {
        ensure_private_dir(&dir)?;
    }
    let err = |e: std::io::Error| DbError::io(format!("Cannot write {}: {e}", path.display()));
    // tempfile creates the file with mode 0600 on Unix.
    let mut tmp = tempfile::Builder::new()
        .prefix(".dbiji-")
        .suffix(".tmp")
        .tempfile_in(&dir)
        .map_err(err)?;
    tmp.write_all(bytes).map_err(err)?;
    tmp.flush().map_err(err)?;
    tmp.as_file().sync_all().map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
            .map_err(err)?;
    }
    tmp.persist(path).map_err(|e| err(e.error))?;
    #[cfg(unix)]
    {
        // Make the rename durable.
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Reads and parses a JSON file. A missing file yields `Ok(None)`; malformed
/// JSON is an [`dbiji_core::ErrorKind::Config`] error.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> DbResult<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DbError::io(format!("Cannot read {}: {e}", path.display()))),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| DbError::config(format!("{} is not valid: {e}", path.display())))
}

/// Serializes `value` as pretty JSON and writes it with [`write_secure`].
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> DbResult<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_secure(path, &bytes)
}

/// Moves a corrupt file aside as `<name>.corrupt-<timestamp>` and returns the new path.
pub fn backup_corrupt_file(path: &Path) -> DbResult<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut target = path.with_file_name(format!("{name}.corrupt-{stamp}"));
    let mut n = 1;
    while target.exists() {
        target = path.with_file_name(format!("{name}.corrupt-{stamp}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &target)
        .map_err(|e| DbError::io(format!("Cannot back up {}: {e}", path.display())))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn rooted_layout() {
        let p = AppPaths::with_root("/tmp/x");
        assert!(p.is_portable());
        assert_eq!(
            p.settings_file(),
            PathBuf::from("/tmp/x/config/settings.json")
        );
        assert_eq!(
            p.connections_file(),
            PathBuf::from("/tmp/x/config/connections.json")
        );
        assert_eq!(p.keymap_file(), PathBuf::from("/tmp/x/config/keymap.json"));
        assert_eq!(
            p.templates_file(),
            PathBuf::from("/tmp/x/config/templates.json")
        );
        assert_eq!(p.history_db(), PathBuf::from("/tmp/x/data/history.db"));
        assert_eq!(p.projects_dir(), PathBuf::from("/tmp/x/data/projects"));
        assert_eq!(p.tasks_dir(), PathBuf::from("/tmp/x/data/tasks"));
        assert_eq!(p.vault_file(), PathBuf::from("/tmp/x/data/vault.bin"));
        assert_eq!(
            p.known_hosts_file(),
            PathBuf::from("/tmp/x/data/known_hosts")
        );
        assert_eq!(p.logs_dir(), Path::new("/tmp/x/logs"));
        assert_eq!(p.cache_dir(), Path::new("/tmp/x/cache"));
    }

    #[test]
    fn standard_layout_is_resolvable() {
        // Does not touch the file system; only resolves names.
        if std::env::var_os(ENV_HOME).is_none() {
            let p = AppPaths::new().unwrap();
            assert!(!p.is_portable());
            assert!(p.settings_file().ends_with("settings.json"));
        }
    }

    #[test]
    fn ensure_dirs_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = AppPaths::with_root(dir.path().join("root"));
        p.ensure_dirs().unwrap();
        assert!(p.projects_dir().is_dir());
        assert!(p.tasks_dir().is_dir());
        #[cfg(unix)]
        {
            assert_eq!(mode(p.config_dir()), 0o700);
            assert_eq!(mode(p.data_dir()), 0o700);
            // Loose permissions are tightened.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p.data_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
            p.ensure_dirs().unwrap();
            assert_eq!(mode(p.data_dir()), 0o700);
        }
    }

    #[test]
    fn secure_atomic_writes_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("nested").join("a.json");
        write_json(&f, &serde_json::json!({"a": 1})).unwrap();
        write_json(&f, &serde_json::json!({"a": 2})).unwrap();
        let v: Option<serde_json::Value> = read_json(&f).unwrap();
        assert_eq!(v.unwrap()["a"], 2);
        #[cfg(unix)]
        {
            assert_eq!(mode(&f), 0o600);
            assert_eq!(mode(f.parent().unwrap()), 0o700);
        }
        // No temporary files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(f.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let missing: Option<serde_json::Value> = read_json(&dir.path().join("none.json")).unwrap();
        assert!(missing.is_none());
        std::fs::write(dir.path().join("bad.json"), "{nope").unwrap();
        let bad = read_json::<serde_json::Value>(&dir.path().join("bad.json")).unwrap_err();
        assert_eq!(bad.kind, dbiji_core::ErrorKind::Config);
    }

    #[test]
    fn corrupt_backup() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        std::fs::write(&f, "garbage").unwrap();
        let b1 = backup_corrupt_file(&f).unwrap();
        std::fs::write(&f, "garbage2").unwrap();
        let b2 = backup_corrupt_file(&f).unwrap();
        assert_ne!(b1, b2);
        assert!(!f.exists());
        assert_eq!(std::fs::read_to_string(b2).unwrap(), "garbage2");
    }
}
