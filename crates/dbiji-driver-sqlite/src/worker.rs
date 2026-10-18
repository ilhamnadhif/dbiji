//! The dedicated connection thread.
//!
//! `rusqlite::Connection` is `Send` but not `Sync`, and a prepared statement
//! borrows its connection, so every session owns one OS thread that owns the
//! connection and receives [`Command`]s through a channel. Replies travel back
//! through `tokio::sync::oneshot` channels so the async side never blocks.
//!
//! While a cursor is open the thread keeps the prepared statement alive inside
//! [`Worker::serve_cursor`] and steps it only when a `Fetch` arrives, so memory
//! stays bounded by the batch size. Any command that does not belong to the
//! cursor first finalizes the statement.

use crate::convert::{self, Conv, Param, declared_kind, storage_class_kind, type_params};
use crate::error::{ErrContext, SqliteResultExt, map_error_ctx};
use crate::lexer::{self, StatementKind};
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::value::{ColumnMeta, ColumnSource, DataKind};
use dbiji_core::{DbError, DbResult, ExecOptions, ResultHeader, Row, TransactionMode, Value};
use rusqlite::{Connection, OpenFlags, Statement};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Upper bound of the bytes returned by one `fetch` (rows are returned early
/// when the batch grows beyond it, at least one row per call).
const FETCH_BYTE_LIMIT: usize = 32 * 1024 * 1024;

/// Reply channel of a command.
pub(crate) type Reply<T> = oneshot::Sender<DbResult<T>>;

/// State shared by the session, its cursors, the cancel handle and the thread.
pub(crate) struct Shared {
    pub interrupt: rusqlite::InterruptHandle,
    /// Set by [`Shared::cancel`]; checked by the busy and progress handlers.
    pub cancel_requested: AtomicBool,
    /// `!sqlite3_get_autocommit()`, refreshed after every command.
    pub in_transaction: AtomicBool,
    /// False once the connection thread has exited.
    pub alive: AtomicBool,
}

impl Shared {
    /// Interrupt the running statement (`sqlite3_interrupt`) and abort lock waits.
    pub fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
    }
}

/// Work sent to the connection thread.
pub(crate) enum Command {
    /// Run a closure with the connection (metadata, transactions ...).
    Call(Box<dyn FnOnce(&mut Worker) + Send>),
    /// Execute SQL and serve the resulting cursor.
    Execute(Box<ExecRequest>),
    Fetch {
        cursor: u64,
        max_rows: usize,
        reply: Reply<FetchReply>,
    },
    NextResult {
        cursor: u64,
        reply: Reply<Option<ResultHeader>>,
    },
    /// Close a cursor; `reply` is `None` when the cursor was dropped.
    CloseCursor {
        cursor: u64,
        reply: Option<Reply<()>>,
    },
    /// Close the connection and stop the thread.
    Shutdown { reply: Reply<()> },
}

/// Parameters of [`Command::Execute`].
pub(crate) struct ExecRequest {
    pub cursor: u64,
    pub sql: String,
    pub params: Vec<Value>,
    pub opts: ExecOptions,
    /// Receives the first result header (or `None` for an empty batch).
    pub reply: Reply<Option<ResultHeader>>,
}

/// Rows returned by one fetch; `done` tells that the row set is exhausted.
pub(crate) struct FetchReply {
    pub rows: Vec<Row>,
    pub done: bool,
}

/// Connection settings derived from [`dbiji_core::ConnectParams`].
#[derive(Debug, Clone)]
pub(crate) struct OpenConfig {
    /// File path, `file:` URI or `:memory:`.
    pub path: String,
    pub read_only: bool,
    pub create: bool,
    pub foreign_keys: bool,
    pub journal_mode: Option<String>,
    pub synchronous: Option<String>,
    pub cache_size: Option<i64>,
    pub busy_timeout: Duration,
    /// Extra `PRAGMA name = value` (validated).
    pub pragmas: Vec<(String, String)>,
    /// `ATTACH DATABASE path AS alias`.
    pub attach: Vec<(String, String)>,
    pub mode: TransactionMode,
    pub show_system_objects: bool,
}

