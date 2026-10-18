//! SQL Server [`Session`].

use crate::connect::{self, MsClient};
use crate::cursor::{self, ExecMode, Shared, Statement, TaskReturn};
use crate::dialect::MsSqlDialect;
use crate::error::map_error;
use crate::sqltext::{self, StmtClass};
use crate::{admin, catalog, plan};
use async_trait::async_trait;
use dbiji_core::admin::*;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::value::ColumnMeta;
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ExecOptions, QueryCursor, ResultHeader, Row, ServerMessage,
    Session, TransactionMode, Value,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

/// Cancels the statement running on a session (TDS attention).
pub struct SqlServerCancelHandle {
    shared: Arc<Shared>,
}

#[async_trait]
impl CancelHandle for SqlServerCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        self.shared.cancel_active();
        Ok(())
    }
}

/// One SQL Server connection.
///
/// The underlying connection is re-established transparently when it had to
/// be discarded (failed cancellation, network error); the current database
/// and transaction mode are restored. A transaction that was open at that
/// moment is lost: the next `commit` reports it.
pub struct SqlServerSession {
    params: ConnectParams,
    client: Option<MsClient>,
    pending: Option<oneshot::Receiver<TaskReturn>>,
    shared: Arc<Shared>,
    cursor_open: Arc<AtomicBool>,
    info: ServerInfo,
    dialect: Arc<MsSqlDialect>,
    mode: TransactionMode,
    cancel: Arc<SqlServerCancelHandle>,
    closed: bool,
    lost_transaction: bool,
    /// Warning for the next statement after a transparent reconnect.
    reconnect_notice: Option<ServerMessage>,
    /// `SERVERPROPERTY('EngineEdition')` (5 = Azure SQL Database).
    pub(crate) engine_edition: i32,
}

struct InitInfo {
    info: ServerInfo,
    database: Option<String>,
    edition: i32,
}

async fn init_client(client: &mut MsClient, manual: bool) -> DbResult<InitInfo> {
    let mut sql = String::from("SET TEXTSIZE 2147483647;\n");
    if manual {
        sql.push_str("SET IMPLICIT_TRANSACTIONS ON;\n");
    }
    sql.push_str(
        "SELECT @@VERSION, CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), CAST(@@SPID AS int), \
         CAST(SERVERPROPERTY('EngineEdition') AS int), DB_NAME()",
    );
    let row = client
        .simple_query(sql)
        .await
        .map_err(|e| map_error(e, None))?
        .into_row()
        .await
        .map_err(|e| map_error(e, None))?
        .ok_or_else(|| DbError::connection("The server did not return its version"))?;
    let full: Option<&str> = row.try_get(0).map_err(|e| map_error(e, None))?;
    let product_version: Option<&str> = row.try_get(1).map_err(|e| map_error(e, None))?;
    let spid: Option<i32> = row.try_get(2).map_err(|e| map_error(e, None))?;
    let edition: Option<i32> = row.try_get(3).map_err(|e| map_error(e, None))?;
    let db: Option<&str> = row.try_get(4).map_err(|e| map_error(e, None))?;
    let edition = edition.unwrap_or(0);
    let full = full.unwrap_or_default();
    let version_line = full.lines().next().unwrap_or("").trim().to_string();
    let product = match edition {
        5 => "Azure SQL Database",
        8 => "Azure SQL Managed Instance",
        6 | 11 => "Azure Synapse Analytics",
        _ => "Microsoft SQL Server",
    };
    let pv = product_version.unwrap_or_default().to_string();
    Ok(InitInfo {
        info: ServerInfo {
            product: product.to_string(),
            version: if version_line.is_empty() { pv.clone() } else { full.split_whitespace().collect::<Vec<_>>().join(" ") },
            version_number: parse_version_number(&pv),
            session_id: spid.map(|s| s.to_string()),
        },
        database: db.map(str::to_string),
        edition,
    })
}

