//! Shared, serialised access to one database session plus its single open cursor.

use dbiji_core::driver::{CancelHandle, ExecOptions, QueryCursor, ResultHeader, ServerMessage, Session, TransactionMode};
use dbiji_core::{DbError, DbKind, DbResult, Dialect, Row, Value};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One statement executed inside the current transaction (transaction log, FR-C5).
#[derive(Debug, Clone)]
pub struct TxLogEntry {
    pub sql: String,
    pub at: chrono::DateTime<chrono::Local>,
    pub duration: Duration,
    pub rows: Option<u64>,
    pub ok: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TxState {
    pub mode: TransactionMode,
    pub in_transaction: bool,
    pub log: Vec<TxLogEntry>,
}

struct OpenCursor {
    token: u64,
    cursor: Box<dyn QueryCursor>,
}

struct Shared {
    session: tokio::sync::Mutex<Box<dyn Session>>,
    cursor: tokio::sync::Mutex<Option<OpenCursor>>,
    cancel: Option<Arc<dyn CancelHandle>>,
    kind: DbKind,
    dialect: Arc<dyn Dialect>,
    label: String,
    database: Option<String>,
    busy: AtomicBool,
    closed: AtomicBool,
    /// Closed by [`SessionHandle::close`] (not lost: the server or the network ended it).
    closed_here: AtomicBool,
    next_token: AtomicU64,
    tx: Mutex<TxState>,
    busy_since: Mutex<Option<Instant>>,
}

/// Cloneable handle to a session. All access goes through [`SessionHandle::lock`],
/// which first closes any cursor left open on the session (the driver contract
/// allows only one open cursor per session).
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Shared>,
}

/// Result of [`SessionHandle::execute`].
pub struct Executed {
    pub token: u64,
    pub header: Option<ResultHeader>,
    pub messages: Vec<ServerMessage>,
}

impl SessionHandle {
    pub fn new(session: Box<dyn Session>, label: impl Into<String>, database: Option<String>) -> Self {
        let kind = session.kind();
        let dialect = session.dialect();
        let cancel = session.cancel_handle();
        let mode = session.transaction_mode();
        SessionHandle {
            inner: Arc::new(Shared {
                session: tokio::sync::Mutex::new(session),
                cursor: tokio::sync::Mutex::new(None),
                cancel,
                kind,
                dialect,
                label: label.into(),
                database,
                busy: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                closed_here: AtomicBool::new(false),
                next_token: AtomicU64::new(1),
                tx: Mutex::new(TxState { mode, ..Default::default() }),
                busy_since: Mutex::new(None),
            }),
        }
    }

    pub fn kind(&self) -> DbKind {
        self.inner.kind
    }

