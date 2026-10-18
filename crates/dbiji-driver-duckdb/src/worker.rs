//! The dedicated connection thread of a DuckDB session.
//!
//! Like the SQLite driver, each session owns one OS thread with its
//! `duckdb::Connection`. Row results are executed in DuckDB's streaming mode
//! and fetched chunk by chunk when the cursor asks for rows, so memory stays
//! bounded. Any command that does not belong to the open cursor first drops
//! the streaming result (DuckDB closes it anyway when another query starts).
//!
//! DuckDB must not open the same file twice in one process (two instances
//! would each keep their own buffer pool and WAL), so sessions on the same
//! file share one database instance through a process wide registry and
//! connect with `Connection::try_clone`.

use crate::convert::{self, Conv, Param, conv_of, kind_of, type_name};
use crate::error::{DuckResultExt, ErrContext, map_error, map_error_ctx};
use crate::lexer::{self, SelectItem, StatementKind};
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::value::{ColumnMeta, ColumnSource};
use dbiji_core::{DbError, DbResult, ExecOptions, ResultHeader, Row, TransactionMode, Value};
use duckdb::core::LogicalTypeId;
use duckdb::{AccessMode, Config, Connection, InterruptHandle, Statement, params_from_iter};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Upper bound of the bytes returned by one fetch (at least one row).
const FETCH_BYTE_LIMIT: usize = 32 * 1024 * 1024;

/// After a statement timeout fired, the interrupt is repeated at this
/// interval until the statement ends (DuckDB clears a pending interrupt when
/// a statement starts, so a single interrupt can be lost).
const REFIRE_INTERVAL: Duration = Duration::from_millis(50);

/// Reply channel of a command.
pub(crate) type Reply<T> = oneshot::Sender<DbResult<T>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------ registry

/// One DuckDB database instance shared by all sessions on the same file.
pub(crate) struct SharedDb {
    /// Connection kept to clone new session connections from.
    root: Mutex<Option<Connection>>,
    read_only: bool,
}

impl Drop for SharedDb {
    fn drop(&mut self) {
        // Close the instance while holding the registry lock so that a
        // concurrent open of the same file waits until it is fully closed.
        let _registry = lock(&REGISTRY);
        let root = lock(&self.root).take();
        drop(root);
    }
}

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<SharedDb>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Connection settings derived from the connect parameters.
#[derive(Debug, Clone)]
pub(crate) struct OpenConfig {
    /// File path or `:memory:`.
    pub path: String,
    pub read_only: bool,
    pub threads: Option<i64>,
    pub max_memory: Option<String>,
    pub autoload_extensions: bool,
    /// Extra database settings passed to the DuckDB configuration.
    pub settings: Vec<(String, String)>,
    pub mode: TransactionMode,
    pub show_system_objects: bool,
}

impl OpenConfig {
    pub fn is_memory(&self) -> bool {
        self.path.is_empty() || self.path.starts_with(":memory:")
    }
}

fn open_error(e: duckdb::Error, cfg: &OpenConfig) -> DbError {
    let err = map_error(e);
    let message = format!(
        "Cannot open the DuckDB database '{}': {}",
        cfg.path, err.message
    );
    // invalid settings are reported when the database opens
    let mut out = if err.code.as_deref() == Some("INVALID_INPUT") {
        DbError::config(message)
    } else {
        DbError::io(message)
    };
    out.code = err.code;
    out
}

fn config(cfg: &OpenConfig) -> DbResult<Config> {
    let c = |e: duckdb::Error| {
        DbError::config(format!("Invalid DuckDB setting: {}", map_error(e).message))
    };
    let mut config = Config::default()
        .enable_autoload_extension(cfg.autoload_extensions)
        .map_err(c)?;
    if cfg.read_only {
        config = config.access_mode(AccessMode::ReadOnly).map_err(c)?;
    }
    if let Some(t) = cfg.threads {
        config = config.threads(t).map_err(c)?;
    }
    if let Some(m) = &cfg.max_memory {
        config = config.max_memory(m).map_err(c)?;
    }
    for (k, v) in &cfg.settings {
        config = config.with(k, v).map_err(c)?;
    }
    Ok(config)
}

