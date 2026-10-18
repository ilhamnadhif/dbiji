//! Streaming result cursor.
//!
//! `mysql_async`'s `QueryResult` borrows the connection, while DBiji cursors
//! must be `'static`. [`spawn`] therefore moves the connection into a
//! background task (the *worker*) that executes the statement and answers
//! fetch requests from the [`MySqlCursor`] over a channel. Rows are read from
//! the socket only when the cursor asks for them, so memory stays bounded by
//! the requested batch size (TCP flow control provides the backpressure).
//!
//! When the statement is complete — or the cursor is closed / dropped — the
//! worker hands the connection back to the session through a oneshot channel
//! ([`ConnReturn`]); session operations wait for it. A cursor dropped with
//! unread rows first drains briefly (cheap for small remainders), then sends
//! `KILL QUERY` from a side connection and drains the rest, so the connection
//! becomes usable again quickly and never hangs.

use crate::connect::Killer;
use crate::convert::{self, Decode};
use crate::error::{self, codes};
use crate::sqltext;
use async_trait::async_trait;
use dbiji_core::{DbError, DbResult, ErrorKind, QueryCursor, ResultHeader, Row, ServerMessage};
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Conn, Params, QueryResult, Value as MyValue};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until};

/// How long an abandoned result is drained before `KILL QUERY` is sent.
const SOFT_DRAIN: Duration = Duration::from_millis(150);
/// After this time an abandoned result that still streams is given up and
/// the connection is closed.
const HARD_DRAIN: Duration = Duration::from_secs(30);
/// Maximum number of warnings collected with `SHOW WARNINGS`.
const MAX_WARNINGS: usize = 1000;

/// A statement to execute.
pub(crate) struct Job {
    /// Statement text encoded in the connection character set.
    pub sql: Vec<u8>,
    /// Original text, for error positions and statement classification.
    pub sql_text: String,
    /// Bound parameters (binary protocol) or `None` (text protocol).
    pub params: Option<Vec<MyValue>>,
    /// Client side statement timeout.
    pub timeout: Option<Duration>,
    /// Command tag for a single statement whose OK packet has no info text.
    pub tag: Option<String>,
}

/// Returned to the session when the worker releases the connection.
pub(crate) struct ConnReturn {
    /// The connection, `None` when it is broken.
    pub conn: Option<Conn>,
    /// Why the connection is unusable.
    pub broken: Option<DbError>,
}

/// Transaction state shared between the session and its workers, updated
/// as soon as a statement completes (so the synchronous
/// `Session::in_transaction` is current even before the session gets the
/// connection back).
pub(crate) type TxState = Arc<AtomicBool>;

struct Reply<T> {
    result: DbResult<T>,
    messages: Vec<ServerMessage>,
    /// The statement is complete; the cursor answers further calls locally.
    finished: bool,
}

enum Req {
    NextResult(oneshot::Sender<Reply<Option<ResultHeader>>>),
    Fetch(usize, oneshot::Sender<Reply<Vec<Row>>>),
    Close(oneshot::Sender<Reply<()>>),
}

/// Start executing `job` on `conn` in a background task.
///
/// `abort` lets the session interrupt the worker (used by `Session::close`).
pub(crate) fn spawn(
    conn: Conn,
    job: Job,
    killer: Arc<Killer>,
    abort: watch::Receiver<bool>,
    tx_state: TxState,
) -> (MySqlCursor, oneshot::Receiver<ConnReturn>) {
    let (req_tx, req_rx) = mpsc::channel(1);
    let (alive_tx, alive_rx) = watch::channel(());
    let (ret_tx, ret_rx) = oneshot::channel();
    let deadline = job.timeout.map(|t| Instant::now() + t);
    let watch = Watch {
        alive: alive_rx,
        abort,
        killer,
        deadline,
        timed_out: false,
        abandoned: false,
        killed: false,
    };
    tokio::spawn(run(conn, job, req_rx, watch, ret_tx, tx_state));
    (
        MySqlCursor {
            tx: req_tx,
            _alive: alive_tx,
            finished: false,
            in_flight: false,
            messages: Vec::new(),
        },
        ret_rx,
    )
}