/// Choose batch or `sp_executesql` for a statement.
pub(crate) fn exec_mode(sql: &str, params: &[Value]) -> ExecMode {
    if !params.is_empty() {
        return ExecMode::Rpc;
    }
    // Plain DML runs through sp_executesql to get per-statement row counts,
    // unless it contains statements whose effect is scoped to the batch.
    if sqltext::classify(sql) == StmtClass::Dml
        && !sqltext::is_scope_sensitive(sql)
        && sqltext::split_batches(sql).len() <= 1
        && !sql.lines().any(|l| l.trim().eq_ignore_ascii_case("GO"))
    {
        ExecMode::Rpc
    } else {
        ExecMode::Batch
    }
}

impl SqlServerSession {
    /// Connect and initialise a session.
    pub async fn connect(params: &ConnectParams) -> DbResult<SqlServerSession> {
        let mut client = connect::open(params).await?;
        let manual = !params.auto_commit;
        let init = init_client(&mut client, manual).await?;
        let shared = Arc::new(Shared::default());
        shared.set_db(init.database.clone());
        Ok(SqlServerSession {
            params: params.clone(),
            client: Some(client),
            pending: None,
            cancel: Arc::new(SqlServerCancelHandle { shared: shared.clone() }),
            shared,
            cursor_open: Arc::new(AtomicBool::new(false)),
            info: init.info,
            dialect: Arc::new(MsSqlDialect::new()),
            mode: if manual { TransactionMode::Manual } else { TransactionMode::Auto },
            closed: false,
            lost_transaction: false,
            reconnect_notice: None,
            engine_edition: init.edition,
        })
    }

    /// Re-open the connection and restore database and transaction mode.
    async fn reconnect(&mut self) -> DbResult<()> {
        let was_in_tx = self.shared.in_transaction.load(Ordering::SeqCst);
        let db = self.shared.db();
        let mut client = connect::open(&self.params).await?;
        let init = init_client(&mut client, self.mode == TransactionMode::Manual).await?;
        if let Some(db) = db.filter(|d| init.database.as_deref() != Some(d.as_str())) {
            let sql = format!("USE {}", crate::dialect::quote(&db));
            if let Err(e) = client.simple_query(sql).await.map_err(|e| map_error(e, None)) {
                tracing::debug!("could not restore database {db}: {e}");
                self.shared.set_db(init.database.clone());
            } else {
                self.shared.set_db(Some(db));
            }
        } else {
            self.shared.set_db(init.database.clone());
        }
        // flush any pending result of USE
        let _ = client.simple_query("SELECT 1").await.map_err(|e| map_error(e, None))?.into_results().await;
        self.info = init.info;
        if was_in_tx {
            self.lost_transaction = true;
        }
        self.reconnect_notice = Some(ServerMessage::new(
            "WARNING",
            if was_in_tx {
                "The connection was re-established: the open transaction was rolled back and session state \
                 (temporary tables, SET options) was lost"
            } else {
                "The connection was re-established: session state (temporary tables, SET options) was lost"
            },
        ));
        self.shared.in_transaction.store(false, Ordering::SeqCst);
        self.client = Some(client);
        Ok(())
    }

    /// Get exclusive use of the connection, waiting for a previous
    /// statement task to hand it back and reconnecting if it was lost.
    async fn acquire(&mut self) -> DbResult<MsClient> {
        if self.closed {
            return Err(DbError::connection("The session is closed"));
        }
        if self.pending.is_some() {
            if self.cursor_open.load(Ordering::SeqCst) {
                return Err(DbError::internal(
                    "A result set is still open on this session; close it before running another statement",
                ));
            }
            if let Some(rx) = self.pending.as_mut() {
                let ret = rx.await;
                self.pending = None;
                self.client = ret.ok().and_then(|r| r.client);
            }
        }
        if self.client.is_none() {
            self.reconnect().await?;
        }
        self.client.take().ok_or_else(|| DbError::internal("No connection available"))
    }

    fn start(&mut self, client: MsClient, stmt: Statement) -> Box<dyn QueryCursor> {
        let (cur, rx) = cursor::spawn(client, stmt, self.shared.clone(), self.cursor_open.clone());
        self.pending = Some(rx);
        Box::new(cur)
    }

