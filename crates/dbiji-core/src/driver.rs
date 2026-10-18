//! The driver interface every database implements.
//!
//! Model:
//! * [`Driver`] is a stateless factory registered once per database product.
//! * [`Session`] is one live server connection. All methods take `&mut self`;
//!   the application serialises access (one operation at a time per session).
//! * [`Session::execute`] returns a `'static` [`QueryCursor`] that streams
//!   results. **Only one cursor may be open per session**: the application
//!   closes (or drops) the previous cursor before issuing any other call on the
//!   same session. Drivers may rely on this and block other calls until the
//!   cursor is gone (MySQL/SQLite move the connection into the cursor).
//! * Long running statements are interrupted out-of-band through the
//!   [`CancelHandle`] obtained once after connecting.

use crate::admin::*;
use crate::config::{ConnectionConfig, DbKind, SslConfig};
use crate::ddl::*;
use crate::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::meta::*;
use crate::plan::QueryPlan;
use crate::secret::Secret;
use crate::value::{ColumnMeta, Row, Value};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Everything a driver needs to open a session. Produced by the application
/// after resolving secrets and starting SSH tunnels / proxies: `host`/`port`
/// may point to a local forwarder, in which case `tls_server_name` holds the
/// real server name for TLS verification.
#[derive(Clone, Debug)]
pub struct ConnectParams {
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub tls_server_name: Option<String>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    /// Database file for embedded engines.
    pub file_path: Option<PathBuf>,
    pub ssl: SslConfig,
    /// Driver specific properties from the connection configuration.
    pub properties: BTreeMap<String, String>,
    /// Reported to the server where supported (PostgreSQL `application_name`).
    pub application_name: String,
    pub connect_timeout: Duration,
    /// Ask the server to make the session read-only where supported
    /// (the application additionally rejects write statements client side).
    pub read_only: bool,
    pub auto_commit: bool,
}

impl ConnectParams {
    /// Direct parameters from a configuration (no tunnel).
    pub fn from_config(cfg: &ConnectionConfig, password: Option<Secret>) -> Self {
        ConnectParams {
            kind: cfg.kind,
            host: cfg.host.clone(),
            port: cfg.effective_port(),
            tls_server_name: None,
            database: cfg.database.clone().filter(|d| !d.is_empty()),
            user: cfg.user.clone().filter(|u| !u.is_empty()),
            password,
            file_path: cfg.file_path.as_ref().map(PathBuf::from),
            ssl: cfg.ssl.clone(),
            properties: cfg.properties.clone(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(cfg.init.connect_timeout_secs.max(1) as u64),
            read_only: cfg.read_only,
            auto_commit: cfg.auto_commit.unwrap_or(true),
        }
    }

    /// Host name to verify the server certificate against.
    pub fn tls_host(&self) -> &str {
        self.tls_server_name.as_deref().unwrap_or(&self.host)
    }

    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Resolve the base table of every result column (`ColumnMeta::source`),
    /// needed to make the result editable. Costs an extra catalog lookup.
    pub resolve_sources: bool,
    /// Statement timeout; drivers cancel the statement when it elapses.
    pub timeout: Option<Duration>,
}

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultHeader {
    /// A row set; read the rows with [`QueryCursor::fetch`].
    Rows(Vec<ColumnMeta>),
    /// A statement without rows (DML, DDL, SET ...).
    Update {
        rows_affected: Option<u64>,
        last_insert_id: Option<u64>,
        /// Command tag / status text, e.g. `CREATE TABLE`, `INSERT 0 3`.
        tag: Option<String>,
    },
}

/// Notice, warning or informational message from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMessage {
    /// NOTICE / WARNING / INFO / DEBUG / LOG ...
    pub severity: String,
    pub message: String,
    pub code: Option<String>,
    pub detail: Option<String>,
}

impl ServerMessage {
    pub fn new(severity: impl Into<String>, message: impl Into<String>) -> Self {
        ServerMessage { severity: severity.into(), message: message.into(), code: None, detail: None }
    }
}

