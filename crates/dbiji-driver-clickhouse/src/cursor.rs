//! Streaming [`QueryCursor`] for ClickHouse.
//!
//! Each statement runs on a dedicated OS thread that performs the blocking
//! HTTP request and parses the body line by line. Converted rows travel in
//! small batches through a bounded channel, so memory stays bounded and a
//! slow consumer applies TCP backpressure to the server. Closing or
//! dropping the cursor before the end stops the thread and issues
//! `KILL QUERY` for the statement's `query_id`.

use crate::http::{QueryRequest, ROW_FORMAT, ResponseHead, Transport};
use crate::stream::{BodyParser, Feed, LineReader, header_columns};
use crate::types::{ChType, column_meta, json_to_value, parse_type};
use async_trait::async_trait;
use dbiji_core::value::ColumnMeta;
use dbiji_core::{DbError, DbResult, ErrorKind, QueryCursor, ResultHeader, Row, ServerMessage, Value};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, mpsc};

/// Rows per channel message.
const BATCH_ROWS: usize = 256;
/// Channel capacity in messages (bounded memory: capacity × batch).
const CHANNEL_CAPACITY: usize = 8;
/// Read buffer for the response body.
const READ_BUFFER: usize = 64 * 1024;

/// Message from the reader thread to the cursor.
#[derive(Debug)]
pub(crate) enum Event {
    Header(ResultHeader),
    Rows(Vec<Row>),
    Message(ServerMessage),
    Error(DbError),
}

/// State shared by a running statement, the cursor and the cancel handle.
#[derive(Debug, Default)]
pub(crate) struct JobState {
    /// Stop reading (cursor closed / dropped).
    pub abort: AtomicBool,
    /// The response has been read completely; nothing to kill.
    pub finished: AtomicBool,
    /// A statement timeout fired.
    pub timed_out: AtomicBool,
    /// The user cancelled.
    pub cancelled: AtomicBool,
    /// The reader is blocked handing rows to a slow consumer (this time does
    /// not count towards the statement timeout).
    pub waiting_on_consumer: AtomicBool,
}

/// Currently running statement of a session, for the cancel handle.
#[derive(Debug, Default)]
pub(crate) struct ActiveQuery {
    current: Mutex<Option<(String, Arc<JobState>)>>,
}

impl ActiveQuery {
    pub fn set(&self, query_id: &str, state: Arc<JobState>) {
        if let Ok(mut g) = self.current.lock() {
            *g = Some((query_id.to_string(), state));
        }
    }

    pub fn clear(&self, query_id: &str) {
        if let Ok(mut g) = self.current.lock()
            && g.as_ref().is_some_and(|(q, _)| q == query_id)
        {
            *g = None;
        }
    }

    pub fn get(&self) -> Option<(String, Arc<JobState>)> {
        self.current.lock().ok().and_then(|g| g.clone())
    }
}

/// Everything the reader thread needs.
pub(crate) struct Job {
    pub transport: Arc<Transport>,
    pub request: QueryRequest,
    /// First keyword of the statement, upper case (`SELECT`, `INSERT` ...).
    pub keyword: String,
    /// Database selected by a `USE` statement, applied on success.
    pub use_database: Option<String>,
    /// Session's tracked current database.
    pub database: Arc<Mutex<Option<String>>>,
    pub active: Arc<ActiveQuery>,
    pub timeout: Option<Duration>,
    /// Released when the thread is done, unblocking the next statement.
    pub permit: Option<OwnedSemaphorePermit>,
}

/// Start `job` on its own thread and return the cursor reading its results.
/// Ids of cursors: the session's flag holds the id of the open one.
static NEXT_CURSOR: AtomicU64 = AtomicU64::new(1);

