//! [`SqliteSession`]: the async façade of a connection thread, plus its
//! cursor and cancel handle.

use crate::dialect::SqliteDialect;
use crate::error::worker_gone;
use crate::metadata;
use crate::worker::{Command, ExecRequest, FetchReply, Shared, Worker};
use async_trait::async_trait;
use dbiji_core::admin::*;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, ServerMessage};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::{
    CancelHandle, DbError, DbKind, DbResult, ExecOptions, QueryCursor, ResultHeader, Row, Session,
    TransactionMode, Value,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use tokio::sync::oneshot;

/// Cancels the statement running on a session (`sqlite3_interrupt`); also
/// aborts waits for database locks.
pub struct SqliteCancelHandle {
    shared: Arc<Shared>,
}

#[async_trait]
impl CancelHandle for SqliteCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        self.shared.cancel();
        Ok(())
    }
}

/// One SQLite connection, running on its own thread.
pub struct SqliteSession {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    info: ServerInfo,
    dialect: Arc<SqliteDialect>,
    cancel: Arc<SqliteCancelHandle>,
    mode: TransactionMode,
    closed: bool,
    next_cursor: u64,
    /// Main database file as configured (`None` for in-memory databases).
    path: Option<PathBuf>,
}

impl SqliteSession {
    pub(crate) fn new(
        tx: Sender<Command>,
        shared: Arc<Shared>,
        info: ServerInfo,
        dialect: Arc<SqliteDialect>,
        mode: TransactionMode,
        path: Option<PathBuf>,
    ) -> Self {
        let cancel = Arc::new(SqliteCancelHandle {
            shared: Arc::clone(&shared),
        });
        SqliteSession {
            tx,
            shared,
            info,
            dialect,
            cancel,
            mode,
            closed: false,
            next_cursor: 0,
            path,
        }
    }

    /// Main database file of the session (`None` for in-memory databases).
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    fn ensure_open(&self) -> DbResult<()> {
        if self.closed || !self.shared.alive.load(Ordering::SeqCst) {
            Err(worker_gone())
        } else {
            Ok(())
        }
    }

    /// Run `f` with the connection on the session thread.
    pub(crate) async fn call<R, F>(&self, f: F) -> DbResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Worker) -> DbResult<R> + Send + 'static,
    {
        self.ensure_open()?;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Call(Box::new(move |w: &mut Worker| {
                let _ = tx.send(f(w));
            })))
            .map_err(|_| worker_gone())?;
        rx.await.map_err(|_| {
            DbError::internal("The SQLite connection thread failed while processing the request")
        })?
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
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
        self.call(|_| Ok(())).await
    }

    async fn close(&mut self) -> DbResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown { reply: tx }).is_err() {
            return Ok(());
        }
        rx.await.unwrap_or(Ok(()))
    }

    fn is_closed(&self) -> bool {
        self.closed || !self.shared.alive.load(Ordering::SeqCst)
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        self.ensure_open()?;
        self.next_cursor += 1;
        let id = self.next_cursor;
        let (tx, rx) = oneshot::channel();
        let request = ExecRequest {
            cursor: id,
            sql: sql.to_string(),
            params: params.to_vec(),
            opts: opts.clone(),
            reply: tx,
        };
        self.tx
            .send(Command::Execute(Box::new(request)))
            .map_err(|_| worker_gone())?;
        // Created before awaiting: if this future is dropped, the cursor's
        // drop interrupts the statement and releases the thread.
        let mut cursor = SqliteCursor {
            id,
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            pending: None,
            has_rows: false,
            done: true,
            finished: false,
            closed: false,
            in_flight: true,
        };
        let first = rx.await.map_err(|_| worker_gone())?;
        cursor.in_flight = false;
        cursor.pending = Some(first?);
        Ok(Box::new(cursor))
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        self.call(move |w| {
            if mode == TransactionMode::Auto && !w.conn.is_autocommit() {
                w.exec_batch("COMMIT")?;
            }
            w.mode = mode;
            Ok(())
        })
        .await?;
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        self.shared.in_transaction.load(Ordering::SeqCst)
    }

    async fn commit(&mut self) -> DbResult<()> {
        self.call(|w| {
            if w.conn.is_autocommit() {
                Ok(())
            } else {
                w.exec_batch("COMMIT")
            }
        })
        .await
    }

    async fn rollback(&mut self) -> DbResult<()> {
        self.call(|w| {
            if w.conn.is_autocommit() {
                Ok(())
            } else {
                w.exec_batch("ROLLBACK")
            }
        })
        .await
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        self.call(|w| Ok(Some(metadata::main_database_name(w))))
            .await
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some("main".into()))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        if schema.eq_ignore_ascii_case("main") {
            Ok(())
        } else {
            Err(DbError::unsupported(format!(
                "SQLite has no current schema: unqualified names are looked up in temp, main and then the \
                 attached databases in attach order. Qualify names as {}.<name> instead.",
                crate::dialect::quote_ident(schema)
            )))
        }
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        self.call(metadata::list_databases).await
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        self.call(metadata::list_schemas).await
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let schema = schema.schema.clone();
        self.call(move |w| metadata::list_objects(w, schema.as_deref(), kind))
            .await
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let parent = parent.clone();
        self.call(move |w| metadata::list_children(w, &parent, kind))
            .await
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let table = table.clone();
        self.call(move |w| metadata::table_definition(w, &table))
            .await
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let view = view.clone();
        self.call(move |w| metadata::view_definition(w, &view))
            .await
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let obj = obj.clone();
        self.call(move |w| metadata::object_ddl(w, &obj)).await
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let obj = obj.clone();
        self.call(move |w| metadata::object_properties(w, &obj))
            .await
    }

    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        let obj = obj.clone();
        self.call(move |w| metadata::object_dependencies(w, &obj))
            .await
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let obj = obj.clone();
        self.call(move |w| metadata::object_statistics(w, &obj))
            .await
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let search = search.clone();
        self.call(move |w| metadata::search_metadata(w, &search))
            .await
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let schema = schema.schema.clone();
        let opts = *opts;
        self.call(move |w| metadata::schema_snapshot(w, schema.as_deref(), &opts))
            .await
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        if analyze {
            return Err(DbError::unsupported(
                "SQLite has no EXPLAIN ANALYZE; only EXPLAIN QUERY PLAN is available",
            ));
        }
        let sql = sql.to_string();
        self.call(move |w| crate::explain::explain(w, &sql)).await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        Err(DbError::unsupported(
            "SQLite is an embedded database without server sessions",
        ))
    }

    async fn list_locks(&mut self) -> DbResult<LockList> {
        Err(DbError::unsupported("SQLite has no lock manager"))
    }
}

