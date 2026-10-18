//! Streaming [`QueryCursor`] implementation.
//!
//! Rows are read from the tokio-postgres response stream on demand. The
//! connection task delivers server messages through a bounded channel, so
//! when the application stops fetching, the connection stops reading from
//! the socket and the server blocks: memory stays bounded no matter how big
//! the result is.
//!
//! The cursor holds the session's gate (a mutex guard) until the statement
//! has completed on the server. Closing or dropping it early sends a cancel
//! request (only when the statement is still running) and drains the
//! remaining messages — in a background task when dropped — before the gate
//! is released, so a late cancel can never hit the next statement.

use crate::connect::{PgCancelHandle, SessionShared};
use crate::decode::{binary_value, text_value};
use crate::error::map_error;
use crate::sqltext::{ScriptInfo, StmtInfo, TxEffect};
use crate::types::Conv;
use async_trait::async_trait;
use bytes::Bytes;
use dbiji_core::{ColumnMeta, DataKind, DbError, DbResult, QueryCursor, ResultHeader, Row, ServerMessage, Value};
use futures::StreamExt;
use postgres_types::{FromSql, Type};
use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::OwnedMutexGuard;
use tokio_postgres::{CopyOutStream, RowStream, SimpleQueryMessage, SimpleQueryStream};

/// Time spent draining buffered messages before a cancel request is sent.
const QUICK_DRAIN: Duration = Duration::from_millis(25);
/// Upper bound for draining after a cancel request.
const FULL_DRAIN: Duration = Duration::from_secs(60);

/// Raw access to a binary column value.
pub(crate) struct Raw<'a>(pub Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

const TIMER_ARMED: u8 = 0;
const TIMER_FIRED: u8 = 1;
const TIMER_DONE: u8 = 2;

/// Client side statement timeout: cancels the statement when it fires.
///
/// The state moves from armed to either fired (the timer won and sends a
/// cancel request) or done (the statement won); exactly one side wins, so a
/// cancel request can never be sent after the statement was declared done.
pub(crate) struct StatementTimer {
    handle: Option<tokio::task::JoinHandle<()>>,
    state: Arc<AtomicU8>,
    timeout: Duration,
}

impl StatementTimer {
    pub(crate) fn start(rt: &Handle, timeout: Duration, cancel: Arc<PgCancelHandle>) -> StatementTimer {
        let state = Arc::new(AtomicU8::new(TIMER_ARMED));
        let st = state.clone();
        let handle = rt.spawn(async move {
            tokio::time::sleep(timeout).await;
            if st.compare_exchange(TIMER_ARMED, TIMER_FIRED, Ordering::AcqRel, Ordering::Acquire).is_ok()
                && let Err(e) = cancel.send().await
            {
                tracing::warn!("statement timeout: cancel request failed: {e}");
            }
        });
        StatementTimer { handle: Some(handle), state, timeout }
    }

    /// The timeout elapsed and a cancel request was (or is being) sent.
    pub(crate) fn has_fired(&self) -> bool {
        self.state.load(Ordering::Acquire) == TIMER_FIRED
    }

    /// The statement no longer needs the timeout (first row received).
    fn disarm(&mut self) {
        if self.state.compare_exchange(TIMER_ARMED, TIMER_DONE, Ordering::AcqRel, Ordering::Acquire).is_ok()
            && let Some(h) = self.handle.take()
        {
            h.abort();
        }
    }

    /// Stop the timer. When it already fired, returns the task sending the
    /// cancel request: the session must not be reused before it finished.
    pub(crate) fn stop(mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.disarm();
        if self.has_fired() { self.handle.take() } else { None }
    }
}

impl Drop for StatementTimer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take()
            && !self.has_fired()
        {
            h.abort();
        }
    }
}

/// Column metadata plus the conversion used for its values.
#[derive(Debug, Clone)]
pub(crate) struct ColumnPlan {
    pub meta: ColumnMeta,
    pub conv: Conv,
}