/// Interruption state of a running statement.
struct Watch {
    /// Resolves with an error when the cursor is dropped.
    alive: watch::Receiver<()>,
    /// `true` is sent when the session wants the connection back.
    abort: watch::Receiver<bool>,
    killer: Arc<Killer>,
    deadline: Option<Instant>,
    timed_out: bool,
    abandoned: bool,
    killed: bool,
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86_400 * 365)
}

impl Watch {
    async fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        if let Err(e) = self.killer.kill_query().await {
            tracing::debug!(
                "KILL QUERY for connection {} failed: {e}",
                self.killer.connection_id()
            );
        }
    }

    fn abort_requested(&self) -> bool {
        *self.abort.borrow()
    }

    /// Drive a server operation to completion (it is never dropped half-way,
    /// which would desynchronise the protocol), sending `KILL QUERY` when the
    /// timeout elapses or the cursor goes away.
    async fn guard<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        let mut abort_open = true;
        loop {
            let deadline = if self.killed { None } else { self.deadline };
            tokio::select! {
                biased;
                out = &mut fut => return out,
                _ = sleep_until(deadline.unwrap_or_else(far_future)), if deadline.is_some() => {
                    self.timed_out = true;
                    self.kill().await;
                }
                res = self.alive.changed(), if !self.abandoned => {
                    if res.is_err() {
                        self.abandoned = true;
                        self.kill().await;
                    }
                }
                res = self.abort.changed(), if abort_open && !self.abandoned => {
                    match res {
                        Ok(()) if self.abort_requested() => {
                            self.abandoned = true;
                            self.kill().await;
                        }
                        Ok(()) => {}
                        Err(_) => abort_open = false,
                    }
                }
            }
        }
    }

    /// Like [`guard`](Self::guard) for draining an abandoned result: kill after
    /// `soft`, give up (returning `None`) after `hard`.
    async fn drain<F: Future>(
        &mut self,
        fut: F,
        soft: Instant,
        hard: Instant,
    ) -> Option<F::Output> {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                out = &mut fut => return Some(out),
                _ = sleep_until(soft), if !self.killed => self.kill().await,
                _ = sleep_until(hard) => return None,
            }
        }
    }

    /// Wait for the next request; `None` when the cursor is gone or the
    /// session aborts.
    async fn next_request(&mut self, rx: &mut mpsc::Receiver<Req>) -> Option<Req> {
        if self.abort_requested() {
            return None;
        }
        let mut abort_open = true;
        loop {
            tokio::select! {
                biased;
                req = rx.recv() => return req,
                res = self.abort.changed(), if abort_open => match res {
                    Ok(()) if self.abort_requested() => return None,
                    Ok(()) => {}
                    Err(_) => abort_open = false,
                },
            }
        }
    }
}

/// A reply that is sent once the statement is complete.
enum Pending {
    Next(
        oneshot::Sender<Reply<Option<ResultHeader>>>,
        DbResult<Option<ResultHeader>>,
    ),
    Fetch(oneshot::Sender<Reply<Vec<Row>>>, DbResult<Vec<Row>>),
    Close(oneshot::Sender<Reply<()>>, DbResult<()>),
}

impl Pending {
    fn send(self, messages: Vec<ServerMessage>) {
        match self {
            Pending::Next(tx, result) => {
                let _ = tx.send(Reply {
                    result,
                    messages,
                    finished: true,
                });
            }
            Pending::Fetch(tx, result) => {
                let _ = tx.send(Reply {
                    result,
                    messages,
                    finished: true,
                });
            }
            Pending::Close(tx, result) => {
                let _ = tx.send(Reply {
                    result,
                    messages,
                    finished: true,
                });
            }
        }
    }

    fn with_error(self, e: DbError) -> Pending {
        match self {
            Pending::Next(tx, _) => Pending::Next(tx, Err(e)),
            Pending::Fetch(tx, _) => Pending::Fetch(tx, Err(e)),
            Pending::Close(tx, _) => Pending::Close(tx, Err(e)),
        }
    }
}

