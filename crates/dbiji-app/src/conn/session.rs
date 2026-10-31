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
    /// Last use of the open cursor.
    cursor_used: Mutex<Instant>,
    /// A task watches the open cursor for idleness (one per session).
    cursor_watched: AtomicBool,
    /// Rollbacks done: results saved inside the transaction show stale rows after one.
    rollbacks: AtomicU64,
}

/// An open cursor not read for this long is closed: on the server it holds a
/// snapshot and table locks (PostgreSQL: an ALTER of the table waits for it).
const CURSOR_IDLE: Duration = Duration::from_secs(300);

/// How long an open cursor of `kind` may stay unread. SQLite locks the whole file:
/// an unfinished read blocks every writer (other tabs, other programs) until it ends,
/// so its cursor is closed almost at once and the next page runs the query again.
fn cursor_idle(kind: DbKind) -> Duration {
    match kind {
        DbKind::Sqlite => Duration::from_secs(1),
        _ => CURSOR_IDLE,
    }
}

/// Cloneable handle to a session. All access goes through [`SessionHandle::lock`],
/// which first closes any cursor left open on the session (the driver contract
/// allows only one open cursor per session).
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Shared>,
}

/// A session that is not kept open by this reference (closed once its tabs drop it).
#[derive(Clone)]
pub struct WeakSession(std::sync::Weak<Shared>);