pub(crate) struct SimpleBody {
    pub stream: Pin<Box<SimpleQueryStream>>,
    pub script: ScriptInfo,
    pub stmt_index: usize,
    /// Metadata of the first row result obtained from `Parse`/`Describe`.
    pub prepared: Option<Vec<ColumnPlan>>,
    pub convs: Vec<Conv>,
    pub results_seen: usize,
}

pub(crate) struct ExtendedBody {
    pub stream: Pin<Box<RowStream>>,
    pub columns: Option<Vec<ColumnPlan>>,
    pub info: StmtInfo,
    pub header_done: bool,
}

pub(crate) struct CopyBody {
    pub stream: Pin<Box<CopyOutStream>>,
    pub header_done: bool,
}

pub(crate) enum Body {
    Simple(SimpleBody),
    Extended(ExtendedBody),
    Copy(CopyBody),
    Empty,
}

/// Apply the transaction effect of a statement that completed successfully.
fn apply_success(shared: &SessionShared, info: Option<&StmtInfo>) {
    match info.map(|i| i.tx) {
        Some(TxEffect::Begin) => shared.set_in_tx(true),
        Some(TxEffect::Commit { chain }) | Some(TxEffect::Rollback { chain }) => shared.set_in_tx(chain),
        Some(TxEffect::PrepareTransaction) => shared.set_in_tx(false),
        _ => {}
    }
}

/// Apply the transaction effect of a statement that failed. A failed
/// statement inside a transaction leaves it open (aborted); a failed
/// COMMIT / ROLLBACK / PREPARE TRANSACTION ends it.
fn apply_failure(shared: &SessionShared, info: Option<&StmtInfo>) {
    if let Some(TxEffect::Commit { .. }) | Some(TxEffect::Rollback { .. }) | Some(TxEffect::PrepareTransaction) =
        info.map(|i| i.tx)
    {
        shared.set_in_tx(false);
    }
}

impl Body {
    /// Read and discard everything until the request is complete, keeping
    /// the transaction state up to date. Cancel safe.
    async fn drain(&mut self, shared: &SessionShared) {
        match self {
            Body::Simple(b) => {
                while let Some(m) = b.stream.next().await {
                    match m {
                        Ok(SimpleQueryMessage::CommandComplete(_)) => {
                            apply_success(shared, b.script.statements.get(b.stmt_index));
                            b.stmt_index += 1;
                        }
                        Ok(_) => {}
                        Err(_) => apply_failure(shared, b.script.statements.get(b.stmt_index)),
                    }
                }
            }
            Body::Extended(b) => {
                let mut failed = false;
                while let Some(r) = b.stream.next().await {
                    if r.is_err() {
                        failed = true;
                    }
                }
                if failed {
                    apply_failure(shared, Some(&b.info));
                } else {
                    apply_success(shared, Some(&b.info));
                }
            }
            Body::Copy(b) => while b.stream.next().await.is_some() {},
            Body::Empty => {}
        }
        *self = Body::Empty;
    }
}

/// Finish an abandoned statement: drain briefly, cancel if it is still
/// running, then drain completely.
async fn abandon(mut body: Body, shared: Arc<SessionShared>, cancel: Arc<PgCancelHandle>) {
    if tokio::time::timeout(QUICK_DRAIN, body.drain(&shared)).await.is_ok() {
        return;
    }
    if let Err(e) = cancel.send().await {
        tracing::warn!("cancel request for an abandoned statement failed: {e}");
    }
    if tokio::time::timeout(FULL_DRAIN, body.drain(&shared)).await.is_err() {
        tracing::warn!("statement did not finish after cancel; abandoning its remaining results");
    }
}

/// Streaming cursor over the results of one `execute` call.
pub(crate) struct PgCursor {
    body: Body,
    shared: Arc<SessionShared>,
    guard: Option<OwnedMutexGuard<()>>,
    cancel: Arc<PgCancelHandle>,
    rt: Handle,
    timer: Option<StatementTimer>,
    pending_error: Option<DbError>,
    finished: bool,
    in_rows: bool,
}