    pub fn dialect(&self) -> Arc<dyn Dialect> {
        self.inner.dialect.clone()
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn database(&self) -> Option<&str> {
        self.inner.database.as_deref()
    }

    pub fn same(&self, other: &SessionHandle) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// A statement is currently running on this session.
    pub fn is_busy(&self) -> bool {
        self.inner.busy.load(Ordering::Relaxed)
    }

    pub fn busy_for(&self) -> Option<Duration> {
        self.inner.busy_since.lock().map(|t| t.elapsed())
    }

    pub fn is_closed(&self) -> bool {
        if self.inner.closed.load(Ordering::Relaxed) {
            return true;
        }
        // The server or the network may have ended the connection while it was idle
        // (idle timeout, server restart): the driver knows without a round trip.
        // A busy session is not waited for; its statement reports the error.
        if let Ok(s) = self.inner.session.try_lock()
            && s.is_closed()
        {
            self.inner.closed.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Closed without [`close`](Self::close): the connection to the server was lost.
    pub fn is_lost(&self) -> bool {
        self.is_closed() && !self.inner.closed_here.load(Ordering::Relaxed)
    }

    pub fn tx_state(&self) -> TxState {
        self.inner.tx.lock().clone()
    }

    pub fn clear_tx_log(&self) {
        let mut tx = self.inner.tx.lock();
        tx.log.clear();
    }

    /// Cancel the running statement (out of band).
    pub async fn cancel(&self) -> DbResult<()> {
        match &self.inner.cancel {
            Some(c) => c.cancel().await,
            None => Err(DbError::unsupported("Cancelling is not supported for this database")),
        }
    }

    pub fn can_cancel(&self) -> bool {
        self.inner.cancel.is_some()
    }

    /// Close the open cursor (if any).
    pub async fn close_cursor(&self) {
        let taken = self.inner.cursor.lock().await.take();
        if let Some(mut c) = taken {
            let _ = c.cursor.close().await;
        }
    }

    /// Exclusive access to the session, after closing any open cursor.
    /// The cursor is closed once the session is ours: closed before, a caller
    /// waiting behind a running `execute` would get the session with that
    /// execute's new cursor still open.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, Box<dyn Session>> {
        let guard = self.inner.session.lock().await;
        self.close_cursor().await;
        guard
    }

    fn mark_busy(&self, on: bool) {
        self.inner.busy.store(on, Ordering::Relaxed);
        *self.inner.busy_since.lock() = if on { Some(Instant::now()) } else { None };
    }

    /// Busy until the guard is dropped (also when the operation is cancelled mid-await).
    fn busy(&self) -> BusyGuard<'_> {
        self.mark_busy(true);
        BusyGuard(self)
    }

    fn note_error(&self, e: &DbError) {
        if e.is_connection_lost() {
            self.inner.closed.store(true, Ordering::Relaxed);
        }
    }

    fn refresh_tx(&self, s: &dyn Session) {
        let mut tx = self.inner.tx.lock();
        tx.mode = s.transaction_mode();
        tx.in_transaction = s.in_transaction();
        if !tx.in_transaction {
            tx.log.clear();
        }
        if s.is_closed() {
            self.inner.closed.store(true, Ordering::Relaxed);
        }
    }

    fn log_tx(&self, sql: &str, started: Instant, rows: Option<u64>, ok: bool) {
        let mut tx = self.inner.tx.lock();
        if tx.in_transaction || tx.mode == TransactionMode::Manual {
            tx.log.push(TxLogEntry { sql: sql.chars().take(4000).collect(), at: chrono::Local::now(), duration: started.elapsed(), rows, ok });
            if tx.log.len() > 2000 {
                tx.log.remove(0);
            }
        }
    }

    /// Execute `sql` and keep the cursor open on the session. Returns the first
    /// result header; rows are read with [`SessionHandle::fetch`].
    pub async fn execute(&self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Executed> {
        let started = Instant::now();
        let busy = self.busy();
        let res = async {
            let mut guard = self.lock().await;
            let mut cursor = guard.execute(sql, params, opts).await?;
            let header = cursor.next_result().await;
            let messages = cursor.take_messages();
            self.refresh_tx(guard.as_ref());
            let header = header?;
            let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
            // Store the cursor before releasing the session so no other
            // operation can slip in while the cursor owns the connection.
            *self.inner.cursor.lock().await = Some(OpenCursor { token, cursor });
            drop(guard);
            Ok(Executed { token, header, messages })
        }
        .await;
        drop(busy);
        match &res {
            Ok(ex) => {
                let rows = match &ex.header {
                    Some(ResultHeader::Update { rows_affected, .. }) => *rows_affected,
                    _ => None,
                };
                self.log_tx(sql, started, rows, true);
            }
            Err(e) => {
                self.note_error(e);
                self.log_tx(sql, started, None, false);
                // Transaction state may have changed (e.g. failed transaction).
                if let Ok(g) = self.inner.session.try_lock() {
                    self.refresh_tx(g.as_ref());
                }
            }
        }
        res
    }

    /// Fetch up to `max` rows from the open cursor identified by `token`.
    /// Returns `Err(CursorClosed)` when the cursor was closed in between
    /// (another operation needed the session).
    pub async fn fetch(&self, token: u64, max: usize) -> Result<FetchOutcome, FetchError> {
        let busy = self.busy();
        let mut slot = self.inner.cursor.lock().await;
        let res = match slot.as_mut() {
            Some(c) if c.token == token => match c.cursor.fetch(max).await {
                Ok(rows) => {
                    let messages = c.cursor.take_messages();
                    Ok(FetchOutcome { exhausted: rows.len() < max, rows, messages })
                }
                Err(e) => {
                    self.note_error(&e);
                    Err(FetchError::Db(e))
                }
            },
            _ => Err(FetchError::CursorClosed),
        };
        drop(slot);
        drop(busy);
        res
    }

    /// Advance the open cursor to its next result (multi-result statements).
    pub async fn next_result(&self, token: u64) -> Result<Option<ResultHeader>, FetchError> {
        let mut slot = self.inner.cursor.lock().await;
        match slot.as_mut() {
            Some(c) if c.token == token => c.cursor.next_result().await.map_err(FetchError::Db),
            _ => Err(FetchError::CursorClosed),
        }
    }

    /// Whether the cursor identified by `token` is still open.
    pub async fn cursor_open(&self, token: u64) -> bool {
        self.inner.cursor.lock().await.as_ref().is_some_and(|c| c.token == token)
    }

    /// Close the cursor only if it is the one identified by `token`.
    pub async fn close_cursor_token(&self, token: u64) {
        let mut slot = self.inner.cursor.lock().await;
        if slot.as_ref().is_some_and(|c| c.token == token)
            && let Some(mut c) = slot.take()
        {
            let _ = c.cursor.close().await;
        }
    }

    /// Run `sql` to completion and return the first row set (metadata-style queries).
    pub async fn query_all(&self, sql: &str, params: &[Value]) -> DbResult<(Vec<dbiji_core::ColumnMeta>, Vec<Row>)> {
        let mut guard = self.lock().await;
        let r = dbiji_core::driver::query_all(guard.as_mut(), sql, params).await;
        self.refresh_tx(guard.as_ref());
        if let Err(e) = &r {
            self.note_error(e);
        }
        r
    }

    /// Run `sql` ignoring results; returns affected rows.
    pub async fn execute_update(&self, sql: &str, params: &[Value]) -> DbResult<Option<u64>> {
        let started = Instant::now();
        let mut guard = self.lock().await;
        let r = dbiji_core::driver::execute_update(guard.as_mut(), sql, params).await;
        self.refresh_tx(guard.as_ref());
        drop(guard);
        match &r {
            Ok(n) => self.log_tx(sql, started, *n, true),
            Err(e) => {
                self.note_error(e);
                self.log_tx(sql, started, None, false);
            }
        }
        r
    }

    /// Run data-change statements atomically (FR-D2 "save in one transaction").
    ///
    /// In auto-commit mode a transaction is opened, committed on success and
    /// rolled back on the first error, then auto-commit is restored. In manual
    /// mode the statements join the user's open transaction (nothing is
    /// committed). Returns the total number of affected rows and whether the
    /// changes are already committed.
    pub async fn apply_changes(&self, stmts: &[(String, Vec<Value>)]) -> DbResult<(u64, bool)> {
        self.apply_changes_reading(stmts).await.map(|a| (a.rows, a.committed))
    }

    /// [`apply_changes`](Self::apply_changes) that also keeps, per statement, the first
    /// row it returned (`INSERT … RETURNING`) and the generated id (MySQL).
    pub async fn apply_changes_reading(&self, stmts: &[(String, Vec<Value>)]) -> DbResult<Applied> {
        let mut guard = self.lock().await;
        let manual = guard.transaction_mode() == TransactionMode::Manual;
        if !manual {
            guard.set_transaction_mode(TransactionMode::Manual).await?;
        }
        // In the user's open transaction a failure must not leave the batch half applied:
        // the grid keeps every change pending, saving again would repeat the first ones.
        let savepoint = match (manual, guard.dialect().kind()) {
            (false, _) | (_, dbiji_core::DbKind::DuckDb | dbiji_core::DbKind::ClickHouse) => None,
            (true, dbiji_core::DbKind::SqlServer) => Some(("SAVE TRANSACTION dbiji_save", "ROLLBACK TRANSACTION dbiji_save", None)),
            (true, _) => Some(("SAVEPOINT dbiji_save", "ROLLBACK TO SAVEPOINT dbiji_save", Some("RELEASE SAVEPOINT dbiji_save"))),
        };
        let savepoint = match savepoint {
            Some(sp) if dbiji_core::driver::execute_update(guard.as_mut(), sp.0, &[]).await.is_ok() => Some(sp),
            _ => None,
        };
        let mut total = 0u64;
        let mut failure = None;
        let mut results = Vec::with_capacity(stmts.len());
        for (sql, params) in stmts {
            let started = Instant::now();
            match run_reading(guard.as_mut(), sql, params).await {
                Ok(r) => {
                    total += r.rows.unwrap_or(0);
                    self.log_tx(sql, started, r.rows, true);
                    results.push(r);
                }
                Err(e) => {
                    self.note_error(&e);
                    self.log_tx(sql, started, None, false);
                    failure = Some(e.with_context(sql.chars().take(200).collect::<String>()));
                    break;
                }
            }
        }
        if let Some((_, rollback, release)) = savepoint {
            if failure.is_some() {
                let _ = dbiji_core::driver::execute_update(guard.as_mut(), rollback, &[]).await;
            }
            if let Some(release) = release {
                let _ = dbiji_core::driver::execute_update(guard.as_mut(), release, &[]).await;
            }
        }
        let result = match failure {
            Some(e) => {
                if !manual {
                    let _ = guard.rollback().await;
                }
                Err(e)
            }
            None if !manual => guard.commit().await.map(|_| Applied { rows: total, committed: true, results }),
            None => Ok(Applied { rows: total, committed: false, results }),
        };
        if !manual {
            let _ = guard.set_transaction_mode(TransactionMode::Auto).await;
        }
        self.refresh_tx(guard.as_ref());
        result
    }

    pub async fn set_transaction_mode(&self, mode: TransactionMode) -> DbResult<()> {
        let mut guard = self.lock().await;
        let r = guard.set_transaction_mode(mode).await;
        self.refresh_tx(guard.as_ref());
        r
    }

    pub async fn commit(&self) -> DbResult<()> {
        let mut guard = self.lock().await;
        let r = guard.commit().await;
        self.refresh_tx(guard.as_ref());
        if r.is_ok() {
            self.inner.tx.lock().log.clear();
        }
        r
    }

    pub async fn rollback(&self) -> DbResult<()> {
        let mut guard = self.lock().await;
        let r = guard.rollback().await;
        self.refresh_tx(guard.as_ref());
        if r.is_ok() {
            self.inner.tx.lock().log.clear();
        }
        r
    }

    pub async fn ping(&self) -> DbResult<()> {
        // Do not disturb an open cursor or running statement for a keep-alive.
        if self.is_busy() {
            return Ok(());
        }
        let Ok(mut guard) = self.inner.session.try_lock() else { return Ok(()) };
        if self.inner.cursor.try_lock().map(|c| c.is_some()).unwrap_or(true) {
            return Ok(());
        }
        // A connection silently dropped by the network may never answer: bounded.
        let r = match tokio::time::timeout(Duration::from_secs(20), guard.ping()).await {
            Ok(r) => r,
            Err(_) => Err(DbError::connection("The server did not answer the ping within 20 s")),
        };
        if let Err(e) = &r {
            self.note_error(e);
        }
        r
    }

    pub async fn close(&self) {
        self.inner.closed_here.store(true, Ordering::Relaxed);
        self.close_cursor().await;
        let mut guard = self.inner.session.lock().await;
        let _ = guard.close().await;
        self.inner.closed.store(true, Ordering::Relaxed);
    }
}

/// Outcome of [`SessionHandle::apply_changes_reading`].
pub struct Applied {
    /// Rows changed.
    pub rows: u64,
    /// Committed (auto-commit), else left in the open transaction.
    pub committed: bool,
    /// Per statement, in order.
    pub results: Vec<StatementResult>,
}

/// What one statement of a save returned.
#[derive(Default)]
pub struct StatementResult {
    pub rows: Option<u64>,
    /// First row of its result (`RETURNING`).
    pub first_row: Option<Row>,
    /// Id generated by the statement (MySQL `LAST_INSERT_ID`).
    pub last_insert_id: Option<u64>,
}

/// Run `sql` to its end, keeping its first returned row and generated id.
async fn run_reading(s: &mut dyn Session, sql: &str, params: &[Value]) -> DbResult<StatementResult> {
    let mut cursor = s.execute(sql, params, &ExecOptions::default()).await?;
    let mut out = StatementResult::default();
    let mut returned = 0u64;
    let mut counted = false;
    while let Some(h) = cursor.next_result().await? {
        match h {
            ResultHeader::Update { rows_affected, last_insert_id, .. } => {
                if let Some(n) = rows_affected {
                    out.rows = Some(out.rows.unwrap_or(0) + n);
                    counted = true;
                }
                out.last_insert_id = last_insert_id.filter(|id| *id > 0).or(out.last_insert_id);
            }
            ResultHeader::Rows(_) => loop {
                let rows = cursor.fetch(1024).await?;
                if rows.is_empty() {
                    break;
                }
                returned += rows.len() as u64;
                if out.first_row.is_none() {
                    out.first_row = rows.into_iter().next();
                }
            },
        }
    }
    cursor.close().await?;
    // `INSERT … RETURNING` reports its rows as a result, not as a count.
    if !counted && returned > 0 {
        out.rows = Some(returned);
    }
    Ok(out)
}

pub struct FetchOutcome {
    pub rows: Vec<Row>,
    pub exhausted: bool,
    pub messages: Vec<ServerMessage>,
}

#[derive(Debug)]
pub enum FetchError {
    CursorClosed,
    Db(DbError),
}

impl From<FetchError> for DbError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::CursorClosed => DbError::internal("The result cursor was closed"),
            FetchError::Db(e) => e,
        }
    }
}

struct BusyGuard<'a>(&'a SessionHandle);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.mark_busy(false);
    }
}
