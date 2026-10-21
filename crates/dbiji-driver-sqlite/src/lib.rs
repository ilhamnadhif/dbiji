//! SQLite driver for DBiji, built on the bundled SQLite engine of `rusqlite`.
//!
//! * [`SqliteDriver`] implements [`dbiji_core::Driver`] for [`DbKind::Sqlite`].
//! * Every [`SqliteSession`] runs its `rusqlite::Connection` on a dedicated
//!   OS thread; results are streamed: the prepared statement stays alive on
//!   that thread and is stepped only when the cursor fetches more rows.
//! * Schemas are the attached databases (`main`, `temp`, `ATTACH`ed files).
//! * [`SqliteDialect`] / [`SqliteDdl`] describe the SQL dialect and generate
//!   DDL, including the 12 step table rebuild for `ALTER TABLE` changes
//!   SQLite cannot do natively.
//! * [`online_backup`] copies a database with SQLite's online backup API.
//!
//! # Connection properties
//!
//! | property | meaning |
//! | --- | --- |
//! | `read_only` | open read-only (also `ConnectParams::read_only`) |
//! | `create` | create the file when missing (default `true`) |
//! | `foreign_keys` | enforce foreign keys (default `true`) |
//! | `journal_mode` | `DELETE`, `TRUNCATE`, `PERSIST`, `MEMORY`, `WAL`, `OFF` |
//! | `busy_timeout` | milliseconds to wait for locks (default `5000`) |
//! | `synchronous` | `OFF`, `NORMAL`, `FULL`, `EXTRA` |
//! | `cache_size` | `PRAGMA cache_size` (pages, or KiB when negative) |
//! | `show_system_objects` | list `sqlite_*` and FTS shadow tables (default `false`) |
//! | `pragma.<name>` | any other `PRAGMA <name> = <value>` run after connecting |
//! | `attach.<alias>` | database file attached as `<alias>` after connecting |

// `DbError` (defined by dbiji-core, 144 bytes) is the error type of the whole
// driver contract; boxing it here would not change the trait signatures.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

mod backup;
mod catalogs;
mod convert;
pub mod ddl;
mod dialect;
mod error;
mod explain;
mod lexer;
mod metadata;
mod schema_sql;
mod session;
mod worker;

pub use backup::{
    BackupProgress, BackupReport, BackupSource, ProgressFn, online_backup,
    online_backup_with_progress, vacuum_into_sql,
};
pub use ddl::SqliteDdl;
pub use dialect::{SQLITE_FUNCTIONS, SQLITE_KEYWORDS, SQLITE_SYNTAX, SQLITE_TYPES, SqliteDialect};
pub use session::{SqliteCancelHandle, SqliteSession};

use async_trait::async_trait;
use dbiji_core::admin::{BackupFormat, BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::value::parse_bool;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Session, TransactionMode};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use worker::OpenConfig;

/// Stack size of connection threads (SQLite recursion on deeply nested SQL).
const THREAD_STACK: usize = 8 * 1024 * 1024;

static THREAD_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The SQLite driver.
pub struct SqliteDriver {
    info: DriverInfo,
    dialect: Arc<SqliteDialect>,
}

impl Default for SqliteDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SqliteDriver {
    /// Create the driver.
    pub fn new() -> Self {
        SqliteDriver {
            info: DriverInfo {
                kind: DbKind::Sqlite,
                name: "SQLite",
                description: "SQLite database files (embedded engine, no server needed)",
                capabilities: Self::capabilities(),
            },
            dialect: Arc::new(SqliteDialect::new()),
        }
    }

    /// Feature flags of SQLite sessions.
    pub fn capabilities() -> Capabilities {
        Capabilities {
            multiple_databases: false,
            schemas: true,
            schema_label: "Databases",
            switch_database: false,
            transactions: true,
            savepoints: true,
            explain: true,
            explain_analyze: false,
            session_manager: false,
            lock_manager: false,
            server_metrics: false,
            backup: true,
            restore: true,
            debugger: false,
            cancel: true,
            returning: true,
            schema_object_kinds: vec![
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::Index,
                ObjectKind::Trigger,
            ],
            table_child_kinds: vec![
                ObjectKind::Column,
                ObjectKind::PrimaryKey,
                ObjectKind::UniqueKey,
                ObjectKind::ForeignKey,
                ObjectKind::CheckConstraint,
                ObjectKind::Index,
                ObjectKind::Trigger,
            ],
        }
    }