/// Streaming access to the results of one executed statement.
///
/// Protocol: call [`next_result`](QueryCursor::next_result) to advance to the
/// first (and every following) result; for row results call
/// [`fetch`](QueryCursor::fetch) until it returns an empty vector. Advancing
/// with unread rows discards them. `next_result` returning `None` means the
/// statement is complete. Errors raised while executing are returned from
/// `next_result` or `fetch`.
#[async_trait]
pub trait QueryCursor: Send {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>>;

    /// Up to `max_rows` rows of the current row result; empty when exhausted.
    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>>;

    /// Discard remaining results, cancelling server side work when rows remain,
    /// and release the session. Dropping a cursor must be equally safe (the
    /// driver finishes cleanup in the background).
    async fn close(&mut self) -> DbResult<()>;

    /// Server messages received so far (notices, warnings).
    fn take_messages(&mut self) -> Vec<ServerMessage>;
}

/// Out-of-band cancellation of the statement currently running on a session.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> DbResult<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TransactionMode {
    /// Every statement commits immediately.
    #[default]
    Auto,
    /// Statements run inside an implicit transaction until commit / rollback.
    Manual,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// Product name reported by the server, e.g. "PostgreSQL", "MariaDB".
    pub product: String,
    /// Full version string.
    pub version: String,
    /// Parsed (major, minor, patch).
    pub version_number: (u32, u32, u32),
    /// Server side id of this session (PostgreSQL backend pid, MySQL connection id).
    pub session_id: Option<String>,
}

impl ServerInfo {
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.version_number.0, self.version_number.1) >= (major, minor)
    }
}

/// Parse the leading `major.minor.patch` numbers out of a version string.
pub fn parse_version_number(version: &str) -> (u32, u32, u32) {
    let start = version.find(|c: char| c.is_ascii_digit()).unwrap_or(version.len());
    let rest = &version[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    let mut nums = rest[..end]
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>().unwrap_or(0));
    (nums.next().unwrap_or(0), nums.next().unwrap_or(0), nums.next().unwrap_or(0))
}

/// Feature flags used by the UI to show or hide functionality.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// The server hosts several databases that need their own sessions
    /// (PostgreSQL, SQL Server). The navigator shows a Databases level.
    pub multiple_databases: bool,
    /// Objects live in schemas (PostgreSQL, SQL Server) or MySQL databases.
    pub schemas: bool,
    /// Navigator label for the schema level ("Schemas" or "Databases").
    pub schema_label: &'static str,
    /// The session can change its current database (MySQL/SQL Server `USE`).
    pub switch_database: bool,
    pub transactions: bool,
    pub savepoints: bool,
    pub explain: bool,
    pub explain_analyze: bool,
    pub session_manager: bool,
    pub lock_manager: bool,
    pub server_metrics: bool,
    pub backup: bool,
    pub restore: bool,
    pub debugger: bool,
    pub cancel: bool,
    pub returning: bool,
    /// Folders shown under a schema, in display order.
    pub schema_object_kinds: Vec<ObjectKind>,
    /// Folders shown under a table, in display order.
    pub table_child_kinds: Vec<ObjectKind>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            multiple_databases: false,
            schemas: true,
            schema_label: "Schemas",
            switch_database: false,
            transactions: true,
            savepoints: false,
            explain: true,
            explain_analyze: false,
            session_manager: false,
            lock_manager: false,
            server_metrics: false,
            backup: false,
            restore: false,
            debugger: false,
            cancel: true,
            returning: false,
            schema_object_kinds: vec![ObjectKind::Table, ObjectKind::View],
            table_child_kinds: vec![ObjectKind::Column, ObjectKind::Index, ObjectKind::ForeignKey],
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub kind: DbKind,
    pub name: &'static str,
    pub description: &'static str,
    pub capabilities: Capabilities,
}

/// Factory for sessions of one database product.
#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn info(&self) -> &DriverInfo;

    fn kind(&self) -> DbKind {
        self.info().kind
    }

    fn dialect(&self) -> Arc<dyn Dialect>;

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>>;

    /// Command line for a backup with the vendor tool (pg_dump, mysqldump ...).
    fn backup_command(&self, params: &ConnectParams, opts: &BackupOptions) -> DbResult<CommandSpec> {
        let _ = (params, opts);
        Err(DbError::unsupported("Backup is not supported for this database"))
    }

    /// Command line for restoring a backup (pg_restore, psql, mysql ...).
    fn restore_command(&self, params: &ConnectParams, opts: &RestoreOptions) -> DbResult<CommandSpec> {
        let _ = (params, opts);
        Err(DbError::unsupported("Restore is not supported for this database"))
    }

    /// Command line running a script file with the native command line client
    /// ("execute with local client").
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let _ = (params, script);
        Err(DbError::unsupported("No local client is known for this database"))
    }
}