    /// Execute with an explicit mode (internal use).
    pub(crate) async fn exec_with(&mut self, sql: &str, params: &[Value], mode: Option<ExecMode>, opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        let client = self.acquire().await?;
        let stmt = Statement {
            sql: sql.to_string(),
            params: params.to_vec(),
            mode: mode.unwrap_or_else(|| exec_mode(sql, params)),
            timeout: opts.timeout,
            resolve_sources: opts.resolve_sources,
            refresh_state: self.mode == TransactionMode::Manual || sqltext::touches_session_state(sql),
            tag: sqltext::first_keyword(sql),
            notice: self.reconnect_notice.take(),
        };
        Ok(self.start(client, stmt))
    }

    /// All result sets of `sql` (batch mode).
    pub(crate) async fn results(&mut self, sql: &str, params: &[Value]) -> DbResult<Vec<(Vec<ColumnMeta>, Vec<Row>)>> {
        let mode = if params.is_empty() { Some(ExecMode::Batch) } else { Some(ExecMode::Rpc) };
        let mut c = self.exec_with(sql, params, mode, &ExecOptions::default()).await?;
        let mut out = Vec::new();
        let res = async {
            while let Some(h) = c.next_result().await? {
                if let ResultHeader::Rows(cols) = h {
                    let mut rows = Vec::new();
                    loop {
                        let b = c.fetch(1024).await?;
                        if b.is_empty() {
                            break;
                        }
                        rows.extend(b);
                    }
                    out.push((cols, rows));
                }
            }
            Ok::<(), DbError>(())
        }
        .await;
        c.close().await?;
        res.map(|_| out)
    }

    /// Rows of the first result set.
    pub(crate) async fn rows(&mut self, sql: &str, params: &[Value]) -> DbResult<Vec<Row>> {
        Ok(self.results(sql, params).await?.into_iter().next().map(|(_, r)| r).unwrap_or_default())
    }

    /// First value of the first row.
    pub(crate) async fn scalar(&mut self, sql: &str, params: &[Value]) -> DbResult<Value> {
        Ok(self.rows(sql, params).await?.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
    }

    /// Run a batch and discard results.
    pub(crate) async fn batch(&mut self, sql: &str) -> DbResult<()> {
        self.results(sql, &[]).await.map(|_| ())
    }

    /// Connected to Azure SQL Database (no cross-database queries).
    pub(crate) fn is_azure_db(&self) -> bool {
        self.engine_edition == 5
    }

    async fn explain_impl(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        let (on, off) = if analyze {
            ("SET STATISTICS XML ON", "SET STATISTICS XML OFF")
        } else {
            ("SET SHOWPLAN_XML ON", "SET SHOWPLAN_XML OFF")
        };
        self.batch(on).await?;
        let res = self.results(sql, &[]).await;
        let off_res = self.batch(off).await;
        let results = res?;
        off_res?;
        let mut docs = Vec::new();
        for (cols, rows) in results {
            if cols.len() == 1 && cols[0].name.contains("Showplan") {
                for r in rows {
                    if let Some(v) = r.into_iter().next()
                        && !v.is_null() {
                            docs.push(v.to_text().into_owned());
                        }
                }
            }
        }
        if docs.is_empty() {
            return Err(DbError::query("The server returned no execution plan for this statement"));
        }
        plan::build_plan(&docs, analyze)
    }
}

#[async_trait]
impl Session for SqlServerSession {
    fn kind(&self) -> DbKind {
        DbKind::SqlServer
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(self.cancel.clone())
    }

    async fn ping(&mut self) -> DbResult<()> {
        let v = self.scalar("SELECT 1", &[]).await?;
        if v.as_i64() == Some(1) { Ok(()) } else { Err(DbError::connection("Unexpected ping response")) }
    }

