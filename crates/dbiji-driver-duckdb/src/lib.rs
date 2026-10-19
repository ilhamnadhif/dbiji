//! DuckDB driver for DBiji, built on the bundled DuckDB engine of the
//! `duckdb` crate (embedded, no server).
//!
//! * [`DuckDbDriver`] implements [`dbiji_core::Driver`] for [`DbKind::DuckDb`].
//! * Every [`DuckDbSession`] runs its `duckdb::Connection` on a dedicated OS
//!   thread. Row results are executed in streaming mode and fetched chunk by
//!   chunk (2048 rows) when the cursor asks for rows.
//! * Sessions on the same database file share one DuckDB instance (DuckDB
//!   must not open a file twice in one process); they must therefore agree on
//!   the read-only setting.
//! * Databases are DuckDB catalogs (the main file and `ATTACH`ed databases),
//!   schemas live inside them. `ObjectRef::database` is `None` for the current
//!   database.
//! * [`DuckDbDialect`] / [`DuckDbDdl`] describe the SQL dialect and generate
//!   DDL (sequence backed auto-increment, table rebuilds for constraint
//!   changes).
//! * Extension auto-install / auto-load is disabled by default (it downloads
//!   from the internet); the JSON extension is linked statically.
//!
//! # Connection properties
//!
//! | property | meaning |
//! | --- | --- |
//! | `read_only` | open read-only (also `ConnectParams::read_only`) |
//! | `threads` | worker threads of the DuckDB instance |
//! | `max_memory` | memory limit, e.g. `4GB` |
//! | `autoload_extensions` | allow DuckDB to install and load known extensions on demand (default `false`) |
//! | `show_system_objects` | list internal tables, views and schemas (default `false`) |
//! | `setting.<name>` | any other DuckDB configuration option, e.g. `setting.default_order` = `DESC` |

// `DbError` (defined by dbiji-core, 144 bytes) is the error type of the whole
// driver contract; boxing it here would not change the trait signatures.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

mod catalogs;
mod convert;
pub mod ddl;
mod dialect;
mod error;
mod explain;
mod lexer;
mod metadata;
mod session;
mod worker;

pub use ddl::DuckDbDdl;
pub use dialect::{DUCKDB_FUNCTIONS, DUCKDB_KEYWORDS, DUCKDB_SYNTAX, DUCKDB_TYPES, DuckDbDialect};
pub use session::{DuckDbCancelHandle, DuckDbSession};

use async_trait::async_trait;
use dbiji_core::admin::{BackupFormat, BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::value::parse_bool;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Session, TransactionMode};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use worker::OpenConfig;

/// Stack size of connection threads.
const THREAD_STACK: usize = 8 * 1024 * 1024;

static THREAD_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The DuckDB driver.
pub struct DuckDbDriver {
    info: DriverInfo,
    dialect: Arc<DuckDbDialect>,
}

impl Default for DuckDbDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl DuckDbDriver {
    /// Create the driver.
    pub fn new() -> Self {
        DuckDbDriver {
            info: DriverInfo {
                kind: DbKind::DuckDb,
                name: "DuckDB",
                description: "DuckDB analytical database files (embedded engine, no server needed)",
                capabilities: Self::capabilities(),
            },
            dialect: Arc::new(DuckDbDialect::new()),
        }
    }