impl OpenConfig {
    pub fn is_memory(&self) -> bool {
        self.path == ":memory:"
            || self.path.starts_with("file::memory:")
            || self.path.contains("mode=memory")
    }
}

thread_local! {
    static THREAD_SHARED: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
    static BUSY_TIMEOUT_MS: Cell<u64> = const { Cell::new(5000) };
    static BUSY_STARTED: Cell<Option<Instant>> = const { Cell::new(None) };
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static TIMED_OUT: Cell<bool> = const { Cell::new(false) };
}

fn cancel_requested() -> bool {
    THREAD_SHARED.with(|s| {
        s.borrow()
            .as_ref()
            .is_some_and(|s| s.cancel_requested.load(Ordering::SeqCst))
    })
}

/// Busy handler: waits like SQLite's default handler up to the busy timeout,
/// but gives up immediately when the statement is cancelled or its timeout
/// elapses.
fn busy_handler(count: i32) -> bool {
    if cancel_requested() {
        return false;
    }
    let now = Instant::now();
    let deadline = DEADLINE.get();
    if deadline.is_some_and(|d| now >= d) {
        TIMED_OUT.set(true);
        return false;
    }
    let timeout = Duration::from_millis(BUSY_TIMEOUT_MS.get());
    let started = if count <= 0 {
        BUSY_STARTED.set(Some(now));
        now
    } else {
        BUSY_STARTED.get().unwrap_or(now)
    };
    let elapsed = now.saturating_duration_since(started);
    if elapsed >= timeout {
        return false;
    }
    const DELAYS_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
    let idx = usize::try_from(count).unwrap_or(0).min(DELAYS_MS.len() - 1);
    let mut delay = Duration::from_millis(DELAYS_MS[idx]).min(timeout - elapsed);
    if let Some(d) = deadline {
        delay = delay.min(d.saturating_duration_since(now));
    }
    std::thread::sleep(delay);
    true
}

/// Progress handler: interrupts the statement when it was cancelled before
/// it started running or when its timeout elapsed.
fn progress_handler(shared: &Shared) -> bool {
    if shared.cancel_requested.load(Ordering::Relaxed) {
        return true;
    }
    match DEADLINE.get() {
        Some(d) if Instant::now() >= d => {
            TIMED_OUT.set(true);
            true
        }
        _ => false,
    }
}

fn valid_pragma_value(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-+.".contains(c))
}

fn valid_pragma_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn open_error(e: rusqlite::Error, cfg: &OpenConfig) -> DbError {
    let path = std::path::Path::new(&cfg.path);
    let base = crate::error::map_error(e);
    if cfg.is_memory() || cfg.path.starts_with("file:") {
        return DbError::io(format!(
            "Cannot open the database '{}': {}",
            cfg.path, base.message
        ));
    }
    let exists = path.exists();
    let dir_missing = path
        .parent()
        .is_some_and(|p| !p.as_os_str().is_empty() && !p.exists());
    let msg = if !exists && cfg.read_only {
        format!(
            "The database file '{}' does not exist (a read-only connection cannot create it)",
            cfg.path
        )
    } else if !exists && !cfg.create {
        format!("The database file '{}' does not exist", cfg.path)
    } else if !exists && dir_missing {
        format!(
            "Cannot create the database file '{}': the directory does not exist",
            cfg.path
        )
    } else if path.is_dir() {
        format!("'{}' is a directory, not an SQLite database file", cfg.path)
    } else {
        format!(
            "Cannot open the database file '{}': {}",
            cfg.path, base.message
        )
    };
    let mut err = DbError::io(msg);
    err.code = base.code;
    err
}

fn open_connection(cfg: &OpenConfig) -> DbResult<Connection> {
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if cfg.read_only {
        flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
    } else {
        flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
        if cfg.create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
    }
    let conn = if cfg.path == ":memory:" {
        Connection::open_in_memory_with_flags(flags)
    } else {
        Connection::open_with_flags(&cfg.path, flags)
    };
    conn.map_err(|e| open_error(e, cfg))
}