/// How the statement ended.
#[derive(Default)]
struct Outcome {
    pending: Option<Pending>,
    /// Highest warning count reported by the server.
    warnings: u32,
    /// The connection is unusable.
    broken: Option<DbError>,
    /// The cursor went away; results were discarded.
    abandoned: bool,
    /// Server error number of a failure.
    error_code: Option<u16>,
}

impl Outcome {
    fn abandoned() -> Outcome {
        Outcome {
            abandoned: true,
            ..Default::default()
        }
    }
}

/// Error conversion with timeout awareness.
fn convert_error(e: mysql_async::Error, w: &Watch, job: &Job) -> (DbError, Option<u16>, bool) {
    let code = error::server_code(&e);
    let fatal = error::is_fatal(&e);
    let mut err = error::map_error(e, Some(&job.sql_text));
    if w.timed_out && matches!(code, Some(codes::QUERY_INTERRUPTED | codes::QUERY_TIMEOUT)) {
        let secs = job.timeout.map(|t| t.as_secs_f64()).unwrap_or_default();
        err = DbError::timeout(format!(
            "The statement exceeded the timeout of {secs} s and was cancelled"
        ))
        .with_code(code.map(|c| c.to_string()).unwrap_or_default());
    }
    (err, code, fatal)
}

fn failure(
    e: mysql_async::Error,
    w: &Watch,
    job: &Job,
    pending: Pending,
    warnings: u32,
) -> Outcome {
    let (err, code, fatal) = convert_error(e, w, job);
    Outcome {
        broken: if fatal { Some(err.clone()) } else { None },
        pending: Some(pending.with_error(err)),
        warnings,
        abandoned: false,
        error_code: code,
    }
}

async fn run(
    mut conn: Conn,
    mut job: Job,
    mut rx: mpsc::Receiver<Req>,
    mut w: Watch,
    ret: oneshot::Sender<ConnReturn>,
    tx_state: TxState,
) {
    let outcome = match job.params.take() {
        None => {
            let res = w.guard(conn.query_iter(job.sql.as_slice())).await;
            match res {
                Ok(qr) => serve(qr, &mut rx, &mut w, &job).await,
                Err(e) => first_failure(e, &mut rx, &mut w, &job).await,
            }
        }
        Some(params) => {
            if params.len() > u16::MAX as usize {
                let err = DbError::query(format!(
                    "Too many parameters ({}); MySQL supports at most 65535",
                    params.len()
                ));
                client_failure(err, &mut rx, &mut w).await
            } else {
                let stmt = w.guard(conn.prep(job.sql.as_slice())).await;
                match stmt {
                    Err(e) => first_failure(e, &mut rx, &mut w, &job).await,
                    Ok(stmt) if stmt.num_params() as usize != params.len() => {
                        let err = DbError::query(format!(
                            "The statement expects {} parameter(s) but {} were supplied",
                            stmt.num_params(),
                            params.len()
                        ));
                        client_failure(err, &mut rx, &mut w).await
                    }
                    Ok(stmt) => {
                        let res = w
                            .guard(conn.exec_iter(stmt, Params::Positional(params)))
                            .await;
                        match res {
                            Ok(qr) => serve(qr, &mut rx, &mut w, &job).await,
                            Err(e) => first_failure(e, &mut rx, &mut w, &job).await,
                        }
                    }
                }
            }
        }
    };
    finish(conn, outcome, &job, ret, &tx_state).await;
}