/// Registry key of a database file: its canonical path (for a file that does
/// not exist yet, the canonical directory plus the file name).
fn registry_key(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if let Ok(c) = std::fs::canonicalize(&p) {
        return c;
    }
    let abs = std::path::absolute(&p).unwrap_or(p);
    match (abs.parent(), abs.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or(abs.clone()),
        _ => abs,
    }
}

/// Join an already open instance.
fn join_shared(db: &SharedDb, cfg: &OpenConfig) -> DbResult<Connection> {
    if db.read_only != cfg.read_only {
        return Err(DbError::config(format!(
            "The DuckDB database '{}' is already open {} in this application; DuckDB allows one instance per \
             file, so all connections to it must use the same read-only setting",
            cfg.path,
            if db.read_only {
                "read-only"
            } else {
                "read-write"
            }
        )));
    }
    match lock(&db.root).as_ref() {
        Some(r) => r.try_clone().map_err(|e| open_error(e, cfg)),
        None => Err(DbError::internal("The shared DuckDB instance is closing")),
    }
}

/// Open (or join) the database instance and create this session's connection.
pub(crate) fn open_shared(cfg: &OpenConfig) -> DbResult<(Arc<SharedDb>, Connection)> {
    if cfg.is_memory() {
        if cfg.read_only {
            return Err(DbError::config(
                "An in-memory DuckDB database cannot be opened read-only",
            ));
        }
        let conn =
            Connection::open_in_memory_with_flags(config(cfg)?).map_err(|e| open_error(e, cfg))?;
        let db = Arc::new(SharedDb {
            root: Mutex::new(None),
            read_only: false,
        });
        return Ok((db, conn));
    }
    let key = registry_key(&cfg.path);
    let mut registry = lock(&REGISTRY);
    registry.retain(|_, w| w.strong_count() > 0);
    if let Some(db) = registry.get(&key).and_then(Weak::upgrade) {
        // release the registry before `db` can be dropped (its Drop locks it)
        drop(registry);
        let conn = join_shared(&db, cfg)?;
        return Ok((db, conn));
    }
    if cfg.read_only && !std::path::Path::new(&cfg.path).exists() {
        return Err(DbError::io(format!(
            "The DuckDB database file '{}' does not exist (a read-only connection cannot create it)",
            cfg.path
        )));
    }
    let conn =
        Connection::open_with_flags(&cfg.path, config(cfg)?).map_err(|e| open_error(e, cfg))?;
    let root = conn.try_clone().map_err(|e| open_error(e, cfg))?;
    let db = Arc::new(SharedDb {
        root: Mutex::new(Some(root)),
        read_only: cfg.read_only,
    });
    // the file exists now: its canonical path may differ from the lookup key
    let created_key = registry_key(&cfg.path);
    if created_key != key {
        registry.insert(created_key, Arc::downgrade(&db));
    }
    registry.insert(key, Arc::downgrade(&db));
    drop(registry);
    Ok((db, conn))
}

/// Number of live database instances (for tests).
#[cfg(test)]
pub(crate) fn open_instances() -> usize {
    let live: std::collections::HashSet<usize> = lock(&REGISTRY)
        .values()
        .filter(|w| w.strong_count() > 0)
        .map(|w| w.as_ptr() as usize)
        .collect();
    live.len()
}

// ------------------------------------------------------------------ watchdog

/// Interrupts the running statement when its timeout elapses.
pub(crate) struct Watchdog {
    state: Mutex<WatchState>,
    cv: Condvar,
    interrupt: Arc<InterruptHandle>,
    fired: AtomicBool,
}

#[derive(Default)]
struct WatchState {
    deadline: Option<Instant>,
    stop: bool,
    started: bool,
}

impl Watchdog {
    fn new(interrupt: Arc<InterruptHandle>) -> Arc<Self> {
        Arc::new(Watchdog {
            state: Mutex::new(WatchState::default()),
            cv: Condvar::new(),
            interrupt,
            fired: AtomicBool::new(false),
        })
    }