fn unsupported<T>(what: &str) -> DbResult<T> {
    Err(DbError::unsupported(format!("{what} is not supported for this database")))
}

/// One live connection to a database server.
#[async_trait]
pub trait Session: Send {
    fn kind(&self) -> DbKind;
    fn dialect(&self) -> Arc<dyn Dialect>;
    fn server_info(&self) -> &ServerInfo;

    /// Handle for cancelling the running statement from another task.
    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>>;

    /// Lightweight round trip; errors with `ErrorKind::Connection` when the connection is gone.
    async fn ping(&mut self) -> DbResult<()>;

    /// Graceful shutdown. The session must not be used afterwards.
    async fn close(&mut self) -> DbResult<()>;

    /// Best-effort local knowledge that the connection is closed / broken.
    fn is_closed(&self) -> bool;

    // ---------------------------------------------------------------- execution

    /// Execute one statement. `params` bind positional placeholders
    /// (`$1` / `?` as given by [`Dialect::placeholder`]).
    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>>;

    // ------------------------------------------------------------ transactions

    fn transaction_mode(&self) -> TransactionMode;

    /// Switch auto-commit on/off. Switching to `Auto` while a transaction is
    /// open commits it (JDBC semantics).
    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()>;

    /// A transaction is open (manual mode after a statement, or explicit BEGIN).
    fn in_transaction(&self) -> bool;

    async fn commit(&mut self) -> DbResult<()>;
    async fn rollback(&mut self) -> DbResult<()>;

    // ------------------------------------------------------------------ context

    async fn current_database(&mut self) -> DbResult<Option<String>>;
    async fn current_schema(&mut self) -> DbResult<Option<String>>;
    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()>;
    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        let _ = database;
        unsupported("Changing the current database")
    }

    // ----------------------------------------------------------------- metadata

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>>;
    /// Schemas of the current database (MySQL: databases).
    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>>;
    /// Objects of `kind` inside a schema (tables, views, sequences, routines ...).
    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>>;
    /// Children of a table: columns, indexes, constraints, triggers, partitions.
    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>>;

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef>;
    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let _ = view;
        unsupported("Reading view definitions")
    }
    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        let _ = routine;
        unsupported("Reading routine definitions")
    }
    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        let _ = sequence;
        unsupported("Reading sequence definitions")
    }

    /// Complete DDL text of an object as shown in the DDL tab.
    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String>;

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let _ = obj;
        Ok(Vec::new())
    }
    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        let _ = obj;
        Ok(Vec::new())
    }
    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let _ = obj;
        Ok(Vec::new())
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>>;

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot>;

    // --------------------------------------------------------------------- plan

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan>;

    // -------------------------------------------------------------------- admin

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        unsupported("The session manager")
    }
    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        let _ = (id, mode);
        unsupported("Killing sessions")
    }
    async fn list_locks(&mut self) -> DbResult<LockList> {
        unsupported("The lock manager")
    }
    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        unsupported("Server metrics")
    }
    /// Start debugging `routine` called with `args` (SQL literal texts).
    async fn debug_routine(&mut self, routine: &ObjectRef, args: &[String]) -> DbResult<Box<dyn DebugSession>> {
        let _ = (routine, args);
        unsupported("The debugger")
    }
}

/// Run `sql` and collect the first row result completely.
pub async fn query_all(
    session: &mut dyn Session,
    sql: &str,
    params: &[Value],
) -> DbResult<(Vec<ColumnMeta>, Vec<Row>)> {
    let mut cursor = session.execute(sql, params, &ExecOptions::default()).await?;
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    while let Some(h) = cursor.next_result().await? {
        if let ResultHeader::Rows(cols) = h {
            columns = cols;
            loop {
                let batch = cursor.fetch(1024).await?;
                if batch.is_empty() {
                    break;
                }
                rows.extend(batch);
            }
            break;
        }
    }
    cursor.close().await?;
    Ok((columns, rows))
}