    /// Connect and return the concrete session type (needed for
    /// [`online_backup`] from a session).
    pub async fn connect_sqlite(&self, params: &ConnectParams) -> DbResult<SqliteSession> {
        let cfg = open_config(params)?;
        let mode = cfg.mode;
        let dialect = if cfg.foreign_keys {
            Arc::clone(&self.dialect)
        } else {
            Arc::new(SqliteDialect::with_foreign_keys(false))
        };
        let path =
            (!cfg.is_memory() && !cfg.path.starts_with("file:")).then(|| PathBuf::from(&cfg.path));
        let (tx, rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let n = THREAD_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::thread::Builder::new()
            .name(format!("dbiji-sqlite-{n}"))
            .stack_size(THREAD_STACK)
            .spawn(move || worker::run(cfg, rx, ready_tx))
            .map_err(|e| {
                DbError::internal(format!("Cannot start the SQLite connection thread: {e}"))
            })?;
        let (shared, info) = ready_rx.await.map_err(|_| {
            DbError::internal("The SQLite connection thread ended while connecting")
        })??;
        Ok(SqliteSession::new(tx, shared, info, dialect, mode, path))
    }
}

fn bool_property(params: &ConnectParams, key: &str, default: bool) -> DbResult<bool> {
    match params
        .property(key)
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        None => Ok(default),
        Some(v) => parse_bool(v).ok_or_else(|| {
            DbError::config(format!("Property '{key}' must be true or false, got '{v}'"))
        }),
    }
}

/// Expand a leading `~/`.
fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Database path from the parameters (`file_path`, else `database`).
fn database_path(params: &ConnectParams) -> DbResult<String> {
    let raw = params
        .file_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.trim().is_empty())
        .or_else(|| params.database.clone().filter(|d| !d.trim().is_empty()))
        .ok_or_else(|| {
            DbError::config(
                "No SQLite database file configured (use :memory: for an in-memory database)",
            )
        })?;
    let p = raw.trim();
    // `:memory:<key>`: one in-memory database shared by the sessions of one
    // connection (memdb VFS: normal locking, unlike the shared cache whose table
    // locks fail at once); `:memory:` alone is private to its session.
    if let Some(key) = p.strip_prefix(":memory:").filter(|k| !k.is_empty()) {
        let key: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        return Ok(format!("file:/dbiji-{key}?vfs=memdb"));
    }
    Ok(if p == ":memory:" || p.starts_with("file:") {
        p.to_string()
    } else {
        expand_home(p)
    })
}

fn open_config(params: &ConnectParams) -> DbResult<OpenConfig> {
    let path = database_path(params)?;
    let read_only = params.read_only || bool_property(params, "read_only", false)?;
    let journal_mode = match params
        .property("journal_mode")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        None => None,
        Some(v) => Some(worker::parse_journal_mode(v).ok_or_else(|| {
            DbError::config(format!(
                "Unknown journal_mode '{v}' (DELETE, TRUNCATE, PERSIST, MEMORY, WAL, OFF)"
            ))
        })?),
    };
    let synchronous = match params
        .property("synchronous")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        None => None,
        Some(v) => Some(worker::parse_synchronous(v).ok_or_else(|| {
            DbError::config(format!(
                "Unknown synchronous mode '{v}' (OFF, NORMAL, FULL, EXTRA)"
            ))
        })?),
    };
    let cache_size =
        match params
            .property("cache_size")
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            None => None,
            Some(v) => Some(v.parse::<i64>().map_err(|_| {
                DbError::config(format!("cache_size must be an integer, got '{v}'"))
            })?),
        };
    let busy_timeout = match params
        .property("busy_timeout")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        None => Duration::from_millis(5000),
        Some(v) => Duration::from_millis(
            v.parse::<u64>()
                .map_err(|_| {
                    DbError::config(format!(
                        "busy_timeout must be a number of milliseconds, got '{v}'"
                    ))
                })?
                .min(86_400_000),
        ),
    };
    let mut pragmas = Vec::new();
    let mut attach = Vec::new();
    for (k, v) in &params.properties {
        if let Some(name) = k.strip_prefix("pragma.") {
            let value = v.trim();
            if !worker::valid_pragma(name, value) {
                return Err(DbError::config(format!(
                    "Invalid pragma property '{k}' = '{v}'"
                )));
            }
            pragmas.push((name.to_string(), value.to_string()));
        } else if let Some(alias) = k.strip_prefix("attach.") {
            if alias.is_empty() || v.trim().is_empty() {
                return Err(DbError::config(format!("Invalid attach property '{k}'")));
            }
            attach.push((alias.to_string(), expand_home(v.trim())));
        }
    }
    Ok(OpenConfig {
        path,
        read_only,
        create: bool_property(params, "create", true)?,
        foreign_keys: bool_property(params, "foreign_keys", true)?,
        journal_mode,
        synchronous,
        cache_size,
        busy_timeout,
        pragmas,
        attach,
        mode: if params.auto_commit {
            TransactionMode::Auto
        } else {
            TransactionMode::Manual
        },
        show_system_objects: bool_property(params, "show_system_objects", false)?,
    })
}