fn configure(conn: &Connection, cfg: &OpenConfig, shared: &Arc<Shared>) -> DbResult<()> {
    conn.busy_handler(Some(busy_handler as fn(i32) -> bool))
        .db()?;
    let progress_shared = Arc::clone(shared);
    conn.progress_handler(1000, Some(move || progress_handler(&progress_shared)))
        .db()?;
    // Reads the file header: fails early for files that are not databases.
    conn.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
        .map_err(|e| {
            let err = crate::error::map_error(e);
            let mut out = DbError::io(format!(
                "Cannot open the database '{}': {}",
                cfg.path, err.message
            ));
            out.code = err.code;
            out
        })?;
    conn.execute_batch(if cfg.foreign_keys {
        "PRAGMA foreign_keys = ON"
    } else {
        "PRAGMA foreign_keys = OFF"
    })
    .db()?;
    if let Some(mode) = &cfg.journal_mode
        && !cfg.read_only
        && !cfg.is_memory()
    {
        let wanted = mode.to_ascii_lowercase();
        let got: String = conn
            .query_row(&format!("PRAGMA journal_mode = {wanted}"), [], |r| r.get(0))
            .db()?;
        if !got.eq_ignore_ascii_case(&wanted) {
            tracing::warn!("SQLite kept journal_mode '{got}' instead of '{wanted}'");
        }
    }
    if let Some(s) = &cfg.synchronous {
        conn.execute_batch(&format!("PRAGMA synchronous = {s}"))
            .db()?;
    }
    if let Some(n) = cfg.cache_size {
        conn.execute_batch(&format!("PRAGMA cache_size = {n}"))
            .db()?;
    }
    for (name, value) in &cfg.pragmas {
        conn.execute_batch(&format!("PRAGMA {name} = {value}"))
            .db()?;
    }
    for (alias, path) in &cfg.attach {
        conn.execute(
            &format!(
                "ATTACH DATABASE ?1 AS {}",
                crate::dialect::quote_ident(alias)
            ),
            [path],
        )
        .map_err(|e| {
            let err = crate::error::map_error(e);
            DbError::io(format!(
                "Cannot attach '{path}' as '{alias}': {}",
                err.message
            ))
        })?;
    }
    if cfg.read_only {
        conn.execute_batch("PRAGMA query_only = ON").db()?;
    }
    Ok(())
}

/// Validate a journal mode value.
pub(crate) fn parse_journal_mode(v: &str) -> Option<String> {
    let u = v.trim().to_ascii_uppercase();
    matches!(
        u.as_str(),
        "DELETE" | "TRUNCATE" | "PERSIST" | "MEMORY" | "WAL" | "OFF"
    )
    .then_some(u)
}

/// Validate a synchronous value.
pub(crate) fn parse_synchronous(v: &str) -> Option<String> {
    let u = v.trim().to_ascii_uppercase();
    matches!(
        u.as_str(),
        "OFF" | "NORMAL" | "FULL" | "EXTRA" | "0" | "1" | "2" | "3"
    )
    .then_some(u)
}

/// Validate a generic pragma.
pub(crate) fn valid_pragma(name: &str, value: &str) -> bool {
    valid_pragma_name(name) && valid_pragma_value(value)
}

/// Sets `alive` to false when the thread ends (also on unwinding).
struct AliveGuard(Arc<Shared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
        self.0.in_transaction.store(false, Ordering::SeqCst);
    }
}