impl PgCursor {
    pub(crate) fn new(
        body: Body,
        shared: Arc<SessionShared>,
        guard: OwnedMutexGuard<()>,
        cancel: Arc<PgCancelHandle>,
        timer: Option<StatementTimer>,
        rt: Handle,
    ) -> PgCursor {
        shared.cursor_open.store(true, Ordering::Release);
        let finished = matches!(body, Body::Empty);
        let mut c = PgCursor {
            body,
            shared,
            guard: Some(guard),
            cancel,
            rt,
            timer,
            pending_error: None,
            finished: false,
            in_rows: false,
        };
        if finished {
            c.complete();
        }
        c
    }

    /// The statement is complete on the server: release the session (after
    /// a cancel request sent by the timeout, if any, has been processed).
    fn complete(&mut self) {
        self.finished = true;
        self.in_rows = false;
        self.body = Body::Empty;
        self.shared.cursor_open.store(false, Ordering::Release);
        let guard = self.guard.take();
        match self.timer.take().and_then(StatementTimer::stop) {
            Some(pending_cancel) => {
                self.rt.spawn(async move {
                    let _ = pending_cancel.await;
                    drop(guard);
                });
            }
            None => drop(guard),
        }
    }

    fn disarm_timer(&mut self) {
        if let Some(t) = &mut self.timer {
            t.disarm();
        }
    }

    fn convert_error(&self, e: &tokio_postgres::Error) -> DbError {
        let err = map_error(e);
        if err.code.as_deref() == Some("57014")
            && let Some(t) = &self.timer
            && t.has_fired()
        {
            let mut te = DbError::timeout(format!(
                "The statement was cancelled because it exceeded the timeout of {:.1}s",
                t.timeout.as_secs_f64()
            ))
            .with_code("57014");
            te.detail = Some(err.message.clone());
            return te;
        }
        err
    }

    /// Handle an execution error: map it, update transaction state and finish.
    async fn fail(&mut self, e: tokio_postgres::Error) -> DbError {
        let err = self.convert_error(&e);
        match &mut self.body {
            Body::Simple(b) => {
                apply_failure(&self.shared, b.script.statements.get(b.stmt_index));
                b.stmt_index = b.script.statements.len();
            }
            Body::Extended(b) => apply_failure(&self.shared, Some(&b.info)),
            _ => {}
        }
        // Only ReadyForQuery is left; drain it so the gate can be released.
        let mut body = std::mem::replace(&mut self.body, Body::Empty);
        if tokio::time::timeout(FULL_DRAIN, body.drain(&self.shared)).await.is_err() {
            tracing::warn!("timed out reading the end of a failed statement");
        }
        self.complete();
        err
    }

    /// Discard the unread rows of the current row result. Every call of
    /// `fetch_inner` consumes at least one message, so this terminates.
    async fn skip_rows(&mut self) -> DbResult<()> {
        while self.in_rows && !self.finished {
            self.fetch_inner(4096, false).await?;
        }
        Ok(())
    }

