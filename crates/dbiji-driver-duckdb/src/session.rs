//! [`DuckDbSession`]: the async façade of a connection thread, plus its
//! cursor and cancel handle.

use crate::dialect::{DuckDbDialect, quote_ident};
use crate::error::worker_gone;
use crate::metadata;
use crate::worker::{Command, ExecRequest, FetchReply, Shared, Worker};
use async_trait::async_trait;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, ServerMessage};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::{
    CancelHandle, DbError, DbKind, DbResult, ExecOptions, QueryCursor, ResultHeader, Row, Session,
    TransactionMode, Value,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use tokio::sync::oneshot;

/// Cancels the statement running on a session (`duckdb_interrupt`).
pub struct DuckDbCancelHandle {
    shared: Arc<Shared>,
}

/// The connection thread dropped a reply. It recovers from a panic (duckdb-rs
/// panics on a few types it cannot read): the session is still usable then.
fn reply_lost(shared: &Shared) -> DbError {
    if shared.alive.load(Ordering::SeqCst) {
        DbError::query(
            "DuckDB could not read this result: it may hold a type DBiji cannot convert yet (e.g. TIME_NS); cast that column to VARCHAR",
        )
    } else {
        worker_gone()
    }
}

#[async_trait]
impl CancelHandle for DuckDbCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        self.shared.cancel();
        Ok(())
    }
}

/// One DuckDB connection, running on its own thread.
pub struct DuckDbSession {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    info: ServerInfo,
    dialect: Arc<DuckDbDialect>,
    cancel: Arc<DuckDbCancelHandle>,
    mode: TransactionMode,
    closed: bool,
    next_cursor: u64,
}

impl DuckDbSession {
    pub(crate) fn new(
        tx: Sender<Command>,
        shared: Arc<Shared>,
        info: ServerInfo,
        dialect: Arc<DuckDbDialect>,
        mode: TransactionMode,
    ) -> Self {
        let cancel = Arc::new(DuckDbCancelHandle {
            shared: Arc::clone(&shared),
        });
        DuckDbSession {
            tx,
            shared,
            info,
            dialect,
            cancel,
            mode,
            closed: false,
            next_cursor: 0,
        }
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
            DbError::internal("The DuckDB connection thread failed while processing the request")
        })?
    }
}

#[async_trait]
impl Session for DuckDbSession {
    fn kind(&self) -> DbKind {
        DbKind::DuckDb
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
        self.call(|w| w.conn.execute_batch("SELECT 1").map_err(|e| w.map_err(e)))
            .await
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
            cancel_seq: self.shared.cancel_seq.load(Ordering::SeqCst),
            reply: tx,
        };
        self.tx
            .send(Command::Execute(Box::new(request)))
            .map_err(|_| worker_gone())?;
        // Created before awaiting: if this future is dropped, the cursor's
        // drop interrupts the statement and releases the thread.
        let mut cursor = DuckDbCursor {
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
        let first = rx.await.map_err(|_| reply_lost(&self.shared))?;
        cursor.in_flight = false;
        cursor.pending = Some(first?);
        Ok(Box::new(cursor))
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        self.call(move |w| {
            if mode == TransactionMode::Auto && w.in_tx {
                w.exec_transaction("COMMIT")?;
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
            if w.in_tx {
                w.exec_transaction("COMMIT")
            } else {
                Ok(())
            }
        })
        .await
    }

    async fn rollback(&mut self) -> DbResult<()> {
        self.call(|w| {
            if w.in_tx {
                w.exec_transaction("ROLLBACK")
            } else {
                Ok(())
            }
        })
        .await
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        self.call(|w| w.query_value::<String>("SELECT current_database()", &[]))
            .await
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        self.call(|w| w.query_value::<String>("SELECT current_schema()", &[]))
            .await
    }

    /// `USE <current database>.<schema>`.
    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let schema = schema.to_string();
        self.call(move |w| {
            let db = w
                .query_value::<String>("SELECT current_database()", &[])?
                .unwrap_or_default();
            let sql = format!("USE {}.{}", quote_ident(&db), quote_ident(&schema));
            w.conn.execute_batch(&sql).map_err(|e| w.map_err(e))
        })
        .await
    }

    /// `USE <database>` (an attached database).
    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        let sql = format!("USE {}", quote_ident(database));
        self.call(move |w| w.conn.execute_batch(&sql).map_err(|e| w.map_err(e)))
            .await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        self.call(|w| metadata::list_databases(w)).await
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        self.call(|w| metadata::list_schemas(w)).await
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let schema = schema.clone();
        self.call(move |w| metadata::list_objects(w, &schema, kind))
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

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        let routine = routine.clone();
        self.call(move |w| metadata::routine_definition(w, &routine))
            .await
    }

    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        let sequence = sequence.clone();
        self.call(move |w| metadata::sequence_definition(w, &sequence))
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
        let schema = schema.clone();
        let opts = *opts;
        self.call(move |w| metadata::schema_snapshot(w, &schema, &opts))
            .await
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        let sql = sql.to_string();
        self.call(move |w| crate::explain::explain(w, &sql, analyze))
            .await
    }
}

/// Streaming cursor of a [`DuckDbSession`].
pub(crate) struct DuckDbCursor {
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

impl DuckDbCursor {
    async fn request<T>(
        &mut self,
        cmd: Command,
        rx: oneshot::Receiver<DbResult<T>>,
    ) -> DbResult<T> {
        self.tx.send(cmd).map_err(|_| worker_gone())?;
        self.in_flight = true;
        let r = rx.await;
        self.in_flight = false;
        r.map_err(|_| reply_lost(&self.shared))?
    }
}

#[async_trait]
impl QueryCursor for DuckDbCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        if self.closed || self.finished {
            return Ok(None);
        }
        let header = match self.pending.take() {
            Some(h) => h,
            None => {
                let (tx, rx) = oneshot::channel();
                match self
                    .request(
                        Command::NextResult {
                            cursor: self.id,
                            reply: tx,
                        },
                        rx,
                    )
                    .await
                {
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

impl Drop for DuckDbCursor {
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