/// Path of the database file for the command line tool.
fn tool_database(params: &ConnectParams) -> DbResult<String> {
    let path = database_path(params)?;
    if path == ":memory:"
        || path.contains("mode=memory")
        || path.contains("vfs=memdb")
        || path.starts_with("file::memory:")
    {
        return Err(DbError::unsupported(
            "An in-memory database cannot be handled by the sqlite3 tool; use the online backup instead",
        ));
    }
    if path.starts_with("file:") {
        return Ok(path);
    }
    // absolute, so that a name starting with '-' is not taken for an option
    Ok(std::path::absolute(&path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(path))
}

fn tool_program(tool_path: &Option<String>) -> String {
    tool_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("sqlite3")
        .to_string()
}

/// Quote a file name argument of a `sqlite3` dot-command.
fn dot_command_arg(path: &Path) -> String {
    let s = path.to_string_lossy();
    if !s.contains('\'') {
        format!("'{s}'")
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        Ok(Box::new(self.connect_sqlite(params).await?))
    }

    /// `sqlite3 -readonly <db> ".dump"` into `output` for [`BackupFormat::Plain`]
    /// (`.schema` when `schema_only`, `--data-only` when `data_only`), and
    /// `sqlite3 -readonly <db> ".backup main '<output>'"` (a binary copy) for
    /// the other formats. `VACUUM INTO` ([`vacuum_into_sql`]) and
    /// [`online_backup`] are in-process alternatives.
    fn backup_command(
        &self,
        params: &ConnectParams,
        opts: &BackupOptions,
    ) -> DbResult<CommandSpec> {
        let db = tool_database(params)?;
        if opts.output.as_os_str().is_empty() {
            return Err(DbError::config("No backup output file given"));
        }
        let mut args = opts.extra_args.clone();
        args.push("-readonly".into());
        args.push(db);
        let mut stdout_file = None;
        match opts.format {
            BackupFormat::Plain => {
                if opts.schema_only {
                    if opts.tables.is_empty() {
                        args.push(".schema --nosys".into());
                    } else {
                        for t in &opts.tables {
                            args.push(format!(".schema {}", dot_command_arg(Path::new(t))));
                        }
                    }
                } else {
                    let mut cmd = String::from(".dump");
                    if opts.data_only {
                        cmd.push_str(" --data-only");
                    }
                    cmd.push_str(" --nosys");
                    for t in &opts.tables {
                        cmd.push(' ');
                        cmd.push_str(&dot_command_arg(Path::new(t)));
                    }
                    args.push(cmd);
                }
                stdout_file = Some(opts.output.clone());
            }
            _ => {
                let schema = opts
                    .database
                    .as_deref()
                    .filter(|d| !d.is_empty())
                    .unwrap_or("main");
                args.push(format!(
                    ".backup {} {}",
                    crate::dialect::quote_ident(schema),
                    dot_command_arg(&opts.output)
                ));
            }
        }
        Ok(CommandSpec {
            program: tool_program(&opts.tool_path),
            args,
            env: Vec::new(),
            stdin_file: None,
            stdout_file,
        })
    }

    /// `sqlite3 -bail <db> < script` for SQL dumps ([`BackupFormat::Plain`]),
    /// `sqlite3 <db> ".restore main '<input>'"` for database file copies.
    fn restore_command(
        &self,
        params: &ConnectParams,
        opts: &RestoreOptions,
    ) -> DbResult<CommandSpec> {
        if params.read_only || bool_property(params, "read_only", false)? {
            return Err(DbError::read_only(
                "Cannot restore into a read-only connection",
            ));
        }
        let db = tool_database(params)?;
        let mut args = opts.extra_args.clone();
        let mut stdin_file = None;
        match opts.format {
            BackupFormat::Plain => {
                args.push("-bail".into());
                args.push(db);
                stdin_file = Some(opts.input.clone());
            }
            _ => {
                let schema = opts
                    .database
                    .as_deref()
                    .filter(|d| !d.is_empty())
                    .unwrap_or("main");
                args.push(db);
                args.push(format!(
                    ".restore {} {}",
                    crate::dialect::quote_ident(schema),
                    dot_command_arg(&opts.input)
                ));
            }
        }
        Ok(CommandSpec {
            program: tool_program(&opts.tool_path),
            args,
            env: Vec::new(),
            stdin_file,
            stdout_file: None,
        })
    }

    /// `sqlite3 -bail [-readonly] <db> < script`.
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let db = tool_database(params)?;
        let mut args = vec!["-bail".to_string()];
        if params.read_only || bool_property(params, "read_only", false)? {
            args.push("-readonly".into());
        }
        args.push(db);
        Ok(CommandSpec {
            program: "sqlite3".into(),
            args,
            env: Vec::new(),
            stdin_file: Some(script.to_path_buf()),
            stdout_file: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ConnectionConfig;

    fn params(path: &str) -> ConnectParams {
        let mut cfg = ConnectionConfig::new(DbKind::Sqlite, "t");
        cfg.file_path = Some(path.into());
        ConnectParams::from_config(&cfg, None)
    }

    #[tokio::test]
    async fn keyed_memory_database_is_shared_by_sessions() {
        let d = SqliteDriver::new();
        let mut a = d.connect(&params(":memory:conn-1")).await.unwrap();
        let mut b = d.connect(&params(":memory:conn-1")).await.unwrap();
        let mut other = d.connect(&params(":memory:conn-2")).await.unwrap();
        dbiji_core::driver::execute_update(a.as_mut(), "CREATE TABLE shared_t (x INTEGER)", &[])
            .await
            .unwrap();
        assert!(
            dbiji_core::driver::query_scalar(b.as_mut(), "SELECT count(*) FROM shared_t", &[])
                .await
                .is_ok()
        );
        assert!(
            dbiji_core::driver::query_scalar(other.as_mut(), "SELECT count(*) FROM shared_t", &[])
                .await
                .is_err()
        );
    }

    #[test]
    fn config_parsing() {
        let mut p = params("~/x.db");
        p.properties.insert("journal_mode".into(), "wal".into());
        p.properties.insert("busy_timeout".into(), "250".into());
        p.properties
            .insert("pragma.temp_store".into(), "MEMORY".into());
        let cfg = open_config(&p).unwrap();
        assert_eq!(cfg.journal_mode.as_deref(), Some("WAL"));
        assert_eq!(cfg.busy_timeout, Duration::from_millis(250));
        assert_eq!(
            cfg.pragmas,
            vec![("temp_store".to_string(), "MEMORY".to_string())]
        );
        assert!(!cfg.path.starts_with('~') || std::env::var_os("HOME").is_none());
        p.properties.insert("journal_mode".into(), "bogus".into());
        assert_eq!(
            open_config(&p).unwrap_err().kind,
            dbiji_core::ErrorKind::Config
        );
        let mut p = params("x.db");
        p.properties
            .insert("pragma.x".into(), "1; DROP TABLE t".into());
        assert!(open_config(&p).is_err());
        let mut none = params("");
        none.file_path = None;
        assert_eq!(
            open_config(&none).unwrap_err().kind,
            dbiji_core::ErrorKind::Config
        );
    }

    #[test]
    fn commands() {
        let d = SqliteDriver::new();
        let p = params("/data/app.db");
        let opts = BackupOptions {
            output: "/backups/app.sql".into(),
            ..Default::default()
        };
        let cmd = d.backup_command(&p, &opts).unwrap();
        assert_eq!(cmd.program, "sqlite3");
        assert_eq!(cmd.args, vec!["-readonly", "/data/app.db", ".dump --nosys"]);
        assert_eq!(
            cmd.stdout_file.as_deref(),
            Some(Path::new("/backups/app.sql"))
        );
        let opts = BackupOptions {
            output: "/backups/it's.db".into(),
            format: BackupFormat::Custom,
            ..Default::default()
        };
        let cmd = d.backup_command(&p, &opts).unwrap();
        assert_eq!(cmd.args[2], ".backup main \"/backups/it's.db\"");
        let cmd = d.client_command(&p, Path::new("/s.sql")).unwrap();
        assert_eq!(cmd.args, vec!["-bail", "/data/app.db"]);
        assert_eq!(cmd.stdin_file.as_deref(), Some(Path::new("/s.sql")));
        let r = d
            .restore_command(
                &p,
                &RestoreOptions {
                    input: "/b.db".into(),
                    format: BackupFormat::Custom,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(r.args, vec!["/data/app.db", ".restore main '/b.db'"]);
        assert!(
            d.backup_command(
                &params(":memory:"),
                &BackupOptions {
                    output: "x".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
