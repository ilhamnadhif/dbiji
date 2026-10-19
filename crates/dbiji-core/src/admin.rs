//! Administration model: session/lock manager data, server metrics,
//! maintenance operations, backup/restore commands and the debugger interface.

use crate::error::DbResult;
use crate::meta::ObjectKind;
use crate::secret::Secret;
use crate::value::Value;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Simple table of values with column names (session lists, lock lists,
/// statistics, debugger variables).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TabularData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl TabularData {
    pub fn new(columns: Vec<String>) -> Self {
        TabularData { columns, rows: Vec::new() }
    }
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    /// Value at (row, column name).
    pub fn get(&self, row: usize, column: &str) -> Option<&Value> {
        let c = self.column_index(column)?;
        self.rows.get(row)?.get(c)
    }
}

/// Server sessions as reported by the session manager.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionList {
    pub data: TabularData,
    /// Column holding the session id passed to `Session::kill_session`.
    pub id_column: usize,
    /// Column with the current query text, if any.
    pub query_column: Option<usize>,
    /// Column with the session state (active/idle ...), if any.
    pub state_column: Option<usize>,
    /// Column listing blocking session ids, if any.
    pub blocked_by_column: Option<usize>,
}

/// Locks as reported by the lock manager.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LockList {
    pub data: TabularData,
    /// Column with the id of the session holding / waiting for the lock.
    pub session_column: usize,
    /// Column with the id of the blocking session (empty when not waiting).
    pub blocked_by_column: Option<usize>,
    /// Column telling whether the lock is granted.
    pub granted_column: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillMode {
    /// Cancel the running statement, keep the session.
    CancelQuery,
    /// Terminate the session.
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    /// Instantaneous value (connections, cache hit ratio).
    Gauge,
    /// Monotonic counter; dashboards plot the per-second rate.
    Counter,
}

/// One sample of a server health metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Stable key, e.g. `connections.active`, `xact.commit`.
    pub key: String,
    pub label: String,
    pub value: f64,
    pub kind: MetricKind,
    /// Display unit: "", "%", "bytes", "ms", "rows" ...
    pub unit: String,
    /// Chart group, e.g. "Connections", "Transactions", "I/O".
    pub group: String,
}

impl Metric {
    pub fn gauge(group: &str, key: &str, label: &str, value: f64, unit: &str) -> Self {
        Metric { key: key.into(), label: label.into(), value, kind: MetricKind::Gauge, unit: unit.into(), group: group.into() }
    }
    pub fn counter(group: &str, key: &str, label: &str, value: f64, unit: &str) -> Self {
        Metric { key: key.into(), label: label.into(), value, kind: MetricKind::Counter, unit: unit.into(), group: group.into() }
    }
}

/// Maintenance operation offered by a dialect (VACUUM, ANALYZE, OPTIMIZE ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceOp {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Object kinds the operation applies to (Database = whole database).
    pub applies_to: &'static [ObjectKind],
    /// Boolean options: (key, label, default).
    pub options: &'static [(&'static str, &'static str, bool)],
    /// Statements cannot run inside a transaction block (e.g. VACUUM).
    pub needs_autocommit: bool,
}

/// External program invocation (pg_dump, mysqldump, psql ...).
#[derive(Debug, Clone, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Environment variables; values may contain secrets (PGPASSWORD), so this is never logged.
    pub env: Vec<(String, Secret)>,
    /// File connected to stdin (restore of plain SQL dumps).
    pub stdin_file: Option<PathBuf>,
    /// File receiving stdout (plain dumps).
    pub stdout_file: Option<PathBuf>,
}

impl CommandSpec {
    /// Command line for display with secrets removed.
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') {
                s.push_str(&format!("\"{a}\""));
            } else {
                s.push_str(a);
            }
        }
        if let Some(f) = &self.stdin_file {
            s.push_str(&format!(" < {}", f.display()));
        }
        if let Some(f) = &self.stdout_file {
            s.push_str(&format!(" > {}", f.display()));
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackupFormat {
    /// Plain SQL script.
    #[default]
    Plain,
    /// pg_dump custom archive (`-Fc`).
    Custom,
    /// pg_dump tar archive.
    Tar,
    /// pg_dump directory format.
    Directory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BackupOptions {
    pub output: PathBuf,
    pub format: BackupFormat,
    pub database: Option<String>,
    /// Limit to these schemas / tables (empty = everything).
    pub schemas: Vec<String>,
    pub tables: Vec<String>,
    pub schema_only: bool,
    pub data_only: bool,
    /// Emit `DROP` statements before `CREATE`.
    pub clean: bool,
    /// Include `CREATE DATABASE`.
    pub create_database: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// Use INSERT statements instead of COPY (PostgreSQL) / extended inserts off (MySQL).
    pub inserts: bool,
    /// Compression level 0-9 for formats that support it.
    pub compression: Option<u8>,
    /// MySQL: `--single-transaction`.
    pub single_transaction: bool,
    /// MySQL: include routines and events.
    pub routines: bool,
    /// Every database of the server (PostgreSQL: `pg_dumpall`).
    pub all_databases: bool,
    /// With `all_databases`: only roles and tablespaces (`--globals-only`).
    pub globals_only: bool,
    /// Path to the tool; empty = look it up in PATH.
    pub tool_path: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RestoreOptions {
    pub input: PathBuf,
    pub format: BackupFormat,
    pub database: Option<String>,
    pub clean: bool,
    pub create_database: bool,
    pub no_owner: bool,
    pub single_transaction: bool,
    pub exit_on_error: bool,
    pub tool_path: Option<String>,
    pub extra_args: Vec<String>,
}

/// State of a debugging session after a step / continue.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DebugState {
    /// Current line in the routine source (1-based).
    pub line: Option<u32>,
    pub finished: bool,
    /// Result of the routine once finished.
    pub result: Option<TabularData>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    pub level: u32,
    pub routine: String,
    pub line: u32,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugVariable {
    pub name: String,
    pub data_type: String,
    pub value: String,
    pub is_argument: bool,
}

/// Interactive routine debugger (PostgreSQL pldbgapi).
#[async_trait]
pub trait DebugSession: Send {
    /// Source text of the routine being debugged.
    fn source(&self) -> &str;
    /// Wait until the target hits a breakpoint or finishes.
    async fn wait(&mut self) -> DbResult<DebugState>;
    async fn step_into(&mut self) -> DbResult<DebugState>;
    async fn step_over(&mut self) -> DbResult<DebugState>;
    async fn resume(&mut self) -> DbResult<DebugState>;
    async fn set_breakpoint(&mut self, line: u32) -> DbResult<()>;
    async fn clear_breakpoint(&mut self, line: u32) -> DbResult<()>;
    async fn variables(&mut self) -> DbResult<Vec<DebugVariable>>;
    async fn stack(&mut self) -> DbResult<Vec<StackFrame>>;
    /// Change a variable's value in the current frame.
    async fn set_variable(&mut self, name: &str, value: &str) -> DbResult<()>;
    /// Abort the target and release both sessions.
    async fn abort(&mut self) -> DbResult<()>;
}