    fn arm(self: &Arc<Self>, timeout: Duration) {
        let mut st = lock(&self.state);
        st.deadline = Some(Instant::now() + timeout);
        self.fired.store(false, Ordering::SeqCst);
        if !st.started {
            st.started = true;
            let me = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("dbiji-duckdb-timeout".into())
                .spawn(move || me.run());
            if spawned.is_err() {
                st.started = false;
                tracing::warn!(
                    "cannot start the DuckDB statement timeout thread; timeouts are disabled"
                );
            }
        }
        self.cv.notify_all();
    }

    fn disarm(&self) {
        let mut st = lock(&self.state);
        st.deadline = None;
        self.cv.notify_all();
    }

    fn stop(&self) {
        let mut st = lock(&self.state);
        st.stop = true;
        self.cv.notify_all();
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    fn run(self: Arc<Self>) {
        let mut st = lock(&self.state);
        loop {
            if st.stop {
                return;
            }
            match st.deadline {
                None => st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        self.fired.store(true, Ordering::SeqCst);
                        self.interrupt.interrupt();
                        st.deadline = Some(now + REFIRE_INTERVAL);
                    } else {
                        st = self
                            .cv
                            .wait_timeout(st, d - now)
                            .map(|(g, _)| g)
                            .unwrap_or_else(|e| e.into_inner().0);
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------- shared

/// State shared by the session, its cursors, the cancel handle and the thread.
pub(crate) struct Shared {
    pub interrupt: Arc<InterruptHandle>,
    /// Incremented by every cancel request; a request is cancelled when the
    /// sequence changed after it was sent.
    pub cancel_seq: AtomicU64,
    pub in_transaction: AtomicBool,
    pub alive: AtomicBool,
    pub watchdog: Arc<Watchdog>,
}

impl Shared {
    /// Cancel the running (or about to run) statement.
    pub fn cancel(&self) {
        self.cancel_seq.fetch_add(1, Ordering::SeqCst);
        self.interrupt.interrupt();
    }
}

/// Work sent to the connection thread.
pub(crate) enum Command {
    Call(Box<dyn FnOnce(&mut Worker) + Send>),
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
    CloseCursor {
        cursor: u64,
        reply: Option<Reply<()>>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

pub(crate) struct ExecRequest {
    pub cursor: u64,
    pub sql: String,
    pub params: Vec<Value>,
    pub opts: ExecOptions,
    /// [`Shared::cancel_seq`] when the request was sent.
    pub cancel_seq: u64,
    pub reply: Reply<Option<ResultHeader>>,
}

pub(crate) struct FetchReply {
    pub rows: Vec<Row>,
    pub done: bool,
}

struct AliveGuard(Arc<Shared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
        self.0.in_transaction.store(false, Ordering::SeqCst);
        self.0.watchdog.stop();
    }
}

/// Entry point of the connection thread.
pub(crate) fn run(
    cfg: OpenConfig,
    rx: Receiver<Command>,
    ready: oneshot::Sender<DbResult<(Arc<Shared>, ServerInfo)>>,
) {
    let (db, conn) = match open_shared(&cfg) {
        Ok(x) => x,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let interrupt = conn.interrupt_handle();
    let shared = Arc::new(Shared {
        watchdog: Watchdog::new(Arc::clone(&interrupt)),
        interrupt,
        cancel_seq: AtomicU64::new(0),
        in_transaction: AtomicBool::new(false),
        alive: AtomicBool::new(true),
    });
    let guard = AliveGuard(Arc::clone(&shared));
    let version: String = match conn.query_row("SELECT version()", [], |r| r.get(0)).db() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let info = ServerInfo {
        product: "DuckDB".into(),
        version_number: parse_version_number(&version),
        version,
        session_id: None,
    };
    let mut worker = Worker {
        conn,
        shared: Arc::clone(&shared),
        mode: cfg.mode,
        in_tx: false,
        show_system_objects: cfg.show_system_objects,
    };
    if ready.send(Ok((Arc::clone(&shared), info))).is_err() {
        return;
    }
    let shutdown = worker.serve(&rx);
    let Worker { conn, .. } = worker;
    let result = conn.close().map_err(|(_, e)| map_error(e));
    drop(guard);
    drop(db);
    if let Some(reply) = shutdown {
        let _ = reply.send(result);
    }
}

enum Flow {
    Continue,
    Next(Command),
    Stop(Reply<()>),
}

enum CursorCmd {
    Fetch(usize, Reply<FetchReply>),
    Next(Reply<Option<ResultHeader>>),
    Close(Reply<()>),
    Discard,
    Foreign(Command),
}

fn cursor_closed() -> DbError {
    DbError::query("The result set is closed (another statement was executed on this session)")
}

fn row_bytes(row: &Row) -> usize {
    std::mem::size_of::<Row>() + row.iter().map(convert::value_bytes).sum::<usize>()
}

/// BIT values arrive in DuckDB's internal layout: a padding count byte,
/// then the bits with the padding at the start of the first byte.
fn bit_text(bytes: &[u8]) -> Value {
    let Some((&padding, data)) = bytes.split_first() else {
        return Value::text("");
    };
    let mut s = String::with_capacity(data.len() * 8);
    for b in data {
        s.push_str(&format!("{b:08b}"));
    }
    let skip = usize::from(padding).min(s.len());
    Value::text(&s[skip..])
}

fn read_row(row: &duckdb::Row<'_>, convs: &[(Conv, bool)]) -> Row {
    let mut out = Vec::with_capacity(convs.len());
    for (i, (conv, is_bit)) in convs.iter().enumerate() {
        out.push(match row.get_ref(i) {
            Ok(duckdb::types::ValueRef::Blob(b)) if *is_bit => bit_text(b),
            Ok(v) => convert::convert(v, *conv),
            Err(_) => Value::Null,
        });
    }
    out
}

/// Base table of a simple query: database, schema, table and its columns
/// (name, nullable, auto increment).
struct SourceTable {
    database: String,
    schema: String,
    table: String,
    columns: Vec<(String, bool, bool)>,
}

/// Result type of a statement as executed by the driver.
fn returns_rows(kind: &StatementKind, stmt: &str) -> bool {
    match kind {
        k if k.is_dml() => lexer::has_returning(stmt),
        StatementKind::Ddl(_)
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Rollback => false,
        StatementKind::Session(word) => matches!(word.as_str(), "PRAGMA" | "EXPLAIN"),
        StatementKind::Other(word) => !matches!(word.as_str(), "COPY" | "EXPORT" | "IMPORT"),
        _ => true,
    }
}

/// The connection owned by the thread plus session state.
pub(crate) struct Worker {
    pub conn: Connection,
    pub shared: Arc<Shared>,
    pub mode: TransactionMode,
    /// An explicit or implicit transaction is open (DuckDB's C API does not
    /// report the transaction state, so the driver tracks it).
    pub in_tx: bool,
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
            self.shared.watchdog.disarm();
            self.sync_state();
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::Next(c)) => pending = Some(c),
                Ok(Flow::Stop(reply)) => return Some(reply),
                Err(_) => tracing::error!("DuckDB connection thread recovered from a panic"),
            }
        }
    }

    fn handle(&mut self, cmd: Command, rx: &Receiver<Command>) -> Flow {
        match cmd {
            Command::Call(f) => {
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

    pub fn sync_state(&self) {
        self.shared
            .in_transaction
            .store(self.in_tx, Ordering::SeqCst);
    }

    /// Reset the timeout state for the next step and arm the timeout.
    fn arm(&self, timeout: Option<Duration>) {
        self.shared.watchdog.fired.store(false, Ordering::SeqCst);
        if let Some(t) = timeout.filter(|t| !t.is_zero()) {
            self.shared.watchdog.arm(t);
        }
    }

    fn disarm(&self) {
        self.shared.watchdog.disarm();
    }

    fn cancelled(&self, seq: u64) -> bool {
        self.shared.cancel_seq.load(Ordering::SeqCst) != seq
    }

    /// Error context of the statement `stmt` at byte `offset` of `sql`.
    fn err_ctx<'a>(&self, sql: &'a str, offset: usize, stmt: &'a str) -> ErrContext<'a> {
        ErrContext {
            statement: Some(stmt),
            chars_before: sql.get(..offset).map(|s| s.chars().count()).unwrap_or(0),
            timed_out: self.shared.watchdog.fired(),
        }
    }

    /// Map an error of a call without statement text.
    pub fn map_err(&self, e: duckdb::Error) -> DbError {
        map_error(e)
    }

    /// Execute transaction control (or another statement without results)
    /// and update the tracked transaction state.
    pub fn exec_transaction(&mut self, sql: &str) -> DbResult<()> {
        let kind = lexer::classify(sql);
        let r = self.conn.execute_batch(sql).map_err(map_error);
        self.after_statement(&kind, r.is_ok());
        r
    }

    /// Start the implicit transaction of manual commit mode.
    pub fn implicit_begin(&mut self) -> DbResult<()> {
        if self.mode == TransactionMode::Manual && !self.in_tx {
            self.conn
                .execute_batch("BEGIN TRANSACTION")
                .map_err(map_error)?;
            self.in_tx = true;
            self.sync_state();
        }
        Ok(())
    }

    fn after_statement(&mut self, kind: &StatementKind, ok: bool) {
        match kind {
            StatementKind::Begin if ok => self.in_tx = true,
            StatementKind::Commit | StatementKind::Rollback => self.in_tx = false,
            _ => {}
        }
        self.sync_state();
    }

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

    /// Serve a result without rows until the next result is requested.
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

    /// The statement list is complete: answer `None` until the cursor closes.
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

    /// Columns of the base table of a simple single-table query (looked up
    /// before the query runs: a catalog query closes a streaming result).
    fn source_table(&self, stmt: &str) -> Option<SourceTable> {
        let (db, schema, table) = lexer::single_table_source(stmt)?;
        let sql = "SELECT c.database_name, c.schema_name, c.table_name, c.column_name, c.is_nullable, c.column_default \
                   FROM duckdb_columns() c \
                   JOIN duckdb_tables() t ON t.table_oid = c.table_oid \
                   WHERE lower(c.database_name) = lower(coalesce(?, current_database())) \
                     AND lower(c.schema_name) = lower(coalesce(?, current_schema())) \
                     AND lower(c.table_name) = lower(?) \
                   ORDER BY c.column_index";
        let mut q = self.conn.prepare(sql).ok()?;
        let mut rows = q.query(duckdb::params![db, schema, table]).ok()?;
        let mut out: Option<SourceTable> = None;
        while let Ok(Some(r)) = rows.next() {
            let name: String = r.get(3).ok()?;
            let nullable: bool = r.get(4).unwrap_or(true);
            let default: Option<String> = r.get(5).ok().flatten();
            let auto = default
                .as_deref()
                .is_some_and(|d| d.trim_start().to_ascii_lowercase().starts_with("nextval("));
            if out.is_none() {
                out = Some(SourceTable {
                    database: r.get(0).ok()?,
                    schema: r.get(1).ok()?,
                    table: r.get(2).ok()?,
                    columns: Vec::new(),
                });
            }
            if let Some(s) = out.as_mut() {
                s.columns.push((name, nullable, auto));
            }
        }
        out
    }

    /// Result column descriptions and per-column conversions.
    fn describe(stmt: &Statement<'_>) -> (Vec<ColumnMeta>, Vec<(Conv, bool)>) {
        let n = stmt.column_count();
        let names = stmt.column_names();
        let mut metas = Vec::with_capacity(n);
        let mut convs = Vec::with_capacity(n);
        for i in 0..n {
            let t = stmt.column_logical_type(i);
            let kind = kind_of(&t);
            let mut meta = ColumnMeta::new(
                names.get(i).cloned().unwrap_or_default(),
                type_name(&t),
                kind,
            );
            if t.id() == LogicalTypeId::Decimal {
                meta.precision = Some(u32::from(t.decimal_width()));
                meta.scale = Some(u32::from(t.decimal_scale()));
            }
            convs.push((conv_of(&t), t.id() == LogicalTypeId::Bit));
            metas.push(meta);
        }
        (metas, convs)
    }

    fn apply_sources(stmt_text: &str, columns: &mut [ColumnMeta], source: &SourceTable) {
        let Some(items) = lexer::select_items(stmt_text) else {
            return;
        };
        let mut mapped: Vec<Option<&(String, bool, bool)>> = Vec::new();
        for item in &items {
            match item {
                SelectItem::Star => mapped.extend(source.columns.iter().map(Some)),
                SelectItem::Column(_, src) => mapped.push(
                    source
                        .columns
                        .iter()
                        .find(|c| c.0.eq_ignore_ascii_case(src)),
                ),
                SelectItem::Expression => mapped.push(None),
            }
        }
        if mapped.len() != columns.len() {
            return;
        }
        for (meta, m) in columns.iter_mut().zip(mapped) {
            if let Some((name, nullable, auto)) = m {
                meta.source = Some(ColumnSource {
                    database: Some(source.database.clone()),
                    schema: Some(source.schema.clone()),
                    table: source.table.clone(),
                    column: name.clone(),
                });
                meta.nullable = Some(*nullable);
                meta.auto_increment = Some(*auto);
            }
        }
    }

    /// Execute `req` and serve its cursor until it is closed. Returns a
    /// command that arrived in the meantime and does not belong to the cursor.
    fn serve_cursor(&mut self, req: ExecRequest, rx: &Receiver<Command>) -> Option<Command> {
        let ExecRequest {
            cursor: id,
            sql,
            params,
            opts,
            cancel_seq,
            reply,
        } = req;
        let statements = lexer::split_statements(&sql);
        let single = statements.len() == 1;
        let mut param_pos = 0usize;
        let mut header_reply = reply;
        for span in &statements {
            let fail = |w: &Worker, e: DbError, reply: Reply<Option<ResultHeader>>| {
                let _ = reply.send(Err(e));
                w.finish_cursor(id, rx)
            };
            if self.cancelled(cancel_seq) {
                return fail(self, DbError::cancelled(), header_reply);
            }
            let kind = lexer::classify(span.text);
            // catalog lookups must happen before the statement runs
            let source = if opts.resolve_sources && kind == StatementKind::Query {
                self.source_table(span.text)
            } else {
                None
            };
            let needs_tx = !kind.runs_outside_transaction() || lexer::is_explain_analyze(span.text);
            if needs_tx && let Err(e) = self.implicit_begin() {
                return fail(self, e, header_reply);
            }
            let mut stmt = match self.conn.prepare(span.text) {
                Ok(s) => s,
                Err(e) => {
                    let err = map_error_ctx(e, &self.err_ctx(&sql, span.offset, span.text));
                    return fail(self, err, header_reply);
                }
            };
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
                return fail(self, DbError::query(msg), header_reply);
            }
            let bound: Vec<Param<'_>> =
                params[param_pos..param_pos + n].iter().map(Param).collect();
            param_pos += n;
            if self.cancelled(cancel_seq) {
                drop(stmt);
                return fail(self, DbError::cancelled(), header_reply);
            }

            if !returns_rows(&kind, span.text) {
                self.arm(opts.timeout);
                let result = stmt.execute(params_from_iter(bound.iter()));
                self.disarm();
                drop(stmt);
                let ctx = self.err_ctx(&sql, span.offset, span.text);
                let result = result.map_err(|e| map_error_ctx(e, &ctx));
                self.after_statement(&kind, result.is_ok());
                match result {
                    Ok(changed) => {
                        let changed = u64::try_from(changed).unwrap_or(u64::MAX);
                        let rows_affected = match &kind {
                            k if k.is_dml() => Some(changed),
                            StatementKind::Other(w) if w == "COPY" => Some(changed),
                            StatementKind::Ddl(_) if changed > 0 => Some(changed),
                            _ => None,
                        };
                        let tag = kind.tag();
                        let header = ResultHeader::Update {
                            rows_affected,
                            last_insert_id: None,
                            tag: (!tag.is_empty()).then_some(tag),
                        };
                        let _ = header_reply.send(Ok(Some(header)));
                    }
                    Err(e) => return fail(self, e, header_reply),
                }
                match self.wait_without_rows(id, rx) {
                    Ok(next) => {
                        header_reply = next;
                        continue;
                    }
                    Err(next) => return next,
                }
            }

            // row result, executed in streaming mode
            self.arm(opts.timeout);
            let started = stmt.stream_arrow(params_from_iter(bound.iter())).map(drop);
            if let Err(e) = started {
                self.disarm();
                let err = map_error_ctx(e, &self.err_ctx(&sql, span.offset, span.text));
                drop(stmt);
                return fail(self, err, header_reply);
            }
            let (mut columns, convs) = Self::describe(&stmt);
            if let Some(src) = &source {
                Self::apply_sources(span.text, &mut columns, src);
            }
            let mut rows = stmt.raw_query();
            // the first chunk surfaces execution errors before the header
            let first = rows.next().map(|r| r.map(|row| read_row(row, &convs)));
            self.disarm();
            let first = match first {
                Err(e) => Err(map_error_ctx(
                    e,
                    &self.err_ctx(&sql, span.offset, span.text),
                )),
                Ok(_) if self.shared.watchdog.fired() => Err(DbError::timeout(
                    "The statement timeout elapsed; the query was interrupted",
                )),
                Ok(_) if self.cancelled(cancel_seq) => Err(DbError::cancelled()),
                Ok(r) => Ok(r),
            };
            let buffered = match first {
                Ok(b) => b,
                Err(e) => {
                    drop(rows);
                    drop(stmt);
                    return fail(self, e, header_reply);
                }
            };
            let _ = header_reply.send(Ok(Some(ResultHeader::Rows(columns))));
            let end = self.serve_rows(id, rx, &mut rows, &convs, buffered, &opts, &sql, span);
            drop(rows);
            drop(stmt);
            match end {
                RowsEnd::Next(next_reply) => header_reply = next_reply,
                RowsEnd::Closed => return None,
                RowsEnd::Foreign(cmd) => return Some(cmd),
            }
        }
        let _ = header_reply.send(Ok(None));
        self.finish_cursor(id, rx)
    }

    /// Serve fetches of an open row result.
    #[allow(clippy::too_many_arguments)]
    fn serve_rows(
        &self,
        id: u64,
        rx: &Receiver<Command>,
        rows: &mut duckdb::Rows<'_>,
        convs: &[(Conv, bool)],
        mut buffered: Option<Row>,
        opts: &ExecOptions,
        sql: &str,
        span: &lexer::StatementSpan<'_>,
    ) -> RowsEnd {
        let mut done = buffered.is_none();
        let mut pending_error: Option<DbError> = None;
        loop {
            match self.recv_cursor(id, rx) {
                CursorCmd::Fetch(max_rows, reply) => {
                    if let Some(e) = pending_error.take() {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    self.arm(opts.timeout);
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
                                let r = read_row(row, convs);
                                bytes += row_bytes(&r);
                                out.push(r);
                            }
                            Ok(None) => done = true,
                            Err(e) => {
                                done = true;
                                pending_error = Some(map_error_ctx(
                                    e,
                                    &self.err_ctx(sql, span.offset, span.text),
                                ));
                            }
                        }
                    }
                    self.disarm();
                    if !done && self.shared.watchdog.fired() {
                        // the interrupt may still be pending: end the result here
                        done = true;
                        pending_error = Some(DbError::timeout(
                            "The statement timeout elapsed; the query was interrupted",
                        ));
                    }
                    if out.is_empty()
                        && let Some(e) = pending_error.take()
                    {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    let exhausted = done && pending_error.is_none();
                    let _ = reply.send(Ok(FetchReply {
                        rows: out,
                        done: exhausted,
                    }));
                }
                CursorCmd::Next(reply) => return RowsEnd::Next(reply),
                CursorCmd::Close(reply) => {
                    let _ = reply.send(Ok(()));
                    return RowsEnd::Closed;
                }
                CursorCmd::Discard => return RowsEnd::Closed,
                CursorCmd::Foreign(cmd) => return RowsEnd::Foreign(cmd),
            }
        }
    }

    /// Prepare and run a catalog query, mapping every row with `f`.
    pub fn query_map<T>(
        &self,
        sql: &str,
        params: &[&dyn duckdb::ToSql],
        mut f: impl FnMut(&duckdb::Row<'_>) -> T,
    ) -> DbResult<Vec<T>> {
        let mut stmt = self.conn.prepare(sql).map_err(map_error)?;
        let mut rows = stmt.query(params).map_err(map_error)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(map_error)? {
            out.push(f(r));
        }
        Ok(out)
    }

    /// Value of the first column of the first row of `sql`.
    pub fn query_value<T: duckdb::types::FromSql>(
        &self,
        sql: &str,
        params: &[&dyn duckdb::ToSql],
    ) -> DbResult<Option<T>> {
        Ok(self
            .query_map(sql, params, |r| r.get::<_, Option<T>>(0).ok().flatten())?
            .into_iter()
            .next()
            .flatten())
    }
}

/// How serving a row result ended.
enum RowsEnd {
    Next(Reply<Option<ResultHeader>>),
    Closed,
    Foreign(Command),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits() {
        assert_eq!(bit_text(&[5, 253]), Value::text("101"));
        assert_eq!(bit_text(&[0, 0b1010_0000]), Value::text("10100000"));
        assert_eq!(bit_text(&[]), Value::text(""));
    }

    #[test]
    fn row_classification() {
        assert!(returns_rows(&StatementKind::Query, "SELECT 1"));
        assert!(!returns_rows(
            &StatementKind::Insert,
            "INSERT INTO t VALUES (1)"
        ));
        assert!(returns_rows(
            &StatementKind::Insert,
            "INSERT INTO t VALUES (1) RETURNING *"
        ));
        assert!(!returns_rows(
            &StatementKind::Ddl("CREATE TABLE".into()),
            "CREATE TABLE t(a INT)"
        ));
        assert!(returns_rows(
            &StatementKind::Session("PRAGMA".into()),
            "PRAGMA version"
        ));
        assert!(!returns_rows(
            &StatementKind::Session("SET".into()),
            "SET threads = 1"
        ));
        assert!(!returns_rows(
            &StatementKind::Other("COPY".into()),
            "COPY t TO 'x.csv'"
        ));
        assert!(returns_rows(
            &StatementKind::Other("CALL".into()),
            "CALL pragma_version()"
        ));
    }

    #[test]
    fn shared_instances() {
        let dir =
            std::env::temp_dir().join(format!("dbiji-duckdb-registry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("shared.duckdb").to_string_lossy().into_owned();
        let cfg = OpenConfig {
            path: path.clone(),
            read_only: false,
            threads: Some(1),
            max_memory: None,
            autoload_extensions: false,
            settings: Vec::new(),
            mode: TransactionMode::Auto,
            show_system_objects: false,
        };
        let before = open_instances();
        let (a, ca) = open_shared(&cfg).unwrap();
        let (b, cb) = open_shared(&cfg).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(open_instances(), before + 1);
        ca.execute_batch("CREATE TABLE t(x INT); INSERT INTO t VALUES (1)")
            .unwrap();
        let n: i64 = cb
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let ro = OpenConfig {
            read_only: true,
            ..cfg.clone()
        };
        assert_eq!(
            open_shared(&ro).err().map(|e| e.kind),
            Some(dbiji_core::ErrorKind::Config)
        );
        drop((ca, cb));
        drop((a, b));
        assert_eq!(open_instances(), before);
        // reopening after close works (the file lock was released)
        let (c, cc) = open_shared(&ro).unwrap();
        let n: i64 = cc
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(cc);
        drop(c);
        let mem_ro = OpenConfig {
            path: ":memory:".into(),
            ..ro
        };
        assert_eq!(
            open_shared(&mem_ro).err().map(|e| e.kind),
            Some(dbiji_core::ErrorKind::Config)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
