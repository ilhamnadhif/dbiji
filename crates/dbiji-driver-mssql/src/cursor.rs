//! Statement execution for SQL Server.
//!
//! tiberius result streams borrow `&mut Client`, while DBiji cursors must be
//! `'static`. Each statement therefore runs in a tokio task that owns the
//! client, converts rows and sends them in small batches through a bounded
//! channel (backpressure: the task stops reading from the socket while the
//! channel is full). When the statement finishes — or the cursor is closed
//! or dropped — the task hands the client back to the session through a
//! oneshot channel.
//!
//! Abandoning a cursor (close / drop with rows left) first drains the rest
//! of the response for up to [`DRAIN_BUDGET`], which keeps the connection —
//! and its transaction, temporary tables and SET options — intact.
//!
//! Cancelling (cancel handle or statement timeout), or an abandon that does
//! not drain in time, sends a TDS attention signal so the server stops the
//! work at once. tiberius 0.13 (`Client::cancel_query`) only reads the
//! acknowledgement when it arrives in the same TDS message as the end of the
//! response; SQL Server sends it as a separate message, which leaves the
//! connection one response behind with no public API to read it. In that
//! case the connection is discarded and the session reconnects
//! transparently on its next call (restoring database and transaction mode;
//! the next statement carries a warning that session state was lost).

use crate::connect::MsClient;
use crate::error::map_error;
use crate::sqltext;
use crate::types::{column_meta, convert, executesql_command};
use async_trait::async_trait;
use dbiji_core::value::{ColumnMeta, ColumnSource};
use dbiji_core::{DbError, DbResult, ErrorKind, QueryCursor, ResultHeader, Row, ServerMessage, Value};
use futures::future::FutureExt;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiberius::{ColumnType, CommandItem, QueryItem};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Rows per channel message.
const BATCH_ROWS: usize = 256;
/// Channel capacity in messages.
const CHANNEL_CAPACITY: usize = 8;
/// Maximum time spent sending an attention signal and waiting for its
/// acknowledgement.
pub const ATTENTION_TIMEOUT: Duration = Duration::from_secs(3);
/// Time allowed to drain the rest of an abandoned result before the
/// statement is cancelled instead.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// Message from the statement task to the cursor.
#[derive(Debug)]
pub(crate) enum Event {
    Header(ResultHeader),
    Rows(Vec<Row>),
    Message(ServerMessage),
    Error(DbError),
}

/// State shared by a session, its cancel handle and its statement tasks.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// `@@TRANCOUNT > 0` after the last statement.
    pub in_transaction: AtomicBool,
    /// `DB_NAME()` after the last statement that could change it.
    pub current_db: Mutex<Option<String>>,
    /// Cancel token of the running statement.
    pub active: Mutex<Option<CancellationToken>>,
}

impl Shared {
    pub fn set_db(&self, db: Option<String>) {
        if let Ok(mut g) = self.current_db.lock() {
            *g = db;
        }
    }

    pub fn db(&self) -> Option<String> {
        self.current_db.lock().ok().and_then(|g| g.clone())
    }

    fn set_active(&self, token: Option<CancellationToken>) {
        if let Ok(mut g) = self.active.lock() {
            *g = token;
        }
    }

    /// Cancel the running statement, if any.
    pub fn cancel_active(&self) {
        if let Some(t) = self.active.lock().ok().and_then(|g| g.clone()) {
            t.cancel();
        }
    }
}

/// How the statement is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecMode {
    /// SQL batch (`simple_query`): session scope, no per-statement counts.
    Batch,
    /// `sp_executesql` RPC: parameters and rows-affected counts.
    Rpc,
}

/// A statement to run.
#[derive(Debug, Clone)]
pub(crate) struct Statement {
    pub sql: String,
    pub params: Vec<Value>,
    pub mode: ExecMode,
    pub timeout: Option<Duration>,
    pub resolve_sources: bool,
    /// Re-read `@@TRANCOUNT` / `DB_NAME()` afterwards.
    pub refresh_state: bool,
    /// Command tag for row-less results (first keyword).
    pub tag: Option<String>,
    /// Message delivered before the results (e.g. a reconnect warning).
    pub notice: Option<ServerMessage>,
    /// `datetimeoffset` values in this zone (property `time_zone`: `local` or `utc`).
    pub zone: Option<crate::types::Zone>,
}