impl WeakSession {
    pub fn upgrade(&self) -> Option<SessionHandle> {
        self.0.upgrade().map(|inner| SessionHandle { inner })
    }
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
                cursor_used: Mutex::new(Instant::now()),
                cursor_watched: AtomicBool::new(false),
                rollbacks: AtomicU64::new(0),
            }),
        }
    }

    pub fn kind(&self) -> DbKind {
        self.inner.kind
    }

    pub fn downgrade(&self) -> WeakSession {
        WeakSession(Arc::downgrade(&self.inner))
    }

    /// Cancel a running statement, then close (Disconnect: a long query does not hold it up).
    pub async fn cancel_and_close(&self) {
        if self.is_busy() {
            let _ = self.cancel().await;
        }
        self.close().await;
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

    /// An open transaction with changes (manual commit mode opens one for a SELECT too).
    pub fn has_uncommitted_writes(&self) -> bool {
        let tx = self.inner.tx.lock();
        tx.in_transaction && tx.log.iter().any(|e| dbiji_sql::classify(&e.sql, self.inner.dialect.syntax()).is_write)
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
            let was_in_tx = guard.in_transaction();
            let mut cursor = guard.execute(sql, params, opts).await?;
            let header = cursor.next_result().await;
            let messages = cursor.take_messages();
            self.refresh_tx(guard.as_ref());
            // A ROLLBACK typed in an editor ends the transaction like the Rollback button:
            // results saved inside it reload.
            if was_in_tx && !guard.in_transaction() && ends_with_rollback(sql) {
                self.inner.rollbacks.fetch_add(1, Ordering::Relaxed);
            }
            let header = header?;
            let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
            // Store the cursor before releasing the session so no other
            // operation can slip in while the cursor owns the connection.
            let rows = matches!(header, Some(ResultHeader::Rows(_)));
            *self.inner.cursor.lock().await = Some(OpenCursor { token, cursor });
            *self.inner.cursor_used.lock() = Instant::now();
            drop(guard);
            // Rows left to read hold the cursor open (an INSERT's is gone at once).
            if rows {
                self.watch_idle_cursor();
            }
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
                    *self.inner.cursor_used.lock() = Instant::now();
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

    /// Close the open cursor once it was not read for [`cursor_idle`]; the result
    /// fetches its further rows by running the query again. One task per session
    /// watches whichever cursor is open, and ends when none is.
    fn watch_idle_cursor(&self) {
        if self.inner.cursor_watched.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.inner.cursor_watched.store(false, Ordering::Release);
            return;
        };
        let weak = self.downgrade();
        let limit = cursor_idle(self.inner.kind);
        rt.spawn(async move {
            let mut wait = limit;
            loop {
                tokio::time::sleep(wait).await;
                let Some(s) = weak.upgrade() else { return };
                // Busy (a fetch is running): looked at again later.
                let token = match s.inner.cursor.try_lock() {
                    Ok(slot) => slot.as_ref().map(|c| c.token),
                    Err(_) => {
                        wait = limit.min(Duration::from_secs(30));
                        continue;
                    }
                };
                let Some(token) = token else {
                    s.inner.cursor_watched.store(false, Ordering::Release);
                    return;
                };
                let idle = s.inner.cursor_used.lock().elapsed();
                if idle >= limit && !s.is_busy() {
                    s.close_cursor_token(token).await;
                    s.inner.cursor_watched.store(false, Ordering::Release);
                    return;
                }
                wait = limit.saturating_sub(idle).max(limit.min(Duration::from_secs(10)));
            }
        });
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
        self.apply_changes_reading(stmts, &[]).await.map(|a| (a.rows, a.committed))
    }

    /// [`apply_changes`](Self::apply_changes) that also keeps, per statement, the first
    /// row it returned (`INSERT … RETURNING`) and the generated id (MySQL).
    /// `expect_one[i]`: statement `i` must change exactly one row (a grid UPDATE / DELETE by
    /// its key); otherwise the save fails and is undone (0: changed or deleted by someone
    /// else; more: the key does not identify one row).
    pub async fn apply_changes_reading(&self, stmts: &[(String, Vec<Value>)], expect_one: &[bool]) -> DbResult<Applied> {
        let mut guard = self.lock().await;
        // A transaction already open (manual commit, or BEGIN in an auto-commit editor):
        // the statements join it and nothing is committed or rolled back for the user.
        let in_tx = guard.in_transaction();
        let manual = guard.transaction_mode() == TransactionMode::Manual || in_tx;
        if !manual {
            guard.set_transaction_mode(TransactionMode::Manual).await?;
        }
        // In the user's open transaction a failure must not leave the batch half applied:
        // the grid keeps every change pending, saving again would repeat the first ones.
        // Without one, a savepoint would itself be the transaction (SQLite: RELEASE commits),
        // and on failure the whole batch is rolled back instead.
        let savepoint = match (in_tx, guard.dialect().kind()) {
            (false, _) | (_, dbiji_core::DbKind::ClickHouse | dbiji_core::DbKind::Cassandra) => None,
            (true, dbiji_core::DbKind::SqlServer) => Some(("SAVE TRANSACTION dbiji_save", "ROLLBACK TRANSACTION dbiji_save", None)),
            // Oracle has no RELEASE SAVEPOINT.
            (true, dbiji_core::DbKind::Oracle) => Some(("SAVEPOINT dbiji_save", "ROLLBACK TO SAVEPOINT dbiji_save", None)),
            (true, _) => Some(("SAVEPOINT dbiji_save", "ROLLBACK TO SAVEPOINT dbiji_save", Some("RELEASE SAVEPOINT dbiji_save"))),
        };
        let savepoint = match savepoint {
            Some(sp) if dbiji_core::driver::execute_update(guard.as_mut(), sp.0, &[]).await.is_ok() => Some(sp),
            _ => None,
        };
        let mut total = 0u64;
        let mut failure = None;
        let mut results = Vec::with_capacity(stmts.len());
        // How far the driver's row count can be trusted: SQL Server reports none with
        // NOCOUNT and adds a trigger's; MySQL counts changed rows (an unchanged value is 0).
        let (zero_means_missing, many_means_ambiguous) = match self.kind() {
            DbKind::Postgres | DbKind::CockroachDb | DbKind::Sqlite | DbKind::Oracle => (true, true),
            DbKind::MySql | DbKind::MariaDb => (false, true),
            _ => (false, false),
        };
        for (sql, params) in stmts {
            let started = Instant::now();
            let checked = run_reading(guard.as_mut(), sql, params).await.and_then(|r| match r.rows {
                Some(n) if expect_one.get(results.len()).copied().unwrap_or(false) && ((n == 0 && zero_means_missing) || (n > 1 && many_means_ambiguous)) => Err(if n == 0 {
                    DbError::query("The row was not found: it was changed or deleted since it was read. Refresh and edit it again.")
                } else {
                    DbError::query(format!("The statement would change {n} rows instead of one: the key does not identify a single row."))
                }),
                _ => Ok(r),
            });
            match checked {
                Ok(r) => {
                    // No count (Cassandra, SQL Server's NOCOUNT): the statement was made for one row.
                    total += r.rows.unwrap_or(1);
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
                // Nothing of the user's was in the transaction: all of the batch goes.
                if !in_tx && savepoint.is_none() {
                    let _ = guard.rollback().await;
                }
                Err(e)
            }
            None if !manual => guard.commit().await.map(|_| Applied { rows: total, committed: true, results }),
            // Manual mode without a transaction before: it holds just this save, left open to commit.
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
            self.inner.rollbacks.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    /// Number of rollbacks done on the session.
    pub fn rollbacks(&self) -> u64 {
        self.inner.rollbacks.load(Ordering::Relaxed)
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
        self.close_from_server().await;
    }

    /// Tests: the connection ends as when the server or the network dropped it
    /// ([`is_lost`](Self::is_lost)).
    #[doc(hidden)]
    pub async fn close_from_server(&self) {
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
/// `sql` rolls the transaction back (`ROLLBACK`, `ABORT`, not `ROLLBACK TO SAVEPOINT`).
fn ends_with_rollback(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    upper.split(';').map(str::trim).filter(|s| !s.is_empty()).any(|s| (s.starts_with("ROLLBACK") || s.starts_with("ABORT")) && !s.contains(" TO "))
}

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

#[cfg(test)]
mod tests {
    use super::ends_with_rollback;

    #[test]
    fn typed_rollbacks() {
        assert!(ends_with_rollback("rollback"));
        assert!(ends_with_rollback("UPDATE t SET a = 1; ROLLBACK;"));
        assert!(ends_with_rollback("ABORT"));
        assert!(!ends_with_rollback("ROLLBACK TO SAVEPOINT s1"));
        assert!(!ends_with_rollback("COMMIT"));
    }
}