/// Run `sql` and return the total number of affected rows (if reported).
pub async fn execute_update(session: &mut dyn Session, sql: &str, params: &[Value]) -> DbResult<Option<u64>> {
    let mut cursor = session.execute(sql, params, &ExecOptions::default()).await?;
    let mut total: Option<u64> = None;
    while let Some(h) = cursor.next_result().await? {
        match h {
            ResultHeader::Update { rows_affected: Some(n), .. } => total = Some(total.unwrap_or(0) + n),
            ResultHeader::Rows(_) => while !cursor.fetch(1024).await?.is_empty() {},
            _ => {}
        }
    }
    cursor.close().await?;
    Ok(total)
}

/// Run `sql` and return the first column of the first row.
pub async fn query_scalar(session: &mut dyn Session, sql: &str, params: &[Value]) -> DbResult<Value> {
    let (_, rows) = query_all(session, sql, params).await?;
    Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
}

/// Run `sql` and return the result as [`TabularData`].
pub async fn query_tabular(session: &mut dyn Session, sql: &str, params: &[Value]) -> DbResult<TabularData> {
    let (cols, rows) = query_all(session, sql, params).await?;
    Ok(TabularData { columns: cols.into_iter().map(|c| c.name).collect(), rows })
}

/// A fully materialised result, for [`MemoryCursor`].
#[derive(Debug, Clone)]
pub struct MaterializedResult {
    pub header: ResultHeader,
    pub rows: Vec<Row>,
}

/// In-memory [`QueryCursor`] for drivers that receive complete results at once
/// (and for tests).
pub struct MemoryCursor {
    results: std::collections::VecDeque<MaterializedResult>,
    current: std::collections::VecDeque<Row>,
    messages: Vec<ServerMessage>,
}

impl MemoryCursor {
    pub fn new(results: Vec<MaterializedResult>) -> Self {
        MemoryCursor { results: results.into(), current: Default::default(), messages: Vec::new() }
    }
    pub fn with_messages(mut self, messages: Vec<ServerMessage>) -> Self {
        self.messages = messages;
        self
    }
    pub fn rows(columns: Vec<ColumnMeta>, rows: Vec<Row>) -> Self {
        Self::new(vec![MaterializedResult { header: ResultHeader::Rows(columns), rows }])
    }
    pub fn update(rows_affected: Option<u64>, tag: Option<String>) -> Self {
        Self::new(vec![MaterializedResult {
            header: ResultHeader::Update { rows_affected, last_insert_id: None, tag },
            rows: Vec::new(),
        }])
    }
}

#[async_trait]
impl QueryCursor for MemoryCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        self.current.clear();
        match self.results.pop_front() {
            Some(r) => {
                self.current = r.rows.into();
                Ok(Some(r.header))
            }
            None => Ok(None),
        }
    }
    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let n = max_rows.min(self.current.len());
        Ok(self.current.drain(..n).collect())
    }
    async fn close(&mut self) -> DbResult<()> {
        self.results.clear();
        self.current.clear();
        Ok(())
    }
    fn take_messages(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DataKind;

    #[test]
    fn version_parse() {
        assert_eq!(parse_version_number("PostgreSQL 16.2 on aarch64"), (16, 2, 0));
        assert_eq!(parse_version_number("8.0.36-0ubuntu0"), (8, 0, 36));
        assert_eq!(parse_version_number("10.11.6-MariaDB"), (10, 11, 6));
    }

    #[tokio::test]
    async fn memory_cursor_protocol() {
        let mut c = MemoryCursor::rows(
            vec![ColumnMeta::new("a", "int", DataKind::Int)],
            (0..5).map(|i| vec![Value::Int(i)]).collect(),
        );
        assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        assert_eq!(c.fetch(3).await.unwrap().len(), 3);
        assert_eq!(c.fetch(3).await.unwrap().len(), 2);
        assert!(c.fetch(3).await.unwrap().is_empty());
        assert!(c.next_result().await.unwrap().is_none());
    }
}