/// What the task gives back to the session.
pub(crate) struct TaskReturn {
    /// The connection, unless it had to be discarded.
    pub client: Option<MsClient>,
}

enum Interrupt {
    Cancelled,
    TimedOut,
    Abandoned,
}

enum Step {
    Done,
    /// A server error ended the batch (the connection is clean).
    Failed(DbError),
    /// The connection is unusable.
    Broken(DbError),
    Interrupted(Interrupt),
    /// The cursor went away and the response was drained: connection clean.
    AbandonedClean,
}

/// One item from either tiberius stream type.
enum Item {
    Meta(Vec<tiberius::Column>),
    Row(tiberius::Row),
    Count(u64),
}

/// Described result column (for `resolve_sources`).
#[derive(Debug, Clone, Default)]
struct Described {
    type_name: String,
    nullable: Option<bool>,
    precision: Option<u32>,
    scale: Option<u32>,
    max_length: Option<u32>,
    identity: Option<bool>,
    source: Option<ColumnSource>,
}

/// Start `stmt` on `client`. Returns the cursor and the receiver through
/// which the client comes back.
/// Ids of cursors: the session's flag holds the id of the open one.
static NEXT_CURSOR: AtomicU64 = AtomicU64::new(1);

pub(crate) fn spawn(client: MsClient, stmt: Statement, shared: Arc<Shared>, open_flag: Arc<AtomicU64>) -> (ChannelCursor, oneshot::Receiver<TaskReturn>) {
    let (tx, rx) = mpsc::channel::<Event>(CHANNEL_CAPACITY);
    let (ret_tx, ret_rx) = oneshot::channel::<TaskReturn>();
    let cancel = CancellationToken::new();
    let abandon = CancellationToken::new();
    shared.set_active(Some(cancel.clone()));
    open_flag.store(NEXT_CURSOR.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    let task_abandon = abandon.clone();
    tokio::spawn(async move {
        let client = run(client, &stmt, &shared, &tx, &cancel, &task_abandon).await;
        shared.set_active(None);
        // hand the connection back before signalling the end of results
        let _ = ret_tx.send(TaskReturn { client });
        drop(tx);
    });
    let on_close: Box<dyn FnOnce() + Send> = Box::new(move || abandon.cancel());
    (ChannelCursor::new(rx, Some(on_close), open_flag), ret_rx)
}

/// Deliver an event, waiting for channel capacity (the consumer). Time spent
/// here does not count towards the statement timeout.
async fn send(tx: &mpsc::Sender<Event>, ev: Event, cancel: &CancellationToken, abandon: &CancellationToken) -> Result<(), Interrupt> {
    tokio::select! {
        biased;
        _ = abandon.cancelled() => Err(Interrupt::Abandoned),
        _ = cancel.cancelled() => Err(Interrupt::Cancelled),
        r = tx.send(ev) => r.map_err(|_| Interrupt::Abandoned),
    }
}

/// Statement timeout budget, consumed only while waiting for the server
/// (not while the consumer is slow to fetch rows).
#[derive(Debug, Clone, Copy)]
struct ServerClock {
    remaining: Option<Duration>,
}

impl ServerClock {
    fn new(timeout: Option<Duration>) -> Self {
        ServerClock { remaining: timeout }
    }

    /// Completes when the remaining budget is used up.
    async fn expired(&self) {
        match self.remaining {
            Some(d) => tokio::time::sleep(d).await,
            None => futures::future::pending::<()>().await,
        }
    }

    fn consume(&mut self, since: Instant) {
        if let Some(r) = self.remaining.as_mut() {
            *r = r.saturating_sub(since.elapsed());
        }
    }
}

/// Is a tiberius error fatal for the connection?
fn is_fatal(e: &tiberius::error::Error) -> bool {
    match e {
        tiberius::error::Error::Server(t) => t.class() >= 20,
        tiberius::error::Error::Io { .. } | tiberius::error::Error::Protocol(_) | tiberius::error::Error::Tls(_) => true,
        _ => false,
    }
}

async fn run(mut client: MsClient, stmt: &Statement, shared: &Shared, tx: &mpsc::Sender<Event>, cancel: &CancellationToken, abandon: &CancellationToken) -> Option<MsClient> {
    let mut clock = ServerClock::new(stmt.timeout);
    if let Some(n) = stmt.notice.clone() {
        let _ = tx.send(Event::Message(n)).await;
    }
    let mut described = None;
    if stmt.resolve_sources {
        match describe(&mut client, stmt).await {
            Ok(d) => described = Some(d),
            Err(e) if is_fatal_db(&e) => {
                let _ = tx.send(Event::Error(e)).await;
                return None;
            }
            Err(e) => tracing::debug!("describe_first_result_set failed: {e}"),
        }
    }

    let batches: Vec<(String, u32)> = if stmt.mode == ExecMode::Batch && stmt.params.is_empty() { sqltext::split_batches(&stmt.sql) } else { vec![(stmt.sql.clone(), 1)] };
    let mut emitted = false;
    let mut outcome = Step::Done;
    // Error positions are reported within the batch: shifted to the script (after a GO).
    let mut search_from = 0usize;
    'outer: for (batch, repeat) in &batches {
        let at = stmt.sql[search_from..].find(batch.as_str()).map(|i| search_from + i);
        if let Some(at) = at {
            search_from = at + batch.len();
        }
        let chars_before = at.map_or(0, |at| stmt.sql[..at].chars().count());
        for _ in 0..*repeat {
            match run_batch(&mut client, stmt, batch, tx, cancel, abandon, &mut clock, &mut described, &mut emitted).await {
                Step::Done => {}
                other => {
                    outcome = match other {
                        Step::Failed(e) => Step::Failed(shift_position(e, chars_before)),
                        Step::Broken(e) => Step::Broken(shift_position(e, chars_before)),
                        other => other,
                    };
                    break 'outer;
                }
            }
        }
    }

    let mut client = Some(client);
    match outcome {
        Step::Done => {
            if !emitted {
                let _ = tx.send(Event::Header(ResultHeader::Update { rows_affected: None, last_insert_id: None, tag: stmt.tag.clone() })).await;
            }
        }
        Step::AbandonedClean => {}
        Step::Failed(e) => {
            let _ = tx.send(Event::Error(e)).await;
        }
        Step::Broken(e) => {
            let _ = tx.send(Event::Error(e)).await;
            client = None;
        }
        Step::Interrupted(reason) => {
            let mut c = client.take();
            let mut lost = false;
            if let Some(cl) = c.as_mut() {
                match tokio::time::timeout(ATTENTION_TIMEOUT, cl.cancel_query()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!("attention not acknowledged in sync, discarding the connection: {e}");
                        lost = true;
                    }
                    Err(_) => {
                        tracing::debug!("attention not acknowledged in time, discarding the connection");
                        lost = true;
                    }
                }
            }
            if !lost {
                client = c;
            }
            let reset_note = "The connection had to be re-established; session state (temporary tables, SET options, open transaction) was lost";
            let err = match reason {
                Interrupt::Cancelled => Some(DbError::cancelled()),
                Interrupt::TimedOut => {
                    Some(DbError::timeout(format!("Statement timeout of {:.1} s exceeded; the statement was cancelled", stmt.timeout.map(|t| t.as_secs_f64()).unwrap_or(0.0))))
                }
                Interrupt::Abandoned => None,
            };
            if let Some(mut e) = err {
                if lost {
                    e = e.with_detail(reset_note);
                }
                let _ = tx.send(Event::Error(e)).await;
            }
        }
    }

    if stmt.refresh_state
        && let Some(cl) = client.as_mut()
    {
        match tokio::time::timeout(Duration::from_secs(15), refresh_state(cl)).await {
            Ok(Ok((tc, db))) => {
                shared.in_transaction.store(tc > 0, Ordering::SeqCst);
                shared.set_db(db);
            }
            Ok(Err(e)) => {
                tracing::debug!("state refresh failed: {e}");
                if is_fatal_db(&e) {
                    client = None;
                }
            }
            Err(_) => client = None,
        }
    }
    client
}