pub(crate) fn start(job: Job, open_flag: Arc<AtomicU64>) -> DbResult<ChannelCursor> {
    let (tx, rx) = mpsc::channel::<Event>(CHANNEL_CAPACITY);
    let state = Arc::new(JobState::default());
    let query_id = job.request.query_id.clone();
    job.active.set(&query_id, state.clone());

    // Statement timeout: a watchdog kills the query once the time spent
    // waiting for the server (not for the consumer) exceeds the timeout.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    if let Some(timeout) = job.timeout {
        let transport = job.transport.clone();
        let st = state.clone();
        let qid = query_id.clone();
        std::thread::Builder::new()
            .name("dbiji-ch-timeout".into())
            .spawn(move || {
                let tick = Duration::from_millis(50).min(timeout.max(Duration::from_millis(1)));
                let mut used = Duration::ZERO;
                // ends when the reader finishes (sender dropped)
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(tick) {
                    if !st.waiting_on_consumer.load(Ordering::SeqCst) {
                        used += tick;
                    }
                    if used >= timeout {
                        if !st.finished.load(Ordering::SeqCst) {
                            st.timed_out.store(true, Ordering::SeqCst);
                            if let Err(e) = transport.kill_query(&qid, false) {
                                tracing::debug!("statement timeout: KILL QUERY failed: {e}");
                            }
                        }
                        break;
                    }
                }
            })
            .map_err(|e| DbError::internal(format!("Cannot start timeout thread: {e}")))?;
    }

    let thread_state = state.clone();
    let transport = job.transport.clone();
    std::thread::Builder::new()
        .name("dbiji-ch-query".into())
        .spawn(move || {
            let qid = job.request.query_id.clone();
            let active = job.active.clone();
            let transport = job.transport.clone();
            let permit = run(job, &tx, &thread_state);
            drop(done_tx);
            if !thread_state.finished.load(Ordering::SeqCst) {
                // The response was abandoned: stop server side work.
                if let Err(e) = transport.kill_query(&qid, false) {
                    tracing::debug!("KILL QUERY after abandoned cursor failed: {e}");
                }
            }
            active.clear(&qid);
            drop(permit);
        })
        .map_err(|e| DbError::internal(format!("Cannot start query thread: {e}")))?;

    open_flag.store(NEXT_CURSOR.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    let close_state = state.clone();
    let on_close: Box<dyn FnOnce() + Send> = Box::new(move || {
        close_state.abort.store(true, Ordering::SeqCst);
        if !close_state.finished.load(Ordering::SeqCst) {
            // The reader may be blocked waiting for the server: kill the
            // query so the response ends and the thread can exit.
            let _ = std::thread::Builder::new().name("dbiji-ch-kill".into()).spawn(move || {
                if let Err(e) = transport.kill_query(&query_id, false) {
                    tracing::debug!("KILL QUERY on cursor close failed: {e}");
                }
            });
        }
    });
    Ok(ChannelCursor::new(rx, Some(on_close), open_flag))
}

/// Send helper: false when the cursor is gone or reading was aborted.
fn emit(tx: &mpsc::Sender<Event>, state: &JobState, ev: Event) -> bool {
    if state.abort.load(Ordering::SeqCst) {
        return false;
    }
    state.waiting_on_consumer.store(true, Ordering::SeqCst);
    let ok = tx.blocking_send(ev).is_ok();
    state.waiting_on_consumer.store(false, Ordering::SeqCst);
    ok
}

/// Adjust errors for user cancel / timeout.
fn finalize_error(state: &JobState, timeout: Option<Duration>, e: DbError) -> DbError {
    if state.timed_out.load(Ordering::SeqCst) {
        let secs = timeout.map(|t| t.as_secs_f64()).unwrap_or(0.0);
        return DbError::timeout(format!("Statement timeout of {secs:.1} s exceeded; the query was killed")).with_code(e.code.unwrap_or_default());
    }
    if state.cancelled.load(Ordering::SeqCst) && e.kind == ErrorKind::Cancelled {
        return DbError::cancelled();
    }
    e
}

/// Thread body. Returns the semaphore permit so the caller releases it last.
fn run(mut job: Job, tx: &mpsc::Sender<Event>, state: &JobState) -> Option<OwnedSemaphorePermit> {
    let permit = job.permit.take();
    let mut attempts = 0u32;
    let sent = loop {
        match job.transport.send(&job.request) {
            // The previous (killed) statement may still hold the session lock.
            Err(e) if e.code.as_deref().is_some_and(|c| c.starts_with("373")) && attempts < 40 => {
                attempts += 1;
                if state.abort.load(Ordering::SeqCst) {
                    state.finished.store(true, Ordering::SeqCst);
                    return permit;
                }
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempts.min(5))));
            }
            other => break other,
        }
    };
    let (head, body) = match sent {
        Ok(x) => x,
        Err(e) => {
            state.finished.store(true, Ordering::SeqCst);
            let e = finalize_error(state, job.timeout, e);
            emit(tx, state, Event::Error(e));
            return permit;
        }
    };
    let outcome = match head.format.as_deref() {
        None => read_update(&job, &head, body, tx, state),
        Some(f) if f == ROW_FORMAT => read_rows(&head, body, tx, state),
        Some(other) => read_raw(other, &head, body, tx, state),
    };
    match outcome {
        Ok(true) => {
            state.finished.store(true, Ordering::SeqCst);
            if let Some(db) = job.use_database.take()
                && let Ok(mut g) = job.database.lock()
            {
                *g = Some(db);
            }
        }
        Ok(false) => {} // aborted: the caller kills the query
        Err(e) => {
            // the server ended the response with an error
            state.finished.store(true, Ordering::SeqCst);
            let e = finalize_error(state, job.timeout, e);
            emit(tx, state, Event::Error(e));
        }
    }
    permit
}