/// Entry point of the connection thread.
pub(crate) fn run(
    cfg: OpenConfig,
    rx: Receiver<Command>,
    ready: oneshot::Sender<DbResult<(Arc<Shared>, ServerInfo)>>,
) {
    BUSY_TIMEOUT_MS.set(u64::try_from(cfg.busy_timeout.as_millis()).unwrap_or(u64::MAX));
    let conn = match open_connection(&cfg) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let shared = Arc::new(Shared {
        interrupt: conn.get_interrupt_handle(),
        cancel_requested: AtomicBool::new(false),
        in_transaction: AtomicBool::new(false),
        alive: AtomicBool::new(true),
    });
    THREAD_SHARED.with(|s| *s.borrow_mut() = Some(Arc::clone(&shared)));
    let _guard = AliveGuard(Arc::clone(&shared));
    if let Err(e) = configure(&conn, &cfg, &shared) {
        let _ = ready.send(Err(e));
        return;
    }
    let version: String = match conn
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .db()
    {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let info = ServerInfo {
        product: "SQLite".into(),
        version_number: parse_version_number(&version),
        version,
        session_id: None,
    };
    let mut worker = Worker {
        conn,
        shared: Arc::clone(&shared),
        mode: cfg.mode,
        read_only: cfg.read_only,
        busy_timeout: cfg.busy_timeout,
        show_system_objects: cfg.show_system_objects,
    };
    worker.sync_state();
    if ready.send(Ok((Arc::clone(&shared), info))).is_err() {
        return;
    }
    let shutdown = worker.serve(&rx);
    let Worker { conn, .. } = worker;
    let result = conn.close().map_err(|(_, e)| crate::error::map_error(e));
    if let Some(reply) = shutdown {
        let _ = reply.send(result);
    }
    THREAD_SHARED.with(|s| *s.borrow_mut() = None);
}

/// What the main loop does after a command.
enum Flow {
    Continue,
    /// A command arrived while a cursor was open; process it next.
    Next(Command),
    Stop(Reply<()>),
}

/// Commands addressed to the open cursor.
enum CursorCmd {
    Fetch(usize, Reply<FetchReply>),
    Next(Reply<Option<ResultHeader>>),
    Close(Reply<()>),
    /// Cursor dropped or session gone.
    Discard,
    /// Any other command: the cursor is finalized and the command processed.
    Foreign(Command),
}

fn cursor_closed() -> DbError {
    DbError::query("The result set is closed (another statement was executed on this session)")
}

/// Convert the current row.
fn read_row(row: &rusqlite::Row<'_>, convs: &[Conv]) -> Row {
    let mut out = Vec::with_capacity(convs.len());
    for (i, conv) in convs.iter().enumerate() {
        out.push(match row.get_ref(i) {
            Ok(v) => convert::convert(v, *conv),
            Err(_) => Value::Null,
        });
    }
    out
}

fn row_bytes(row: &Row) -> usize {
    std::mem::size_of::<Row>() + row.iter().map(convert::value_bytes).sum::<usize>()
}

/// The connection owned by the thread plus session state.
pub(crate) struct Worker {
    pub conn: Connection,
    pub shared: Arc<Shared>,
    pub mode: TransactionMode,
    pub read_only: bool,
    pub busy_timeout: Duration,
    pub show_system_objects: bool,
}

impl Worker {
    fn serve(&mut self, rx: &Receiver<Command>) -> Option<Reply<()>> {
        let mut pending: Option<Command> = None;
        loop {
            let cmd = match pending.take() {
                Some(c) => c,
                None => match rx.recv() {
                    Ok(c) => c,
                    Err(_) => return None,
                },
            };
            let flow = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle(cmd, rx)));
            self.end_command();
            self.sync_state();
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::Next(c)) => pending = Some(c),
                Ok(Flow::Stop(reply)) => return Some(reply),
                Err(_) => tracing::error!("SQLite connection thread recovered from a panic"),
            }
        }
    }

    fn handle(&mut self, cmd: Command, rx: &Receiver<Command>) -> Flow {
        match cmd {
            Command::Call(f) => {
                self.begin_command(None);
                f(self);
                Flow::Continue
            }
            Command::Execute(req) => match self.serve_cursor(*req, rx) {
                Some(next) => Flow::Next(next),
                None => Flow::Continue,
            },
            Command::Fetch { reply, .. } => {
                let _ = reply.send(Err(cursor_closed()));
                Flow::Continue
            }
            Command::NextResult { reply, .. } => {
                let _ = reply.send(Err(cursor_closed()));
                Flow::Continue
            }
            Command::CloseCursor { reply, .. } => {
                if let Some(r) = reply {
                    let _ = r.send(Ok(()));
                }
                Flow::Continue
            }
            Command::Shutdown { reply } => Flow::Stop(reply),
        }
    }

    /// Refresh the shared transaction flag.
    pub fn sync_state(&self) {
        self.shared
            .in_transaction
            .store(!self.conn.is_autocommit(), Ordering::SeqCst);
    }

    /// Reset per-command cancellation state and arm the statement timeout.
    pub fn begin_command(&self, timeout: Option<Duration>) {
        self.shared.cancel_requested.store(false, Ordering::SeqCst);
        TIMED_OUT.set(false);
        BUSY_STARTED.set(None);
        DEADLINE.set(timeout.filter(|t| !t.is_zero()).map(|t| Instant::now() + t));
    }

    pub fn end_command(&self) {
        DEADLINE.set(None);
    }

    /// Error context for the current command.
    pub fn err_ctx<'a>(&self, sql: Option<&'a str>, offset: usize) -> ErrContext<'a> {
        ErrContext {
            sql,
            offset,
            busy_timeout_ms: u64::try_from(self.busy_timeout.as_millis()).unwrap_or(u64::MAX),
            cancelled: self.shared.cancel_requested.load(Ordering::SeqCst),
            timed_out: TIMED_OUT.get(),
        }
    }

    /// Map an error of the current command.
    pub fn map_err(&self, e: rusqlite::Error) -> DbError {
        map_error_ctx(e, &self.err_ctx(None, 0))
    }

    /// Execute statements without results (transaction control).
    pub fn exec_batch(&self, sql: &str) -> DbResult<()> {
        self.conn.execute_batch(sql).map_err(|e| self.map_err(e))
    }

    /// Wait for the next command addressed to cursor `id`.
    fn recv_cursor(&self, id: u64, rx: &Receiver<Command>) -> CursorCmd {
        loop {
            let cmd = match rx.recv() {
                Ok(c) => c,
                Err(_) => return CursorCmd::Discard,
            };
            match cmd {
                Command::Fetch {
                    cursor,
                    max_rows,
                    reply,
                } if cursor == id => return CursorCmd::Fetch(max_rows, reply),
                Command::NextResult { cursor, reply } if cursor == id => {
                    return CursorCmd::Next(reply);
                }
                Command::CloseCursor { cursor, reply } if cursor == id => {
                    return match reply {
                        Some(r) => CursorCmd::Close(r),
                        None => CursorCmd::Discard,
                    };
                }
                Command::Fetch { reply, .. } => {
                    let _ = reply.send(Err(cursor_closed()));
                }
                Command::NextResult { reply, .. } => {
                    let _ = reply.send(Err(cursor_closed()));
                }
                Command::CloseCursor { reply, .. } => {
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                other => return CursorCmd::Foreign(other),
            }
        }
    }

    /// The cursor has no current row set: fetches return nothing,
    /// `next_result` returns `next` (end of results after the last one).
    fn wait_without_rows(
        &self,
        id: u64,
        rx: &Receiver<Command>,
    ) -> Result<Reply<Option<ResultHeader>>, Option<Command>> {
        loop {
            match self.recv_cursor(id, rx) {
                CursorCmd::Fetch(_, reply) => {
                    let _ = reply.send(Ok(FetchReply {
                        rows: Vec::new(),
                        done: true,
                    }));
                }
                CursorCmd::Next(reply) => return Ok(reply),
                CursorCmd::Close(reply) => {
                    let _ = reply.send(Ok(()));
                    return Err(None);
                }
                CursorCmd::Discard => return Err(None),
                CursorCmd::Foreign(cmd) => return Err(Some(cmd)),
            }
        }
    }

    /// After the batch finished (or failed): serve the cursor until it is closed.
    fn finish_cursor(&self, id: u64, rx: &Receiver<Command>) -> Option<Command> {
        loop {
            match self.wait_without_rows(id, rx) {
                Ok(reply) => {
                    let _ = reply.send(Ok(None));
                }
                Err(next) => return next,
            }
        }
    }

    fn update_header(&self, kind: &StatementKind) -> ResultHeader {
        let (rows_affected, last_insert_id) = if kind.is_dml() {
            let changes = self.conn.changes();
            let id = if *kind == StatementKind::Insert && changes > 0 {
                u64::try_from(self.conn.last_insert_rowid()).ok()
            } else {
                None
            };
            (Some(changes), id)
        } else {
            (None, None)
        };
        let tag = kind.tag();
        ResultHeader::Update {
            rows_affected,
            last_insert_id,
            tag: (!tag.is_empty()).then_some(tag),
        }
    }

    /// `INTEGER PRIMARY KEY` column of a rowid table?
    fn is_rowid_alias(
        &self,
        db: &str,
        table: &str,
        column: &str,
        cache: &mut HashMap<(String, String), Option<String>>,
    ) -> bool {
        let key = (db.to_string(), table.to_string());
        let alias = cache.entry(key).or_insert_with(|| {
            if self.conn.column_metadata(Some(db), table, "rowid").is_err() {
                return None; // WITHOUT ROWID
            }
            let mut stmt = self
                .conn
                .prepare("SELECT name, type FROM pragma_table_xinfo(?1, ?2) WHERE pk > 0")
                .ok()?;
            let pks: Vec<(String, String)> = stmt
                .query_map([table, db], |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })
                .ok()?
                .filter_map(Result::ok)
                .collect();
            match pks.as_slice() {
                [(name, ty)] if ty.eq_ignore_ascii_case("INTEGER") => Some(name.clone()),
                _ => None,
            }
        });
        alias
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(column))
    }

    /// Result column descriptions and per-column conversions.
    fn describe(
        &self,
        stmt: &Statement<'_>,
        resolve: bool,
    ) -> (Vec<ColumnMeta>, Vec<Conv>, Vec<bool>) {
        let decls: Vec<(String, Option<String>)> = stmt
            .columns()
            .iter()
            .map(|c| (c.name().to_string(), c.decl_type().map(str::to_string)))
            .collect();
        let origins: Vec<(Option<String>, Option<String>, Option<String>)> = stmt
            .columns_with_metadata()
            .iter()
            .map(|m| {
                (
                    m.database_name().map(str::to_string),
                    m.table_name().map(str::to_string),
                    m.origin_name().map(str::to_string),
                )
            })
            .collect();
        let mut cache = HashMap::new();
        let mut metas = Vec::with_capacity(decls.len());
        let mut convs = Vec::with_capacity(decls.len());
        let mut undeclared = Vec::with_capacity(decls.len());
        for (i, (name, decl)) in decls.into_iter().enumerate() {
            let kind = decl
                .as_deref()
                .map(declared_kind)
                .unwrap_or(DataKind::Other);
            let (precision, scale, max_length) = decl
                .as_deref()
                .map(|d| type_params(d, kind))
                .unwrap_or_default();
            let mut meta = ColumnMeta::new(name, decl.clone().unwrap_or_default(), kind);
            meta.precision = precision;
            meta.scale = scale;
            meta.max_length = max_length;
            if let Some((Some(db), Some(table), Some(column))) = origins.get(i).cloned() {
                let is_rowid = ["rowid", "oid", "_rowid_"]
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(&column));
                if let Ok((_, _, not_null, pk, autoinc)) =
                    self.conn
                        .column_metadata(Some(db.as_str()), table.as_str(), column.as_str())
                {
                    // the hidden rowid itself (not a column that happens to be named "rowid")
                    let real_rowid = is_rowid && !self.table_has_column(&db, &table, &column);
                    meta.nullable = Some(!not_null && !real_rowid);
                    meta.auto_increment = if autoinc || real_rowid {
                        Some(true)
                    } else if !pk {
                        Some(false)
                    } else if resolve {
                        // an INTEGER PRIMARY KEY is assigned automatically
                        Some(self.is_rowid_alias(&db, &table, &column, &mut cache))
                    } else {
                        None
                    };
                }
                meta.source = Some(ColumnSource {
                    database: None,
                    schema: Some(db),
                    table,
                    column,
                });
            }
            undeclared.push(decl.is_none());
            convs.push(Conv::for_kind(kind));
            metas.push(meta);
        }
        (metas, convs, undeclared)
    }

    /// The table really has a column named `column` (and not just the rowid alias).
    fn table_has_column(&self, db: &str, table: &str, column: &str) -> bool {
        let Ok(mut stmt) = self.conn.prepare_cached(
            "SELECT 1 FROM pragma_table_xinfo(?1, ?2) WHERE name = ?3 COLLATE NOCASE",
        ) else {
            return false;
        };
        stmt.exists([table, db, column]).unwrap_or(false)
    }

    /// Execute `req` and serve its cursor until it is closed. Returns a
    /// command that arrived in the meantime and does not belong to the cursor.
    fn serve_cursor(&self, req: ExecRequest, rx: &Receiver<Command>) -> Option<Command> {
        let ExecRequest {
            cursor: id,
            sql,
            params,
            opts,
            reply,
        } = req;
        let statements = lexer::split_statements(&sql);
        let single = statements.len() == 1;
        let mut param_pos = 0usize;
        let mut header_reply = reply;
        self.begin_command(opts.timeout);
        for span in &statements {
            let kind = lexer::classify(span.text);
            let fail = |w: &Worker, e: DbError, reply: Reply<Option<ResultHeader>>| {
                let _ = reply.send(Err(e));
                w.end_command();
                w.sync_state();
            };
            let mut stmt = match self.conn.prepare(span.text) {
                Ok(s) => s,
                Err(e) => {
                    let err = map_error_ctx(e, &self.err_ctx(Some(&sql), span.offset));
                    fail(self, err, header_reply);
                    return self.finish_cursor(id, rx);
                }
            };
            if self.read_only && !stmt.readonly() && !kind.runs_outside_transaction() {
                drop(stmt);
                fail(
                    self,
                    DbError::read_only(
                        "The connection is read-only; the statement would modify the database",
                    ),
                    header_reply,
                );
                return self.finish_cursor(id, rx);
            }
            let n = stmt.parameter_count();
            let available = params.len().saturating_sub(param_pos);
            if (single && n != params.len()) || n > available {
                drop(stmt);
                let msg = if single {
                    format!(
                        "The statement expects {n} parameter(s) but {} were supplied",
                        params.len()
                    )
                } else {
                    format!(
                        "Statement at position {} expects {n} parameter(s) but only {available} remain",
                        span.offset + 1
                    )
                };
                fail(self, DbError::query(msg), header_reply);
                return self.finish_cursor(id, rx);
            }
            let mut bind_error = None;
            for i in 0..n {
                if let Err(e) = stmt.raw_bind_parameter(i + 1, Param(&params[param_pos + i])) {
                    bind_error = Some(map_error_ctx(e, &self.err_ctx(Some(&sql), span.offset)));
                    break;
                }
            }
            param_pos += n;
            if let Some(err) = bind_error {
                drop(stmt);
                fail(self, err, header_reply);
                return self.finish_cursor(id, rx);
            }
            // implicit transaction in manual commit mode (after a successful
            // prepare, so that a syntax error does not leave a transaction open)
            if self.mode == TransactionMode::Manual
                && self.conn.is_autocommit()
                && !kind.runs_outside_transaction()
                && let Err(e) = self.conn.execute_batch("BEGIN")
            {
                let err = map_error_ctx(e, &self.err_ctx(Some(&sql), span.offset));
                drop(stmt);
                fail(self, err, header_reply);
                return self.finish_cursor(id, rx);
            }

            if stmt.column_count() == 0 {
                let result = stmt.raw_execute();
                drop(stmt);
                match result {
                    Ok(_) => {
                        let header = self.update_header(&kind);
                        self.sync_state();
                        self.end_command();
                        let _ = header_reply.send(Ok(Some(header)));
                    }
                    Err(e) => {
                        let err = map_error_ctx(e, &self.err_ctx(Some(&sql), span.offset));
                        fail(self, err, header_reply);
                        return self.finish_cursor(id, rx);
                    }
                }
                match self.wait_without_rows(id, rx) {
                    Ok(next) => {
                        header_reply = next;
                        self.begin_command(opts.timeout);
                        continue;
                    }
                    Err(next) => return next,
                }
            }

            // row result
            let (mut columns, convs, undeclared) = self.describe(&stmt, opts.resolve_sources);
            let mut rows = stmt.raw_query();
            let mut buffered: Option<Row> = None;
            let mut done = false;
            match rows.next() {
                Ok(Some(row)) => {
                    for (i, meta) in columns.iter_mut().enumerate() {
                        if undeclared.get(i).copied().unwrap_or(false)
                            && let Ok(v) = row.get_ref(i)
                        {
                            let (kind, type_name) = storage_class_kind(&v);
                            meta.kind = kind;
                            meta.type_name = type_name.to_string();
                        }
                    }
                    buffered = Some(read_row(row, &convs));
                }
                Ok(None) => done = true,
                Err(e) => {
                    let err = map_error_ctx(e, &self.err_ctx(Some(&sql), span.offset));
                    drop(rows);
                    drop(stmt);
                    fail(self, err, header_reply);
                    return self.finish_cursor(id, rx);
                }
            }
            self.sync_state();
            self.end_command();
            let _ = header_reply.send(Ok(Some(ResultHeader::Rows(columns))));
            let mut pending_error: Option<DbError> = None;
            let next_reply = loop {
                match self.recv_cursor(id, rx) {
                    CursorCmd::Fetch(max_rows, reply) => {
                        if let Some(e) = pending_error.take() {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        self.begin_command(opts.timeout);
                        let max = max_rows.max(1);
                        let mut out: Vec<Row> = Vec::with_capacity(max.min(4096));
                        let mut bytes = 0usize;
                        if let Some(r) = buffered.take() {
                            bytes += row_bytes(&r);
                            out.push(r);
                        }
                        while !done && out.len() < max && bytes < FETCH_BYTE_LIMIT {
                            match rows.next() {
                                Ok(Some(row)) => {
                                    let r = read_row(row, &convs);
                                    bytes += row_bytes(&r);
                                    out.push(r);
                                }
                                Ok(None) => done = true,
                                Err(e) => {
                                    done = true;
                                    pending_error = Some(map_error_ctx(
                                        e,
                                        &self.err_ctx(Some(&sql), span.offset),
                                    ));
                                }
                            }
                        }
                        self.end_command();
                        self.sync_state();
                        if out.is_empty()
                            && let Some(e) = pending_error.take()
                        {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        let exhausted = done && buffered.is_none() && pending_error.is_none();
                        let _ = reply.send(Ok(FetchReply {
                            rows: out,
                            done: exhausted,
                        }));
                    }
                    CursorCmd::Next(reply) => break reply,
                    CursorCmd::Close(reply) => {
                        drop(rows);
                        drop(stmt);
                        self.sync_state();
                        let _ = reply.send(Ok(()));
                        return None;
                    }
                    CursorCmd::Discard => return None,
                    CursorCmd::Foreign(cmd) => return Some(cmd),
                }
            };
            drop(rows);
            drop(stmt);
            self.sync_state();
            header_reply = next_reply;
            self.begin_command(opts.timeout);
        }
        let _ = header_reply.send(Ok(None));
        self.end_command();
        self.finish_cursor(id, rx)
    }
}