/// Streaming cursor of a [`SqliteSession`].
pub(crate) struct SqliteCursor {
    id: u64,
    tx: Sender<Command>,
    shared: Arc<Shared>,
    /// First header delivered by `execute`.
    pending: Option<Option<ResultHeader>>,
    has_rows: bool,
    done: bool,
    finished: bool,
    closed: bool,
    /// A request is outstanding (its future may have been dropped).
    in_flight: bool,
}

impl SqliteCursor {
    async fn request<T>(
        &mut self,
        cmd: Command,
        rx: oneshot::Receiver<DbResult<T>>,
    ) -> DbResult<T> {
        self.tx.send(cmd).map_err(|_| worker_gone())?;
        self.in_flight = true;
        let r = rx.await;
        self.in_flight = false;
        r.map_err(|_| worker_gone())?
    }
}

#[async_trait]
impl QueryCursor for SqliteCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        if self.closed || self.finished {
            return Ok(None);
        }
        let header = match self.pending.take() {
            Some(h) => h,
            None => {
                let (tx, rx) = oneshot::channel();
                let r = self
                    .request(
                        Command::NextResult {
                            cursor: self.id,
                            reply: tx,
                        },
                        rx,
                    )
                    .await;
                match r {
                    Ok(h) => h,
                    Err(e) => {
                        self.finished = true;
                        self.has_rows = false;
                        return Err(e);
                    }
                }
            }
        };
        match &header {
            Some(ResultHeader::Rows(_)) => {
                self.has_rows = true;
                self.done = false;
            }
            Some(ResultHeader::Update { .. }) => {
                self.has_rows = false;
                self.done = true;
            }
            None => {
                self.finished = true;
                self.has_rows = false;
                self.done = true;
            }
        }
        Ok(header)
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        if self.closed || !self.has_rows || self.done {
            return Ok(Vec::new());
        }
        let (tx, rx) = oneshot::channel();
        match self
            .request(
                Command::Fetch {
                    cursor: self.id,
                    max_rows,
                    reply: tx,
                },
                rx,
            )
            .await
        {
            Ok(FetchReply { rows, done }) => {
                self.done = done || rows.is_empty();
                Ok(rows)
            }
            Err(e) => {
                self.done = true;
                Err(e)
            }
        }
    }

    async fn close(&mut self) -> DbResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::CloseCursor {
                cursor: self.id,
                reply: Some(tx),
            })
            .is_err()
        {
            return Ok(());
        }
        rx.await.unwrap_or(Ok(()))
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

impl Drop for SqliteCursor {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if self.in_flight {
            // a fetch / execute future was dropped mid-way: stop the statement
            self.shared.cancel();
        }
        let _ = self.tx.send(Command::CloseCursor {
            cursor: self.id,
            reply: None,
        });
    }
}