/// Statement without a result set (DDL, INSERT, SET ...).
fn read_update(job: &Job, head: &ResponseHead, body: crate::http::BodyReader, tx: &mpsc::Sender<Event>, state: &JobState) -> DbResult<bool> {
    let mut text = String::new();
    let mut limited = body.take(1 << 20);
    let _ = limited.read_to_string(&mut text);
    if !text.trim().is_empty() {
        let mut parser = BodyParser::new(head.exception_tag.clone(), head.exception_code.clone());
        for line in text.split('\n') {
            if let Feed::Error(e) = parser.feed(line.as_bytes()) {
                return Err(e);
            }
        }
        if let Some(e) = parser.finish() {
            return Err(e);
        }
        if let Some(i) = crate::error::find_exception_start(&text) {
            return Err(crate::error::exception_to_error(&text[i..], head.exception_code.as_deref()));
        }
    }
    let written = head.summary.as_ref().map(|s| s.written_rows).unwrap_or(0);
    let rows_affected = if job.keyword == "INSERT" || written > 0 { Some(written) } else { None };
    let tag = if job.keyword.is_empty() { None } else { Some(job.keyword.clone()) };
    Ok(emit(tx, state, Event::Header(ResultHeader::Update { rows_affected, last_insert_id: None, tag })))
}

/// `JSONCompactEachRowWithNamesAndTypes` body.
fn read_rows(head: &ResponseHead, body: crate::http::BodyReader, tx: &mpsc::Sender<Event>, state: &JobState) -> DbResult<bool> {
    let mut reader = LineReader::with_capacity(READ_BUFFER, body);
    let mut parser = BodyParser::new(head.exception_tag.clone(), head.exception_code.clone());
    let io_err = |e: std::io::Error| DbError::connection(format!("Error reading the ClickHouse response: {e}"));

    // Header rows: names, then types.
    let mut header_rows = Vec::with_capacity(2);
    while header_rows.len() < 2 {
        match reader.next_line().map_err(io_err)? {
            None => {
                if let Some(e) = parser.finish() {
                    return Err(e);
                }
                return Err(DbError::connection("The ClickHouse response ended before the column header"));
            }
            Some(line) => match parser.feed(line) {
                Feed::Row(r) => header_rows.push(r),
                Feed::Nothing => {}
                Feed::Error(e) => return Err(e),
            },
        }
    }
    let types_row = header_rows.pop().unwrap_or_default();
    let names_row = header_rows.pop().unwrap_or_default();
    let cols = header_columns(names_row, types_row)?;
    let types: Vec<ChType> = cols.iter().map(|(_, t)| parse_type(t)).collect();
    let metas: Vec<ColumnMeta> = cols.iter().zip(&types).map(|((n, t), ty)| column_meta(n, t, ty)).collect();
    if !emit(tx, state, Event::Header(ResultHeader::Rows(metas))) {
        return Ok(false);
    }

    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    loop {
        if state.abort.load(Ordering::SeqCst) {
            return Ok(false);
        }
        // Flush a partial batch before blocking on the network.
        if !batch.is_empty() && (batch.len() >= BATCH_ROWS || reader.buffer_is_empty()) {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ROWS));
            if !emit(tx, state, Event::Rows(full)) {
                return Ok(false);
            }
        }
        let line = match reader.next_line() {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                if !batch.is_empty() {
                    let full = std::mem::take(&mut batch);
                    emit(tx, state, Event::Rows(full));
                }
                return Err(io_err(e));
            }
        };
        match parser.feed(line) {
            Feed::Row(cells) => {
                if cells.len() != types.len() {
                    return Err(DbError::connection(format!("ClickHouse sent a row with {} values for {} columns", cells.len(), types.len())));
                }
                let row: Row = cells.into_iter().zip(&types).map(|(c, t)| json_to_value(t, c)).collect();
                batch.push(row);
            }
            Feed::Nothing => {}
            Feed::Error(e) => {
                if !batch.is_empty() {
                    let full = std::mem::take(&mut batch);
                    emit(tx, state, Event::Rows(full));
                }
                return Err(e);
            }
        }
    }
    if !batch.is_empty() && !emit(tx, state, Event::Rows(batch)) {
        return Ok(false);
    }
    if let Some(e) = parser.finish() {
        return Err(e);
    }
    Ok(true)
}