fn is_fatal_db(e: &DbError) -> bool {
    e.kind == ErrorKind::Connection
}

/// `@@TRANCOUNT` and `DB_NAME()`.
pub(crate) async fn refresh_state(client: &mut MsClient) -> DbResult<(i64, Option<String>)> {
    let row = client.simple_query("SELECT CAST(@@TRANCOUNT AS int), DB_NAME()").await.map_err(|e| map_error(e, None))?.into_row().await.map_err(|e| map_error(e, None))?;
    let Some(row) = row else { return Ok((0, None)) };
    let tc: Option<i32> = row.try_get(0).map_err(|e| map_error(e, None))?;
    let db: Option<&str> = row.try_get(1).map_err(|e| map_error(e, None))?;
    Ok((i64::from(tc.unwrap_or(0)), db.map(str::to_string)))
}

#[allow(clippy::too_many_arguments)]
async fn run_batch(
    client: &mut MsClient,
    stmt: &Statement,
    sql: &str,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    abandon: &CancellationToken,
    clock: &mut ServerClock,
    described: &mut Option<Vec<Described>>,
    emitted: &mut bool,
) -> Step {
    let use_rpc = stmt.mode == ExecMode::Rpc || !stmt.params.is_empty();
    let start = async {
        let s: BoxStream<'_, tiberius::Result<Item>> = if use_rpc {
            let cmd = match executesql_command(sql, &stmt.params) {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            cmd.exec(client)
                .await?
                .try_filter_map(|ci| async move {
                    Ok(match ci {
                        CommandItem::Metadata(m) => Some(Item::Meta(m.columns().to_vec())),
                        CommandItem::Row(r) => Some(Item::Row(r)),
                        CommandItem::RowsAffected(n) => Some(Item::Count(n)),
                        _ => None,
                    })
                })
                .boxed()
        } else {
            client
                .simple_query(sql.to_string())
                .await?
                .map_ok(|qi| match qi {
                    QueryItem::Metadata(m) => Item::Meta(m.columns().to_vec()),
                    QueryItem::Row(r) => Item::Row(r),
                })
                .boxed()
        };
        Ok::<_, tiberius::error::Error>(Ok(s))
    };
    tokio::pin!(start);
    let waited = Instant::now();
    let started = tokio::select! {
        biased;
        _ = abandon.cancelled() => None,
        _ = cancel.cancelled() => return Step::Interrupted(Interrupt::Cancelled),
        _ = clock.expired() => return Step::Interrupted(Interrupt::TimedOut),
        r = &mut start => Some(r),
    };
    clock.consume(waited);
    let started = match started {
        Some(r) => r,
        // abandoned before the first result: wait a little for it, then drain
        None => match tokio::time::timeout(DRAIN_BUDGET, &mut start).await {
            Ok(r) => match r {
                Ok(Ok(mut s)) => {
                    return if drain(&mut s).await { Step::AbandonedClean } else { Step::Interrupted(Interrupt::Abandoned) };
                }
                Ok(Err(_)) => return Step::AbandonedClean,
                Err(e) => return if is_fatal(&e) { Step::Broken(map_error(e, Some(sql))) } else { Step::AbandonedClean },
            },
            Err(_) => return Step::Interrupted(Interrupt::Abandoned),
        },
    };
    let mut stream = match started {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Step::Failed(e),
        Err(e) => {
            let fatal = is_fatal(&e);
            let err = map_error(e, Some(sql));
            return if fatal { Step::Broken(err) } else { Step::Failed(err) };
        }
    };
    match stream_results(&mut stream, stmt, sql, tx, cancel, abandon, clock, described, emitted).await {
        Step::Interrupted(Interrupt::Abandoned) => {
            if drain(&mut stream).await {
                Step::AbandonedClean
            } else {
                Step::Interrupted(Interrupt::Abandoned)
            }
        }
        other => other,
    }
}