/// The statement failed before producing a result: report the error to the
/// first request.
async fn first_failure(
    e: mysql_async::Error,
    rx: &mut mpsc::Receiver<Req>,
    w: &mut Watch,
    job: &Job,
) -> Outcome {
    let (err, code, fatal) = convert_error(e, w, job);
    let broken = if fatal { Some(err.clone()) } else { None };
    if w.abandoned {
        return Outcome {
            broken,
            abandoned: true,
            error_code: code,
            ..Default::default()
        };
    }
    let pending = match w.next_request(rx).await {
        Some(Req::NextResult(tx)) => Some(Pending::Next(tx, Err(err))),
        Some(Req::Fetch(_, tx)) => Some(Pending::Fetch(tx, Err(err))),
        Some(Req::Close(tx)) => Some(Pending::Close(tx, Ok(()))),
        None => None,
    };
    Outcome {
        pending,
        broken,
        abandoned: false,
        error_code: code,
        warnings: 0,
    }
}

/// A client side check failed before anything was sent.
async fn client_failure(err: DbError, rx: &mut mpsc::Receiver<Req>, w: &mut Watch) -> Outcome {
    let pending = match w.next_request(rx).await {
        Some(Req::NextResult(tx)) => Some(Pending::Next(tx, Err(err))),
        Some(Req::Fetch(_, tx)) => Some(Pending::Fetch(tx, Err(err))),
        Some(Req::Close(tx)) => Some(Pending::Close(tx, Ok(()))),
        None => None,
    };
    Outcome {
        pending,
        ..Default::default()
    }
}

/// Position of the worker inside the statement's results.
enum Pos {
    /// At a result whose header has not been reported.
    AtHeader,
    /// Inside a row result.
    InRows(Vec<Decode>),
    /// An update result was reported; it is consumed by the next `next_result`.
    AtUpdate,
    /// An error that follows rows already delivered.
    Failed(mysql_async::Error),
}

fn send<T>(tx: oneshot::Sender<Reply<T>>, result: DbResult<T>) {
    let _ = tx.send(Reply {
        result,
        messages: Vec::new(),
        finished: false,
    });
}