/// Any other output format (the statement had its own `FORMAT` clause):
/// one text column, one row per line.
fn read_raw(format: &str, head: &ResponseHead, body: crate::http::BodyReader, tx: &mpsc::Sender<Event>, state: &JobState) -> DbResult<bool> {
    let mut reader = LineReader::with_capacity(READ_BUFFER, body);
    let meta = ColumnMeta::new(format, "String", dbiji_core::value::DataKind::Text);
    let note = ServerMessage::new("INFO", format!("The statement requested FORMAT {format}; its output is shown as one text row per line"));
    if !emit(tx, state, Event::Message(note)) || !emit(tx, state, Event::Header(ResultHeader::Rows(vec![meta]))) {
        return Ok(false);
    }
    let mut exception: Option<BodyParser> = None;
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    loop {
        if state.abort.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if !batch.is_empty() && (batch.len() >= BATCH_ROWS || reader.buffer_is_empty()) {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ROWS));
            if !emit(tx, state, Event::Rows(full)) {
                return Ok(false);
            }
        }
        let line = match reader.next_line() {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => return Err(DbError::connection(format!("Error reading the ClickHouse response: {e}"))),
        };
        if let Some(p) = exception.as_mut() {
            if let Feed::Error(e) = p.feed(line) {
                return Err(e);
            }
            continue;
        }
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        if trimmed == crate::stream::EXCEPTION_MARKER.as_bytes() {
            let mut p = BodyParser::new(head.exception_tag.clone(), head.exception_code.clone());
            p.feed(trimmed);
            exception = Some(p);
            continue;
        }
        batch.push(vec![Value::text(String::from_utf8_lossy(trimmed).into_owned())]);
    }
    if let Some(mut p) = exception
        && let Some(e) = p.finish()
    {
        return Err(e);
    }
    if !batch.is_empty() && !emit(tx, state, Event::Rows(batch)) {
        return Ok(false);
    }
    Ok(true)
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
    /// The session's open cursor id, and this cursor's: cleared only while it is still this one.
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

    /// Receive the next event, handling messages and end of stream.
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
                Some(Event::Rows(_)) => {}
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

    fn rows(n: i64) -> Vec<Row> {
        (0..n).map(|i| vec![Value::Int(i)]).collect()
    }

    #[tokio::test]
    async fn protocol_rows_then_update() {
        let mut c = cursor_with(vec![
            Event::Header(ResultHeader::Rows(vec![ColumnMeta::new("a", "Int64", DataKind::Int)])),
            Event::Rows(rows(3)),
            Event::Rows(rows(2)),
            Event::Header(ResultHeader::Update { rows_affected: Some(1), last_insert_id: None, tag: None }),
        ]);
        assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        assert_eq!(c.fetch(4).await.unwrap().len(), 4);
        assert_eq!(c.fetch(4).await.unwrap().len(), 1);
        assert!(c.fetch(4).await.unwrap().is_empty());
        assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Update { rows_affected: Some(1), .. })));
        assert!(c.next_result().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn skipping_unread_rows() {
        let mut c = cursor_with(vec![Event::Header(ResultHeader::Rows(vec![])), Event::Rows(rows(10)), Event::Header(ResultHeader::Rows(vec![])), Event::Rows(rows(1))]);
        assert!(c.next_result().await.unwrap().is_some());
        assert!(c.next_result().await.unwrap().is_some());
        assert_eq!(c.fetch(10).await.unwrap().len(), 1);
        assert!(c.next_result().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn error_after_rows_is_reported_by_fetch() {
        let mut c = cursor_with(vec![Event::Header(ResultHeader::Rows(vec![])), Event::Rows(rows(2)), Event::Error(DbError::query("boom"))]);
        assert!(c.next_result().await.unwrap().is_some());
        assert_eq!(c.fetch(10).await.unwrap().len(), 2);
        assert_eq!(c.fetch(10).await.unwrap_err().message, "boom");
        assert!(c.next_result().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn error_before_header() {
        let mut c = cursor_with(vec![Event::Error(DbError::syntax("bad"))]);
        assert_eq!(c.next_result().await.unwrap_err().kind, ErrorKind::Syntax);
        assert!(c.next_result().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn close_drains_and_clears_flag() {
        let flag = Arc::new(AtomicU64::new(7));
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Event::Header(ResultHeader::Rows(vec![]))).unwrap();
        drop(tx);
        let called = Arc::new(AtomicBool::new(false));
        let c2 = called.clone();
        let mut c = ChannelCursor::new(rx, Some(Box::new(move || c2.store(true, Ordering::SeqCst))), flag.clone());
        c.close().await.unwrap();
        assert!(called.load(Ordering::SeqCst));
        assert_eq!(flag.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn late_drop_keeps_a_newer_cursors_flag() {
        let flag = Arc::new(AtomicU64::new(1));
        let (_tx, rx) = mpsc::channel(4);
        let old = ChannelCursor::new(rx, None, flag.clone());
        // a newer cursor took over the session
        flag.store(2, Ordering::SeqCst);
        drop(old);
        assert_eq!(flag.load(Ordering::SeqCst), 2);
    }
}