    /// Feature flags of DuckDB sessions.
    pub fn capabilities() -> Capabilities {
        Capabilities {
            // attached databases are reachable from one session (no extra sessions needed)
            multiple_databases: false,
            schemas: true,
            schema_label: "Schemas",
            switch_database: true,
            transactions: true,
            savepoints: false,
            explain: true,
            explain_analyze: true,
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
                ObjectKind::Sequence,
                ObjectKind::Function,
                ObjectKind::Type,
            ],
            table_child_kinds: vec![
                ObjectKind::Column,
                ObjectKind::PrimaryKey,
                ObjectKind::UniqueKey,
                ObjectKind::ForeignKey,
                ObjectKind::CheckConstraint,
                ObjectKind::Index,
            ],
        }
    }

    /// Connect and return the concrete session type.
    pub async fn connect_duckdb(&self, params: &ConnectParams) -> DbResult<DuckDbSession> {
        let cfg = open_config(params)?;
        let mode = cfg.mode;
        let (tx, rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let n = THREAD_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::thread::Builder::new()
            .name(format!("dbiji-duckdb-{n}"))
            .stack_size(THREAD_STACK)
            .spawn(move || worker::run(cfg, rx, ready_tx))
            .map_err(|e| {
                DbError::internal(format!("Cannot start the DuckDB connection thread: {e}"))
            })?;
        let (shared, info) = ready_rx.await.map_err(|_| {
            DbError::internal("The DuckDB connection thread ended while connecting")
        })??;
        Ok(DuckDbSession::new(
            tx,
            shared,
            info,
            Arc::clone(&self.dialect),
            mode,
        ))
    }
}

fn property<'a>(params: &'a ConnectParams, key: &str) -> Option<&'a str> {
    params
        .property(key)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn bool_property(params: &ConnectParams, key: &str, default: bool) -> DbResult<bool> {
    match property(params, key) {
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
                "No DuckDB database file configured (use :memory: for an in-memory database)",
            )
        })?;
    let p = raw.trim();
    Ok(if p.starts_with(":memory:") {
        p.to_string()
    } else {
        expand_home(p)
    })
}