    async fn close(&mut self) -> DbResult<()> {
        if self.closed {
            return Ok(());
        }
        if let Some(rx) = self.pending.take()
            && !self.cursor_open.load(Ordering::SeqCst)
                && let Ok(r) = rx.await {
                    self.client = r.client;
                }
        self.closed = true;
        if let Some(c) = self.client.take() {
            c.close().await.map_err(|e| map_error(e, None))?;
        }
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed || (self.client.is_none() && self.pending.is_none())
    }

    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        self.exec_with(sql, params, None, opts).await
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        match mode {
            TransactionMode::Manual => self.batch("SET IMPLICIT_TRANSACTIONS ON").await?,
            TransactionMode::Auto => {
                self.batch("WHILE @@TRANCOUNT > 0 COMMIT TRANSACTION;\nSET IMPLICIT_TRANSACTIONS OFF").await?
            }
        }
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        self.shared.in_transaction.load(Ordering::SeqCst)
    }

    async fn commit(&mut self) -> DbResult<()> {
        if self.lost_transaction {
            self.lost_transaction = false;
            return Err(DbError::connection(
                "The connection was re-established; the transaction that was open was rolled back by the server",
            )
            .with_hint("Repeat the changes and commit again"));
        }
        self.batch("WHILE @@TRANCOUNT > 0 COMMIT TRANSACTION").await
    }

    async fn rollback(&mut self) -> DbResult<()> {
        self.lost_transaction = false;
        self.batch("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        let v = self.scalar("SELECT DB_NAME()", &[]).await?;
        let db = v.as_str().map(str::to_string);
        self.shared.set_db(db.clone());
        Ok(db)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        let v = self.scalar("SELECT SCHEMA_NAME()", &[]).await?;
        Ok(v.as_str().map(str::to_string))
    }

    async fn set_current_schema(&mut self, _schema: &str) -> DbResult<()> {
        Err(DbError::unsupported(
            "SQL Server has no per-session current schema: the default schema belongs to the database user \
             (ALTER USER ... WITH DEFAULT_SCHEMA = ...). Qualify object names with the schema instead.",
        ))
    }

    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        self.batch(&format!("USE {}", crate::dialect::quote(database))).await?;
        self.shared.set_db(Some(database.to_string()));
        Ok(())
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        catalog::list_databases(self).await
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        catalog::list_schemas(self, None).await
    }

    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        catalog::list_objects(self, schema, kind).await
    }

    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        catalog::list_children(self, parent, kind).await
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        catalog::table_definition(self, table).await
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        catalog::view_definition(self, view).await
    }

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        catalog::routine_definition(self, routine).await
    }

    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        catalog::sequence_definition(self, sequence).await
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        catalog::object_ddl(self, obj).await
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        catalog::object_properties(self, obj).await
    }

    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        catalog::object_dependencies(self, obj).await
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        catalog::object_statistics(self, obj).await
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        catalog::search_metadata(self, search).await
    }

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        catalog::schema_snapshot(self, schema, opts).await
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        self.explain_impl(sql, analyze).await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        admin::list_sessions(self).await
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        admin::kill_session(self, id, mode).await
    }

    async fn list_locks(&mut self) -> DbResult<LockList> {
        admin::list_locks(self).await
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        admin::server_metrics(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(exec_mode("SELECT 1", &[]), ExecMode::Batch);
        assert_eq!(exec_mode("SELECT @P1", &[Value::Int(1)]), ExecMode::Rpc);
        assert_eq!(exec_mode("UPDATE t SET a = 1", &[]), ExecMode::Rpc);
        assert_eq!(exec_mode("SET NOCOUNT ON; UPDATE t SET a = 1", &[]), ExecMode::Batch);
        assert_eq!(exec_mode("INSERT INTO t VALUES (1)\nGO\nINSERT INTO t VALUES (2)", &[]), ExecMode::Batch);
        assert_eq!(exec_mode("CREATE TABLE t (a int)", &[]), ExecMode::Batch);
        assert_eq!(exec_mode("BEGIN TRAN; DELETE FROM t", &[]), ExecMode::Batch);
    }
}