    async fn fetch_inner(&mut self, max_rows: usize, keep: bool) -> DbResult<Vec<Row>> {
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        if !self.in_rows || self.finished {
            return Ok(Vec::new());
        }
        let max = max_rows.max(1);
        let mut rows: Vec<Row> = if keep { Vec::with_capacity(max.min(4096)) } else { Vec::new() };
        let mut got_any = false;
        let mut count = 0usize;
        loop {
            if count >= max {
                break;
            }
            let next = match &mut self.body {
                Body::Simple(b) => match b.stream.next().await {
                    Some(Ok(SimpleQueryMessage::Row(r))) => {
                        if keep {
                            let mut out = Vec::with_capacity(b.convs.len());
                            for (i, conv) in b.convs.iter().enumerate() {
                                out.push(match r.try_get(i) {
                                    Ok(Some(s)) => text_value(conv, s),
                                    _ => Value::Null,
                                });
                            }
                            rows.push(out);
                        }
                        Step::Row
                    }
                    Some(Ok(SimpleQueryMessage::CommandComplete(_))) => {
                        apply_success(&self.shared, b.script.statements.get(b.stmt_index));
                        b.stmt_index += 1;
                        Step::EndOfResult
                    }
                    Some(Ok(_)) => Step::Row,
                    Some(Err(e)) => Step::Error(e),
                    None => Step::EndOfStream,
                },
                Body::Extended(b) => match b.stream.next().await {
                    Some(Ok(r)) => {
                        if keep && let Some(cols) = &b.columns {
                            let mut out = Vec::with_capacity(cols.len());
                            for (i, c) in cols.iter().enumerate() {
                                out.push(match r.try_get::<_, Raw<'_>>(i) {
                                    Ok(raw) => binary_value(&c.conv, raw.0),
                                    Err(_) => Value::Null,
                                });
                            }
                            rows.push(out);
                        }
                        Step::Row
                    }
                    Some(Err(e)) => Step::Error(e),
                    None => {
                        apply_success(&self.shared, Some(&b.info));
                        Step::EndOfStream
                    }
                },
                Body::Copy(b) => match b.stream.next().await {
                    Some(Ok(bytes)) => {
                        if keep {
                            rows.push(vec![copy_line(bytes)]);
                        }
                        Step::Row
                    }
                    Some(Err(e)) => Step::Error(e),
                    None => Step::EndOfStream,
                },
                Body::Empty => Step::EndOfStream,
            };
            match next {
                Step::Row => {
                    count += 1;
                    if !got_any {
                        got_any = true;
                        self.disarm_timer();
                    }
                }
                Step::EndOfResult => {
                    self.in_rows = false;
                    break;
                }
                Step::EndOfStream => {
                    self.complete();
                    break;
                }
                Step::Error(e) => {
                    let err = self.fail(e).await;
                    if rows.is_empty() {
                        return Err(err);
                    }
                    self.pending_error = Some(err);
                    break;
                }
            }
        }
        Ok(rows)
    }

    async fn next_result_inner(&mut self) -> DbResult<Option<ResultHeader>> {
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        if self.finished {
            return Ok(None);
        }
        if self.in_rows {
            self.skip_rows().await?;
            if self.finished {
                return Ok(None);
            }
        }
        loop {
            let step = match &mut self.body {
                Body::Simple(b) => match b.stream.next().await {
                    Some(Ok(SimpleQueryMessage::RowDescription(cols))) => {
                        let names: Vec<&str> = cols.iter().map(|c| c.name()).collect();
                        let plans = match b.prepared.take() {
                            Some(p)
                                if b.results_seen == 0
                                    && p.len() == names.len()
                                    && p.iter().zip(&names).all(|(a, n)| a.meta.name == *n) =>
                            {
                                p
                            }
                            _ => names
                                .iter()
                                .map(|n| ColumnPlan { meta: ColumnMeta::new(*n, "text", DataKind::Text), conv: Conv::Text })
                                .collect(),
                        };
                        b.results_seen += 1;
                        b.convs = plans.iter().map(|p| p.conv.clone()).collect();
                        HeaderStep::Header(ResultHeader::Rows(plans.into_iter().map(|p| p.meta).collect()), true)
                    }
                    Some(Ok(SimpleQueryMessage::CommandComplete(n))) => {
                        let info = b.script.statements.get(b.stmt_index);
                        apply_success(&self.shared, info);
                        let header = ResultHeader::Update {
                            rows_affected: info.and_then(|i| i.rows_affected(n)),
                            last_insert_id: None,
                            tag: info.map(|i| i.command_tag(n)).filter(|t| !t.is_empty()),
                        };
                        b.stmt_index += 1;
                        b.results_seen += 1;
                        HeaderStep::Header(header, false)
                    }
                    Some(Ok(_)) => HeaderStep::Continue,
                    Some(Err(e)) => HeaderStep::Error(e),
                    None => HeaderStep::End,
                },
                Body::Extended(b) => {
                    if !b.header_done {
                        b.header_done = true;
                        match &b.columns {
                            Some(cols) => HeaderStep::Header(
                                ResultHeader::Rows(cols.iter().map(|c| c.meta.clone()).collect()),
                                true,
                            ),
                            None => {
                                // statement without rows: run it to completion
                                let mut error = None;
                                while let Some(r) = b.stream.next().await {
                                    if let Err(e) = r {
                                        error = Some(e);
                                        break;
                                    }
                                }
                                match error {
                                    Some(e) => HeaderStep::Error(e),
                                    None => {
                                        apply_success(&self.shared, Some(&b.info));
                                        let n = b.stream.rows_affected().unwrap_or(0);
                                        let header = ResultHeader::Update {
                                            rows_affected: b.info.rows_affected(n),
                                            last_insert_id: None,
                                            tag: Some(b.info.command_tag(n)).filter(|t| !t.is_empty()),
                                        };
                                        HeaderStep::HeaderThenEnd(header)
                                    }
                                }
                            }
                        }
                    } else {
                        HeaderStep::End
                    }
                }
                Body::Copy(b) => {
                    if !b.header_done {
                        b.header_done = true;
                        HeaderStep::Header(ResultHeader::Rows(vec![ColumnMeta::new("COPY", "text", DataKind::Text)]), true)
                    } else {
                        HeaderStep::End
                    }
                }
                Body::Empty => HeaderStep::End,
            };
            match step {
                HeaderStep::Header(h, rows) => {
                    self.in_rows = rows;
                    return Ok(Some(h));
                }
                HeaderStep::HeaderThenEnd(h) => {
                    self.complete();
                    return Ok(Some(h));
                }
                HeaderStep::Continue => continue,
                HeaderStep::Error(e) => return Err(self.fail(e).await),
                HeaderStep::End => {
                    self.complete();
                    return Ok(None);
                }
            }
        }
    }

    /// Stop the statement (cancelling it if needed) and release the session.
    async fn shutdown(&mut self) {
        if self.finished {
            self.complete();
            return;
        }
        let body = std::mem::replace(&mut self.body, Body::Empty);
        abandon(body, self.shared.clone(), self.cancel.clone()).await;
        self.complete();
    }
}