fn valid_setting_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn open_config(params: &ConnectParams) -> DbResult<OpenConfig> {
    let path = database_path(params)?;
    let threads = match property(params, "threads") {
        None => None,
        Some(v) => Some(v.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| {
            DbError::config(format!("threads must be a positive integer, got '{v}'"))
        })?),
    };
    let mut settings = Vec::new();
    for (k, v) in &params.properties {
        if let Some(name) = k.strip_prefix("setting.") {
            if !valid_setting_name(name) {
                return Err(DbError::config(format!(
                    "Invalid DuckDB setting property '{k}'"
                )));
            }
            settings.push((name.to_string(), v.trim().to_string()));
        }
    }
    Ok(OpenConfig {
        path,
        read_only: params.read_only || bool_property(params, "read_only", false)?,
        threads,
        max_memory: property(params, "max_memory").map(str::to_string),
        autoload_extensions: bool_property(params, "autoload_extensions", false)?,
        settings,
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
    if path.starts_with(":memory:") {
        return Err(DbError::unsupported(
            "An in-memory database cannot be handled by the duckdb command line tool",
        ));
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
        .unwrap_or("duckdb")
        .to_string()
}

/// `EXPORT DATABASE` statement writing the current database into `dir`
/// (`schema.sql`, `load.sql` and one data file per table; `parquet` selects
/// Parquet data files instead of CSV).
pub fn export_database_sql(dir: &Path, parquet: bool) -> String {
    format!(
        "EXPORT DATABASE {} (FORMAT {})",
        dialect::string_literal(&dir.to_string_lossy()),
        if parquet { "parquet" } else { "csv" }
    )
}

/// `IMPORT DATABASE` statement loading a directory written by
/// [`export_database_sql`].
pub fn import_database_sql(dir: &Path) -> String {
    format!(
        "IMPORT DATABASE {}",
        dialect::string_literal(&dir.to_string_lossy())
    )
}

#[async_trait]
impl Driver for DuckDbDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        Ok(Box::new(self.connect_duckdb(params).await?))
    }

    /// `duckdb -readonly <db> -c "EXPORT DATABASE '<output>' (FORMAT ...)"`:
    /// `output` is a directory; [`BackupFormat::Plain`] writes CSV data
    /// files, the other formats Parquet. The tool needs the file lock, so
    /// sessions of this application on the file must be closed first.
    fn backup_command(
        &self,
        params: &ConnectParams,
        opts: &BackupOptions,
    ) -> DbResult<CommandSpec> {
        if opts.schema_only || opts.data_only {
            return Err(DbError::unsupported(
                "DuckDB exports always contain the schema and the data (EXPORT DATABASE)",
            ));
        }
        if opts.output.as_os_str().is_empty() {
            return Err(DbError::config("No backup output directory given"));
        }
        let db = tool_database(params)?;
        let mut args = opts.extra_args.clone();
        args.extend(["-readonly".to_string(), db, "-c".to_string()]);
        args.push(export_database_sql(
            &opts.output,
            opts.format != BackupFormat::Plain,
        ));
        Ok(CommandSpec {
            program: tool_program(&opts.tool_path),
            args,
            env: Vec::new(),
            stdin_file: None,
            stdout_file: None,
        })
    }

    /// `duckdb <db> -c "IMPORT DATABASE '<input>'"` (a directory written by
    /// [`Driver::backup_command`] or `EXPORT DATABASE`).
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
        args.extend([db, "-c".to_string(), import_database_sql(&opts.input)]);
        Ok(CommandSpec {
            program: tool_program(&opts.tool_path),
            args,
            env: Vec::new(),
            stdin_file: None,
            stdout_file: None,
        })
    }

    /// `duckdb -bail [-readonly] <db> < script`.
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let db = tool_database(params)?;
        let mut args = vec!["-bail".to_string()];
        if params.read_only || bool_property(params, "read_only", false)? {
            args.push("-readonly".into());
        }
        args.push(db);
        Ok(CommandSpec {
            program: "duckdb".into(),
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
        let mut cfg = ConnectionConfig::new(DbKind::DuckDb, "t");
        cfg.file_path = Some(path.into());
        ConnectParams::from_config(&cfg, None)
    }

    #[test]
    fn config_parsing() {
        let mut p = params("~/x.duckdb");
        p.properties.insert("threads".into(), "2".into());
        p.properties.insert("max_memory".into(), "1GB".into());
        p.properties
            .insert("setting.default_order".into(), "DESC".into());
        let cfg = open_config(&p).unwrap();
        assert_eq!(cfg.threads, Some(2));
        assert_eq!(cfg.max_memory.as_deref(), Some("1GB"));
        assert_eq!(
            cfg.settings,
            vec![("default_order".to_string(), "DESC".to_string())]
        );
        assert!(!cfg.autoload_extensions);
        assert!(!cfg.path.starts_with('~') || std::env::var_os("HOME").is_none());
        p.properties.insert("threads".into(), "zero".into());
        assert_eq!(
            open_config(&p).unwrap_err().kind,
            dbiji_core::ErrorKind::Config
        );
        let mut p = params("x.duckdb");
        p.properties.insert("setting.x; DROP".into(), "1".into());
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
        let d = DuckDbDriver::new();
        let p = params("/data/app.duckdb");
        let opts = BackupOptions {
            output: "/backups/app".into(),
            ..Default::default()
        };
        let cmd = d.backup_command(&p, &opts).unwrap();
        assert_eq!(cmd.program, "duckdb");
        assert_eq!(
            cmd.args,
            vec![
                "-readonly",
                "/data/app.duckdb",
                "-c",
                "EXPORT DATABASE '/backups/app' (FORMAT csv)"
            ]
        );
        let r = d
            .restore_command(
                &p,
                &RestoreOptions {
                    input: "/b/it's".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            r.args,
            vec!["/data/app.duckdb", "-c", "IMPORT DATABASE '/b/it''s'"]
        );
        let c = d.client_command(&p, Path::new("/s.sql")).unwrap();
        assert_eq!(c.args, vec!["-bail", "/data/app.duckdb"]);
        assert!(
            d.client_command(&params(":memory:"), Path::new("/s.sql"))
                .is_err()
        );
    }
}