async fn serve<P: Protocol>(
    mut qr: QueryResult<'_, 'static, P>,
    rx: &mut mpsc::Receiver<Req>,
    w: &mut Watch,
    job: &Job,
) -> Outcome {
    let mut pos = Pos::AtHeader;
    let mut default_tag = job.tag.clone();
    let mut warnings: u32 = 0;
    if w.abandoned {
        return abandon(qr, w).await;
    }
    loop {
        let Some(req) = w.next_request(rx).await else {
            return abandon(qr, w).await;
        };
        match req {
            Req::NextResult(tx) => {
                // Move past the current result.
                match std::mem::replace(&mut pos, Pos::AtHeader) {
                    Pos::InRows(_) => loop {
                        let r = w.guard(qr.next()).await;
                        if w.abandoned {
                            return abandon(qr, w).await;
                        }
                        match r {
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(e) => {
                                return failure(e, w, job, Pending::Next(tx, Ok(None)), warnings);
                            }
                        }
                    },
                    Pos::AtUpdate => {
                        let r = w.guard(qr.next()).await;
                        if w.abandoned {
                            return abandon(qr, w).await;
                        }
                        if let Err(e) = r {
                            return failure(e, w, job, Pending::Next(tx, Ok(None)), warnings);
                        }
                    }
                    Pos::Failed(e) => {
                        return failure(e, w, job, Pending::Next(tx, Ok(None)), warnings);
                    }
                    Pos::AtHeader => {}
                }
                warnings = warnings.max(qr.warnings() as u32);
                match qr.columns() {
                    Some(cols) if !cols.is_empty() => {
                        let (metas, decoders): (Vec<_>, Vec<_>) =
                            cols.iter().map(convert::column_meta).unzip();
                        pos = Pos::InRows(decoders);
                        send(tx, Ok(Some(ResultHeader::Rows(metas))));
                    }
                    Some(_) => {
                        let info = qr.info();
                        let header = ResultHeader::Update {
                            rows_affected: Some(qr.affected_rows()),
                            last_insert_id: qr.last_insert_id(),
                            tag: if info.is_empty() {
                                default_tag.take()
                            } else {
                                Some(info.into_owned())
                            },
                        };
                        if qr.is_empty() {
                            // Last result: consuming it needs no I/O.
                            let _ = qr.next().await;
                            return Outcome {
                                pending: Some(Pending::Next(tx, Ok(Some(header)))),
                                warnings,
                                ..Default::default()
                            };
                        }
                        pos = Pos::AtUpdate;
                        send(tx, Ok(Some(header)));
                    }
                    None => {
                        // Either the statement is complete or an error of a
                        // following statement is pending; neither needs I/O.
                        return match qr.next().await {
                            Err(e) => failure(e, w, job, Pending::Next(tx, Ok(None)), warnings),
                            Ok(_) => Outcome {
                                pending: Some(Pending::Next(tx, Ok(None))),
                                warnings,
                                ..Default::default()
                            },
                        };
                    }
                }
            }
            Req::Fetch(max, tx) => {
                let decoders = match &pos {
                    Pos::InRows(d) => d.clone(),
                    Pos::Failed(_) => {
                        if let Pos::Failed(e) = std::mem::replace(&mut pos, Pos::AtHeader) {
                            return failure(
                                e,
                                w,
                                job,
                                Pending::Fetch(tx, Ok(Vec::new())),
                                warnings,
                            );
                        }
                        continue;
                    }
                    _ => {
                        send(tx, Ok(Vec::new()));
                        continue;
                    }
                };
                let mut rows: Vec<Row> = Vec::with_capacity(max.clamp(1, 4096));
                let mut ended = false;
                while rows.len() < max {
                    let r = w.guard(qr.next()).await;
                    if w.abandoned {
                        return abandon(qr, w).await;
                    }
                    match r {
                        Ok(Some(row)) => {
                            rows.push(convert::convert_row(&decoders, row.unwrap_raw()))
                        }
                        Ok(None) => {
                            ended = true;
                            break;
                        }
                        Err(e) => {
                            if rows.is_empty() {
                                return failure(
                                    e,
                                    w,
                                    job,
                                    Pending::Fetch(tx, Ok(Vec::new())),
                                    warnings,
                                );
                            }
                            // Deliver what was read; report the error next time.
                            pos = Pos::Failed(e);
                            break;
                        }
                    }
                }
                if !ended {
                    send(tx, Ok(rows));
                    continue;
                }
                warnings = warnings.max(qr.warnings() as u32);
                if qr.columns().is_some() {
                    pos = Pos::AtHeader;
                    send(tx, Ok(rows));
                    continue;
                }
                match qr.next().await {
                    Err(e) => {
                        pos = Pos::Failed(e);
                        send(tx, Ok(rows));
                    }
                    Ok(_) => {
                        return Outcome {
                            pending: Some(Pending::Fetch(tx, Ok(rows))),
                            warnings,
                            ..Default::default()
                        };
                    }
                }
            }
            Req::Close(tx) => {
                let mut out = abandon(qr, w).await;
                out.pending = Some(Pending::Close(
                    tx,
                    match &out.broken {
                        Some(e) => Err(e.clone()),
                        None => Ok(()),
                    },
                ));
                return out;
            }
        }
    }
}

/// Discard everything that is left of the statement.
async fn abandon<P: Protocol>(mut qr: QueryResult<'_, 'static, P>, w: &mut Watch) -> Outcome {
    w.abandoned = true;
    let soft = Instant::now() + SOFT_DRAIN;
    let hard = soft + HARD_DRAIN;
    loop {
        if qr.is_empty() {
            // Consume a pending, already received result (no I/O).
            let _ = qr.next().await;
            if qr.columns().is_none() {
                break;
            }
            continue;
        }
        match w.drain(qr.next(), soft, hard).await {
            Some(Ok(_)) => {}
            Some(Err(e)) if error::is_fatal(&e) => {
                return Outcome {
                    broken: Some(error::err(e)),
                    abandoned: true,
                    ..Default::default()
                };
            }
            Some(Err(_)) => {}
            None => {
                return Outcome {
                    broken: Some(DbError::connection(
                        "The connection was closed because an abandoned result could not be discarded in time",
                    )),
                    abandoned: true,
                    ..Default::default()
                };
            }
        }
    }
    Outcome::abandoned()
}