enum Step {
    Row,
    EndOfResult,
    EndOfStream,
    Error(tokio_postgres::Error),
}

enum HeaderStep {
    Header(ResultHeader, bool),
    HeaderThenEnd(ResultHeader),
    Continue,
    Error(tokio_postgres::Error),
    End,
}

fn copy_line(bytes: Bytes) -> Value {
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    Value::text(s)
}

#[async_trait]
impl QueryCursor for PgCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        self.next_result_inner().await
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        self.fetch_inner(max_rows, true).await
    }

    async fn close(&mut self) -> DbResult<()> {
        self.shutdown().await;
        self.pending_error = None;
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        self.shared.take_notices()
    }
}

impl Drop for PgCursor {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.shared.cursor_open.store(false, Ordering::Release);
        let body = std::mem::replace(&mut self.body, Body::Empty);
        let shared = self.shared.clone();
        let cancel = self.cancel.clone();
        let guard = self.guard.take();
        let timer = self.timer.take();
        // The runtime handle allows dropping the cursor from any thread.
        self.rt.spawn(async move {
            abandon(body, shared, cancel).await;
            if let Some(pending_cancel) = timer.and_then(StatementTimer::stop) {
                let _ = pending_cancel.await;
            }
            drop(guard);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_lines_strip_newline() {
        assert_eq!(copy_line(Bytes::from_static(b"1\ta\n")), Value::text("1\ta"));
        assert_eq!(copy_line(Bytes::from_static(b"x\r\n")), Value::text("x"));
        assert_eq!(copy_line(Bytes::from_static(b"")), Value::text(""));
    }

    #[test]
    fn tx_effects() {
        let s = SessionShared::new();
        apply_success(&s, Some(&crate::sqltext::classify("begin")));
        assert!(s.in_tx());
        apply_failure(&s, Some(&crate::sqltext::classify("insert into t values (1)")));
        assert!(s.in_tx());
        apply_success(&s, Some(&crate::sqltext::classify("commit and chain")));
        assert!(s.in_tx());
        apply_failure(&s, Some(&crate::sqltext::classify("commit")));
        assert!(!s.in_tx());
        apply_success(&s, Some(&crate::sqltext::classify("start transaction")));
        apply_success(&s, Some(&crate::sqltext::classify("rollback to savepoint a")));
        assert!(s.in_tx());
        apply_success(&s, Some(&crate::sqltext::classify("prepare transaction 'x'")));
        assert!(!s.in_tx());
    }
}