/// Read and discard the rest of a response within [`DRAIN_BUDGET`]. True
/// when the response ended (the connection is in sync again).
async fn drain(stream: &mut BoxStream<'_, tiberius::Result<Item>>) -> bool {
    let deadline = Instant::now() + DRAIN_BUDGET;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(None) => return true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return !is_fatal(&e),
            Err(_) => return false,
        }
    }
}

/// Convert and forward the items of one response.
#[allow(clippy::too_many_arguments)]
async fn stream_results(
    stream: &mut BoxStream<'_, tiberius::Result<Item>>,
    stmt: &Statement,
    sql: &str,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    abandon: &CancellationToken,
    clock: &mut ServerClock,
    described: &mut Option<Vec<Described>>,
    emitted: &mut bool,
) -> Step {
    let mut in_rows = false;
    let mut types: Vec<ColumnType> = Vec::new();
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    loop {
        let next = match stream.next().now_or_never() {
            Some(item) => item,
            None => {
                if !batch.is_empty() {
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ROWS));
                    if let Err(i) = send(tx, Event::Rows(full), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
                let waited = Instant::now();
                let item = tokio::select! {
                    biased;
                    _ = abandon.cancelled() => return Step::Interrupted(Interrupt::Abandoned),
                    _ = cancel.cancelled() => return Step::Interrupted(Interrupt::Cancelled),
                    _ = clock.expired() => return Step::Interrupted(Interrupt::TimedOut),
                    _ = tx.closed() => return Step::Interrupted(Interrupt::Abandoned),
                    item = stream.next() => item,
                };
                clock.consume(waited);
                item
            }
        };
        match next {
            None => break,
            Some(Err(e)) => {
                if !batch.is_empty() {
                    let full = std::mem::take(&mut batch);
                    if let Err(i) = send(tx, Event::Rows(full), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
                let fatal = is_fatal(&e);
                let err = map_error(e, Some(sql));
                return if fatal { Step::Broken(err) } else { Step::Failed(err) };
            }
            Some(Ok(Item::Meta(cols))) => {
                if !batch.is_empty() {
                    let full = std::mem::take(&mut batch);
                    if let Err(i) = send(tx, Event::Rows(full), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
                types = cols.iter().map(|c| c.column_type()).collect();
                let mut metas: Vec<ColumnMeta> = cols.iter().map(column_meta).collect();
                if let Some(d) = described.take() {
                    apply_described(&mut metas, &d);
                }
                in_rows = true;
                *emitted = true;
                if let Err(i) = send(tx, Event::Header(ResultHeader::Rows(metas)), cancel, abandon).await {
                    return Step::Interrupted(i);
                }
            }
            Some(Ok(Item::Row(r))) => {
                let row: Row = r.into_iter().zip(types.iter()).map(|(d, t)| crate::types::in_zone(convert(d, *t), stmt.zone)).collect();
                batch.push(row);
                if batch.len() >= BATCH_ROWS {
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ROWS));
                    if let Err(i) = send(tx, Event::Rows(full), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
            }
            Some(Ok(Item::Count(n))) => {
                if !batch.is_empty() {
                    let full = std::mem::take(&mut batch);
                    if let Err(i) = send(tx, Event::Rows(full), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
                if in_rows {
                    // DONE of the SELECT that produced the rows
                    in_rows = false;
                } else {
                    *emitted = true;
                    let h = ResultHeader::Update { rows_affected: Some(n), last_insert_id: None, tag: stmt.tag.clone() };
                    if let Err(i) = send(tx, Event::Header(h), cancel, abandon).await {
                        return Step::Interrupted(i);
                    }
                }
            }
        }
    }
    if !batch.is_empty()
        && let Err(i) = send(tx, Event::Rows(batch), cancel, abandon).await
    {
        return Step::Interrupted(i);
    }
    Step::Done
}

/// `sys.dm_exec_describe_first_result_set` for the statement (column
/// sources, nullability, precision).
async fn describe(client: &mut MsClient, stmt: &Statement) -> DbResult<Vec<Described>> {
    let decls = crate::types::param_declarations(&stmt.params)?;
    let sql = "SELECT name, system_type_name, is_nullable, precision, scale, max_length, is_identity_column, \
               source_database, source_schema, source_table, source_column \
               FROM sys.dm_exec_describe_first_result_set(@P1, @P2, 1) WHERE is_hidden = 0 ORDER BY column_ordinal";
    let params = vec![Value::text(stmt.sql.clone()), if decls.is_empty() { Value::Null } else { Value::text(decls) }];
    let cmd = executesql_command(sql, &params)?;
    let rows = cmd.exec(client).await.map_err(|e| map_error(e, None))?.into_command_result().await.map_err(|e| map_error(e, None))?;
    let mut out = Vec::new();
    if let Some(rows) = rows.to_query_result(0) {
        for r in rows {
            let text = |i: usize| -> Option<String> { r.try_get::<&str, _>(i).ok().flatten().map(str::to_string) };
            let small = |i: usize| -> Option<u32> { r.try_get::<u8, _>(i).ok().flatten().map(u32::from) };
            let len: Option<i16> = r.try_get(5).ok().flatten();
            let type_name = text(1).unwrap_or_default();
            let lower = type_name.to_ascii_lowercase();
            let max_length = len.filter(|l| *l > 0).map(|l| if lower.starts_with("nchar") || lower.starts_with("nvarchar") { (l / 2) as u32 } else { l as u32 });
            let table = text(9);
            out.push(Described {
                nullable: r.try_get::<bool, _>(2).ok().flatten(),
                precision: if lower.starts_with("decimal") || lower.starts_with("numeric") { small(3) } else { None },
                scale: if lower.starts_with("decimal") || lower.starts_with("numeric") || lower.starts_with("datetime2") || lower.starts_with("time") { small(4) } else { None },
                max_length: if lower.contains("char") || lower.contains("binary") { max_length } else { None },
                identity: r.try_get::<bool, _>(6).ok().flatten(),
                source: match (&table, text(10)) {
                    (Some(t), Some(c)) => Some(ColumnSource { database: text(7), schema: text(8), table: t.clone(), column: c }),
                    _ => None,
                },
                type_name,
            });
        }
    }
    Ok(out)
}

fn apply_described(metas: &mut [ColumnMeta], described: &[Described]) {
    if metas.len() != described.len() {
        return;
    }
    for (m, d) in metas.iter_mut().zip(described) {
        if !d.type_name.is_empty() {
            m.type_name = d.type_name.clone();
            m.kind = crate::types::kind_of_type_name(&d.type_name);
        }
        m.nullable = d.nullable.or(m.nullable);
        m.precision = d.precision.or(m.precision);
        m.scale = d.scale.or(m.scale);
        m.max_length = d.max_length.or(m.max_length);
        m.auto_increment = d.identity;
        m.source = d.source.clone();
    }
}

// ------------------------------------------------------------------ cursor

/// Cursor over the events of one statement.
pub(crate) struct ChannelCursor {
    rx: mpsc::Receiver<Event>,
    in_rows: bool,
    buffer: VecDeque<Row>,
    pending_header: Option<ResultHeader>,
    pending_error: Option<DbError>,
    finished: bool,
    messages: Vec<ServerMessage>,
    on_close: Option<Box<dyn FnOnce() + Send>>,
    /// The session's open cursor id, and this cursor's: cleared only while it is still this one
    /// (an older cursor dropped late must not clear a newer cursor's flag).
    open_flag: Arc<AtomicU64>,
    id: u64,
}

impl ChannelCursor {
    pub(crate) fn new(rx: mpsc::Receiver<Event>, on_close: Option<Box<dyn FnOnce() + Send>>, open_flag: Arc<AtomicU64>) -> Self {
        let id = open_flag.load(Ordering::SeqCst);
        ChannelCursor { rx, in_rows: false, buffer: VecDeque::new(), pending_header: None, pending_error: None, finished: false, messages: Vec::new(), on_close, open_flag, id }
    }

    fn clear_open_flag(&self) {
        let _ = self.open_flag.compare_exchange(self.id, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.rx.recv().await {
                Some(Event::Message(m)) => self.messages.push(m),
                Some(ev) => return Some(ev),
                None => {
                    self.finished = true;
                    self.clear_open_flag();
                    return None;
                }
            }
        }
    }
}

#[async_trait]
impl QueryCursor for ChannelCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        self.buffer.clear();
        while self.in_rows {
            match self.recv().await {
                Some(Event::Rows(_)) | Some(Event::Message(_)) => {}
                Some(Event::Header(h)) => {
                    self.pending_header = Some(h);
                    self.in_rows = false;
                }
                Some(Event::Error(e)) => {
                    self.pending_error = Some(e);
                    self.in_rows = false;
                }
                None => self.in_rows = false,
            }
        }
        if let Some(e) = self.pending_error.take() {
            self.pending_header = None;
            return Err(e);
        }
        if let Some(h) = self.pending_header.take() {
            self.in_rows = matches!(h, ResultHeader::Rows(_));
            return Ok(Some(h));
        }
        loop {
            match self.recv().await {
                Some(Event::Header(h)) => {
                    self.in_rows = matches!(h, ResultHeader::Rows(_));
                    return Ok(Some(h));
                }
                Some(Event::Rows(_)) | Some(Event::Message(_)) => {}
                Some(Event::Error(e)) => return Err(e),
                None => return Ok(None),
            }
        }
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let max = max_rows.max(1);
        while self.buffer.len() < max && self.in_rows {
            match self.recv().await {
                Some(Event::Rows(r)) => self.buffer.extend(r),
                Some(Event::Header(h)) => {
                    self.pending_header = Some(h);
                    self.in_rows = false;
                }
                Some(Event::Error(e)) => {
                    self.pending_error = Some(e);
                    self.in_rows = false;
                }
                Some(Event::Message(_)) => {}
                None => self.in_rows = false,
            }
        }
        if self.buffer.is_empty() {
            if let Some(e) = self.pending_error.take() {
                return Err(e);
            }
            return Ok(Vec::new());
        }
        let n = max.min(self.buffer.len());
        Ok(self.buffer.drain(..n).collect())
    }

    async fn close(&mut self) -> DbResult<()> {
        if let Some(f) = self.on_close.take()
            && !self.finished
        {
            f();
        }
        self.buffer.clear();
        self.in_rows = false;
        self.pending_header = None;
        self.pending_error = None;
        while self.recv().await.is_some() {}
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.messages)
    }
}

impl Drop for ChannelCursor {
    fn drop(&mut self) {
        if let Some(f) = self.on_close.take()
            && !self.finished
        {
            f();
        }
        self.clear_open_flag();
    }
}

/// `e`'s position moved by `chars` (the batch's start in the script).
fn shift_position(mut e: DbError, chars: usize) -> DbError {
    if let Some(p) = e.position.as_mut() {
        *p += chars;
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::value::DataKind;

    fn cursor_with(events: Vec<Event>) -> ChannelCursor {
        let (tx, rx) = mpsc::channel(64);
        for e in events {
            tx.try_send(e).unwrap();
        }
        drop(tx);
        ChannelCursor::new(rx, None, Arc::new(AtomicU64::new(1)))
    }

    #[tokio::test]
    async fn protocol() {
        let mut c = cursor_with(vec![
            Event::Header(ResultHeader::Rows(vec![ColumnMeta::new("a", "int", DataKind::Int)])),
            Event::Rows((0..5).map(|i| vec![Value::Int(i)]).collect()),
            Event::Header(ResultHeader::Update { rows_affected: Some(3), last_insert_id: None, tag: None }),
            Event::Error(DbError::query("late")),
        ]);
        assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        assert_eq!(c.fetch(2).await.unwrap().len(), 2);
        assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Update { rows_affected: Some(3), .. })));
        assert_eq!(c.next_result().await.unwrap_err().message, "late");
        assert!(c.next_result().await.unwrap().is_none());
    }

    #[test]
    fn described_applied() {
        let mut metas = vec![ColumnMeta::new("a", "int", DataKind::Int), ColumnMeta::new("b", "nvarchar", DataKind::Text)];
        let d = vec![
            Described { type_name: "int".into(), nullable: Some(false), identity: Some(true), ..Default::default() },
            Described {
                type_name: "nvarchar(50)".into(),
                nullable: Some(true),
                max_length: Some(50),
                source: Some(ColumnSource { database: Some("db".into()), schema: Some("dbo".into()), table: "t".into(), column: "b".into() }),
                ..Default::default()
            },
        ];
        apply_described(&mut metas, &d);
        assert_eq!(metas[0].auto_increment, Some(true));
        assert_eq!(metas[1].type_name, "nvarchar(50)");
        assert_eq!(metas[1].max_length, Some(50));
        assert_eq!(metas[1].source.as_ref().unwrap().table, "t");
        // mismatched lengths leave metadata untouched
        let mut one = vec![ColumnMeta::new("a", "int", DataKind::Int)];
        apply_described(&mut one, &d);
        assert_eq!(one[0].auto_increment, None);
    }

    #[test]
    fn shared_state() {
        let s = Shared::default();
        s.set_db(Some("x".into()));
        assert_eq!(s.db().as_deref(), Some("x"));
        let t = CancellationToken::new();
        s.set_active(Some(t.clone()));
        s.cancel_active();
        assert!(t.is_cancelled());
    }
}