async fn show_warnings(conn: &mut Conn) -> Vec<ServerMessage> {
    let rows: Vec<mysql_async::Row> = match conn.query("SHOW WARNINGS").await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("SHOW WARNINGS failed: {e}");
            return Vec::new();
        }
    };
    rows.iter()
        .take(MAX_WARNINGS)
        .map(|r| {
            let level = convert::read::text_or_empty(r, 0);
            let code = convert::read::text(r, 1);
            let message = convert::read::text_or_empty(r, 2);
            let mut m = ServerMessage::new(level.to_ascii_uppercase(), message);
            m.code = code;
            m
        })
        .collect()
}

async fn finish(
    mut conn: Conn,
    outcome: Outcome,
    job: &Job,
    ret: oneshot::Sender<ConnReturn>,
    tx_state: &TxState,
) {
    let broken = outcome.broken.or_else(|| {
        conn.is_disconnected()
            .then(|| DbError::connection("The connection to the server was lost"))
    });
    let mut messages = Vec::new();
    if broken.is_none() && !outcome.abandoned && outcome.warnings > 0 && outcome.pending.is_some() {
        messages = show_warnings(&mut conn).await;
    }
    let in_transaction = if broken.is_some() {
        Some(false)
    } else {
        conn.last_ok_packet().map(|ok| {
            ok.status_flags()
                .contains(StatusFlags::SERVER_STATUS_IN_TRANS)
        })
    };
    match in_transaction {
        Some(v) => tx_state.store(v, Ordering::Release),
        None => {
            // After an error the server sends no status flags: a deadlock
            // rolls back the whole transaction, DDL commits implicitly.
            if outcome.error_code == Some(codes::DEADLOCK)
                || sqltext::causes_implicit_commit(&job.sql_text)
            {
                tx_state.store(false, Ordering::Release);
            }
        }
    }
    let _ = ret.send(ConnReturn {
        conn: if broken.is_some() { None } else { Some(conn) },
        broken: broken.clone(),
    });
    if let Some(p) = outcome.pending {
        p.send(messages);
    }
}

/// Cursor over the results of one statement (see the module docs).
pub struct MySqlCursor {
    tx: mpsc::Sender<Req>,
    /// Dropping the cursor drops this sender, which tells the worker that
    /// nobody reads the results any more.
    _alive: watch::Sender<()>,
    finished: bool,
    in_flight: bool,
    messages: Vec<ServerMessage>,
}

impl MySqlCursor {
    async fn call<T>(
        &mut self,
        make: impl FnOnce(oneshot::Sender<Reply<T>>) -> Req,
    ) -> DbResult<T> {
        if self.in_flight {
            return Err(DbError::new(
                ErrorKind::Internal,
                "A previous operation on this cursor was cancelled before it completed; close the cursor",
            ));
        }
        let (tx, rx) = oneshot::channel();
        self.in_flight = true;
        if self.tx.send(make(tx)).await.is_err() {
            self.in_flight = false;
            self.finished = true;
            return Err(DbError::internal("The statement worker ended unexpectedly"));
        }
        let reply = rx.await;
        self.in_flight = false;
        match reply {
            Ok(r) => {
                self.messages.extend(r.messages);
                if r.finished {
                    self.finished = true;
                }
                r.result
            }
            Err(_) => {
                self.finished = true;
                Err(DbError::internal("The statement worker ended unexpectedly"))
            }
        }
    }
}

#[async_trait]
impl QueryCursor for MySqlCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        if self.finished {
            return Ok(None);
        }
        self.call(Req::NextResult).await
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        if self.finished || max_rows == 0 {
            return Ok(Vec::new());
        }
        self.call(|tx| Req::Fetch(max_rows, tx)).await
    }

    async fn close(&mut self) -> DbResult<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Req::Close(tx)).await.is_err() {
            return Ok(());
        }
        match rx.await {
            Ok(r) => {
                self.messages.extend(r.messages);
                r.result
            }
            Err(_) => Ok(()),
        }
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.messages)
    }
}
