//! PL/pgSQL debugger based on the `pldbgapi` extension (pldebugger).
//!
//! Two extra connections are opened: a *proxy* that drives the debugger and
//! a *target* that executes the routine. The proxy creates a listener and a
//! global breakpoint at the entry of the routine for the target backend; the
//! target then calls the routine in the background and blocks when it
//! reaches the breakpoint. Every proxy call that waits for the target races
//! against the completion of the target, so the session never hangs when the
//! routine finishes or fails.
//!
//! Note: pldebugger reports breakpoint line numbers shifted by one; the
//! current line is therefore taken from `pldbg_get_stack`, whose numbers match
//! the routine source (`prosrc`).

use crate::catalog::{P, RowExt, quote_literal};
use crate::connect::{self, PgCancelHandle, SessionShared};
use crate::dialect::qualified;
use crate::error::{map_error, pg};
use crate::session::PgSession;
use crate::tabledef::{normalize_signature, require_schema};
use async_trait::async_trait;
use dbiji_core::admin::{DebugSession, DebugState, DebugVariable, StackFrame, TabularData};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::{ConnectParams, DbError, DbResult, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_postgres::{Client, SimpleQueryMessage};

/// The routine to debug, resolved in the user's session.
pub(crate) struct DebugTarget {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    pub source: String,
}

impl PgSession {
    /// Check the debugger prerequisites and resolve the routine.
    pub(crate) async fn debug_target(&self, routine: &ObjectRef) -> DbResult<DebugTarget> {
        if self.is_cockroach() {
            return Err(DbError::unsupported("CockroachDB has no routine debugger"));
        }
        let installed = self.q1("SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pldbgapi'", &[]).await?;
        if installed.is_none() {
            let available = self.q1("SELECT 1 FROM pg_catalog.pg_available_extensions WHERE name = 'pldbgapi'", &[]).await?.is_some();
            return Err(DbError::unsupported(if available {
                "The pldbgapi extension is available but not installed in this database: run CREATE EXTENSION pldbgapi \
                 (as superuser) and make sure plugin_debugger is listed in shared_preload_libraries"
            } else {
                "Debugging requires the pldebugger extension on the server: install the pldebugger package, add \
                 plugin_debugger to shared_preload_libraries, restart the server and run CREATE EXTENSION pldbgapi"
            }));
        }
        let preload = self.q1("SELECT current_setting('shared_preload_libraries')", &[]).await?.map(|r| r.s(0)).transpose()?.unwrap_or_default();
        if !preload.split(',').any(|l| l.trim().trim_matches('"').ends_with("plugin_debugger")) {
            return Err(DbError::unsupported("plugin_debugger is not loaded: add it to shared_preload_libraries and restart the server"));
        }
        let schema = require_schema(routine.schema.as_deref())?;
        let rows = self
            .q(
                "SELECT p.oid::int8, pg_catalog.oidvectortypes(p.proargtypes), l.lanname::text, p.prokind::text, p.prosrc \
                 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
                 WHERE n.nspname = $1 AND p.proname = $2 ORDER BY p.oid",
                &[P::Name(schema), P::Name(&routine.name)],
            )
            .await?;
        let wanted = routine.signature.as_deref().map(normalize_signature);
        for r in rows {
            let oid = r.i(0)? as u32;
            let sig = format!("({})", r.s(1)?);
            if let Some(w) = &wanted
                && *w != normalize_signature(&sig)
                && *w != oid.to_string()
            {
                continue;
            }
            if r.s(2)? != "plpgsql" {
                return Err(DbError::unsupported("Only PL/pgSQL functions and procedures can be debugged"));
            }
            let kind = match r.s(3)?.as_str() {
                "p" => ObjectKind::Procedure,
                "f" => ObjectKind::Function,
                _ => return Err(DbError::unsupported("Only functions and procedures can be debugged")),
            };
            return Ok(DebugTarget { oid, schema: schema.to_string(), name: routine.name.clone(), kind, source: r.s(4)? });
        }
        Err(DbError::query(format!("Routine {} does not exist", routine.display_path())).with_code("42883"))
    }
}

struct Conn {
    client: Client,
    shared: Arc<SessionShared>,
    cancel: Arc<PgCancelHandle>,
    task: JoinHandle<()>,
}

async fn open_conn(params: &ConnectParams) -> DbResult<Conn> {
    let c = connect::open(params).await?;
    Ok(Conn { client: c.client, shared: c.shared, cancel: c.cancel, task: c.task })
}

/// Rows of a simple query as text values.
fn messages_to_tabular(msgs: Vec<SimpleQueryMessage>) -> TabularData {
    let mut data = TabularData::default();
    for m in msgs {
        match m {
            SimpleQueryMessage::RowDescription(cols) if data.columns.is_empty() => {
                data.columns = cols.iter().map(|c| c.name().to_string()).collect();
            }
            SimpleQueryMessage::Row(r) => {
                if data.columns.is_empty() {
                    data.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                }
                data.rows.push(
                    (0..r.len())
                        .map(|i| match r.try_get(i) {
                            Ok(Some(s)) => Value::text(s),
                            _ => Value::Null,
                        })
                        .collect(),
                );
            }
            _ => {}
        }
    }
    data
}

fn first_row(msgs: &[SimpleQueryMessage]) -> Option<&tokio_postgres::SimpleQueryRow> {
    msgs.iter().find_map(|m| match m {
        SimpleQueryMessage::Row(r) => Some(r),
        _ => None,
    })
}

/// Start a debugging session for `target` called with `args` (SQL literals).
pub(crate) async fn start(params: &ConnectParams, target: DebugTarget, args: &[String]) -> DbResult<Box<dyn DebugSession>> {
    let mut p = params.clone();
    p.application_name = "DBiji debugger".into();
    p.read_only = false;
    let proxy = open_conn(&p).await?;
    let tconn = open_conn(&p).await?;
    let pid_msgs = tconn.client.simple_query("SELECT pg_backend_pid()").await.map_err(pg)?;
    let target_pid: i32 = first_row(&pid_msgs).and_then(|r| r.get(0)).and_then(|s| s.parse().ok()).ok_or_else(|| DbError::internal("cannot read the target backend pid"))?;
    let msgs = proxy.client.simple_query("SELECT pldbg_create_listener()").await.map_err(pg)?;
    let handle: i32 = first_row(&msgs).and_then(|r| r.get(0)).and_then(|s| s.parse().ok()).ok_or_else(|| DbError::internal("pldbg_create_listener returned no handle"))?;
    let bp = proxy.client.simple_query(&format!("SELECT pldbg_set_global_breakpoint({handle}, {}, NULL, {target_pid})", target.oid)).await.map_err(pg)?;
    if first_row(&bp).and_then(|r| r.get(0)) != Some("t") {
        return Err(DbError::query("The debugger could not set the entry breakpoint"));
    }
    let call = format!("{} {}({})", if target.kind == ObjectKind::Procedure { "CALL" } else { "SELECT * FROM" }, qualified(Some(&target.schema), &target.name), args.join(", "));
    let Conn { client: tclient, shared: tshared, cancel: tcancel, task: ttask } = tconn;
    let target_task = tokio::spawn(async move {
        let res = tclient.simple_query(&call).await;
        drop(tclient);
        match res {
            Ok(msgs) => Ok(messages_to_tabular(msgs)),
            Err(e) => Err(map_error(&e)),
        }
    });
    Ok(Box::new(PgDebugSession {
        proxy: Some(proxy.client),
        proxy_cancel: proxy.cancel,
        proxy_task: Some(proxy.task),
        _proxy_shared: proxy.shared,
        handle,
        func_oid: target.oid,
        source: target.source,
        target: Some(target_task),
        target_result: None,
        target_conn_task: Some(ttask),
        target_shared: tshared,
        target_cancel: tcancel,
        attached: false,
        stopped: false,
        finished: None,
        last_vars: Vec::new(),
        broken: None,
    }))
}

enum Outcome {
    Rows(Vec<SimpleQueryMessage>),
    TargetDone,
}

/// Debugger driving a target backend.
struct PgDebugSession {
    proxy: Option<Client>,
    proxy_cancel: Arc<PgCancelHandle>,
    proxy_task: Option<JoinHandle<()>>,
    _proxy_shared: Arc<SessionShared>,
    handle: i32,
    func_oid: u32,
    source: String,
    target: Option<JoinHandle<DbResult<TabularData>>>,
    /// Result of the target once it finished during a proxy call.
    target_result: Option<Result<DbResult<TabularData>, tokio::task::JoinError>>,
    target_conn_task: Option<JoinHandle<()>>,
    target_shared: Arc<SessionShared>,
    target_cancel: Arc<PgCancelHandle>,
    attached: bool,
    stopped: bool,
    finished: Option<DebugState>,
    last_vars: Vec<(String, i32)>,
    /// Set when the proxy/target channel can no longer be trusted.
    broken: Option<DbError>,
}

/// The target closed the debugger connection (the routine finished or failed).
fn is_terminated(e: &DbError) -> bool {
    let m = e.message.to_ascii_lowercase();
    m.contains("debugger connection") || m.contains("connection terminated") || m.contains("target backend")
}

/// pldebugger's socket wait was interrupted by a signal. pldebugger does not
/// retry `select()` on `EINTR`, so a reply may still be pending on the socket.
fn is_interrupted(e: &DbError) -> bool {
    e.message.contains("select() failed")
}

enum Raced {
    Proxy(Result<Vec<SimpleQueryMessage>, tokio_postgres::Error>),
    Target(Result<DbResult<TabularData>, tokio::task::JoinError>),
}

impl PgDebugSession {
    fn proxy(&self) -> DbResult<&Client> {
        self.proxy.as_ref().ok_or_else(|| DbError::connection("The debugging session has ended"))
    }

    fn check(&self) -> DbResult<()> {
        match &self.broken {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn mark_broken(&mut self, cause: DbError) -> DbError {
        let mut err = DbError::connection(
            "The debugger connection to the target was interrupted by a server signal (a pldebugger limitation, \
             e.g. during a concurrent DROP DATABASE); abort and restart the debugging session",
        )
        .with_code(cause.code.clone().unwrap_or_else(|| "08006".into()));
        err.detail = Some(cause.message);
        self.broken = Some(err.clone());
        err
    }

    fn messages(&self) -> Vec<String> {
        self.target_shared.take_notices().into_iter().map(|m| format!("{}: {}", m.severity, m.message)).collect()
    }

    /// Plain proxy query (no waiting on the target).
    async fn simple(&mut self, sql: &str) -> DbResult<Vec<SimpleQueryMessage>> {
        self.check()?;
        let res = self.proxy()?.simple_query(sql).await;
        match res {
            Ok(rows) => Ok(rows),
            Err(e) => {
                let err = map_error(&e);
                if is_interrupted(&err) { Err(self.mark_broken(err)) } else { Err(err) }
            }
        }
    }

    /// Run a proxy call that waits for the target; if the target finishes
    /// first, cancel the call. With `retry_safe`, interruptions are returned
    /// as plain errors so the caller can retry; otherwise the session is
    /// marked broken.
    async fn call(&mut self, sql: &str, retry_safe: bool) -> DbResult<Outcome> {
        self.check()?;
        let proxy_error = {
            let proxy = self.proxy.as_ref().ok_or_else(|| DbError::connection("The debugging session has ended"))?;
            let fut = proxy.simple_query(sql);
            tokio::pin!(fut);
            let raced = match self.target.as_mut() {
                Some(target) => tokio::select! {
                    r = &mut fut => Raced::Proxy(r),
                    res = &mut *target => Raced::Target(res),
                },
                None => Raced::Proxy(fut.as_mut().await),
            };
            match raced {
                Raced::Proxy(Ok(rows)) => return Ok(Outcome::Rows(rows)),
                Raced::Proxy(Err(e)) => map_error(&e),
                Raced::Target(res) => {
                    self.target = None;
                    self.target_result = Some(res);
                    // the target has finished: release the pending proxy call
                    let _ = self.proxy_cancel.send().await;
                    let _ = tokio::time::timeout(Duration::from_secs(10), &mut fut).await;
                    return Ok(Outcome::TargetDone);
                }
            }
        };
        if is_terminated(&proxy_error) {
            return Ok(Outcome::TargetDone);
        }
        if is_interrupted(&proxy_error) {
            // the target may have finished meanwhile (its connection closes the channel)
            if let Some(t) = self.target.as_mut()
                && let Ok(res) = tokio::time::timeout(Duration::from_millis(200), &mut *t).await
            {
                self.target = None;
                self.target_result = Some(res);
                return Ok(Outcome::TargetDone);
            }
            if !retry_safe {
                return Err(self.mark_broken(proxy_error));
            }
        }
        Err(proxy_error)
    }

    /// Final state once the target has finished.
    async fn finish(&mut self) -> DebugState {
        if let Some(state) = &self.finished {
            return state.clone();
        }
        let mut messages = Vec::new();
        let outcome = match (self.target_result.take(), self.target.take()) {
            (Some(r), _) => Some(r),
            (None, Some(t)) => match tokio::time::timeout(Duration::from_secs(30), t).await {
                Ok(r) => Some(r),
                Err(_) => {
                    let _ = self.target_cancel.send().await;
                    messages.push("ERROR: the routine did not finish".into());
                    None
                }
            },
            (None, None) => None,
        };
        let result = match outcome {
            Some(Ok(Ok(data))) => Some(data),
            Some(Ok(Err(e))) => {
                messages.push(format!("ERROR: {}", e.full_text()));
                None
            }
            Some(Err(e)) => {
                messages.push(format!("ERROR: {e}"));
                None
            }
            None => None,
        };
        let mut all = self.messages();
        all.extend(messages);
        let state = DebugState { line: None, finished: true, result, messages: all };
        self.stopped = false;
        self.finished = Some(state.clone());
        state
    }

    /// Current line from the stack (pldebugger's breakpoint rows are shifted by one).
    async fn current_state(&mut self, bp_rows: &[SimpleQueryMessage]) -> DbResult<DebugState> {
        self.stopped = true;
        let bp_line = first_row(bp_rows).and_then(|r| r.get(1)).and_then(|s| s.parse::<i64>().ok());
        let stack = self.simple(&format!("SELECT linenumber FROM pldbg_get_stack({}) ORDER BY level LIMIT 1", self.handle)).await?;
        let line = first_row(&stack).and_then(|r| r.get(0)).and_then(|s| s.parse::<i64>().ok()).or_else(|| bp_line.map(|l| l - 1)).filter(|l| *l > 0).map(|l| l as u32);
        Ok(DebugState { line, finished: false, result: None, messages: self.messages() })
    }

    async fn step(&mut self, func: &str) -> DbResult<DebugState> {
        if self.finished.is_some() {
            return Ok(self.finish().await);
        }
        self.check()?;
        if !self.attached || !self.stopped {
            return self.wait().await;
        }
        self.stopped = false;
        let sql = format!("SELECT * FROM {func}({})", self.handle);
        match self.call(&sql, false).await? {
            Outcome::Rows(rows) if first_row(&rows).is_some() => self.current_state(&rows).await,
            _ => Ok(self.finish().await),
        }
    }
}

#[async_trait]
impl DebugSession for PgDebugSession {
    fn source(&self) -> &str {
        &self.source
    }

    async fn wait(&mut self) -> DbResult<DebugState> {
        if self.finished.is_some() {
            return Ok(self.finish().await);
        }
        self.check()?;
        if self.stopped {
            let rows = self.simple(&format!("SELECT linenumber FROM pldbg_get_stack({}) ORDER BY level LIMIT 1", self.handle)).await?;
            let line = first_row(&rows).and_then(|r| r.get(0)).and_then(|s| s.parse::<u32>().ok());
            return Ok(DebugState { line, finished: false, result: None, messages: self.messages() });
        }
        if !self.attached {
            let mut attempts = 0;
            loop {
                match self.call(&format!("SELECT pldbg_wait_for_target({})", self.handle), true).await {
                    Ok(Outcome::Rows(_)) => {
                        self.attached = true;
                        break;
                    }
                    Ok(Outcome::TargetDone) => return Ok(self.finish().await),
                    // waiting for the target to connect is interrupted by signals in
                    // older pldebugger versions; nothing was exchanged yet, so wait again
                    Err(e) if is_interrupted(&e) && attempts < 20 => {
                        attempts += 1;
                        tracing::debug!("pldbg_wait_for_target interrupted, retrying ({attempts})");
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        match self.call(&format!("SELECT * FROM pldbg_wait_for_breakpoint({})", self.handle), false).await? {
            Outcome::Rows(rows) if first_row(&rows).is_some() => self.current_state(&rows).await,
            _ => Ok(self.finish().await),
        }
    }

    async fn step_into(&mut self) -> DbResult<DebugState> {
        self.step("pldbg_step_into").await
    }

    async fn step_over(&mut self) -> DbResult<DebugState> {
        self.step("pldbg_step_over").await
    }

    async fn resume(&mut self) -> DbResult<DebugState> {
        self.step("pldbg_continue").await
    }

    async fn set_breakpoint(&mut self, line: u32) -> DbResult<()> {
        let rows = self.simple(&format!("SELECT pldbg_set_breakpoint({}, {}, {line})", self.handle, self.func_oid)).await?;
        if first_row(&rows).and_then(|r| r.get(0)) == Some("t") { Ok(()) } else { Err(DbError::query(format!("Cannot set a breakpoint at line {line}"))) }
    }

    async fn clear_breakpoint(&mut self, line: u32) -> DbResult<()> {
        self.simple(&format!("SELECT pldbg_drop_breakpoint({}, {}, {line})", self.handle, self.func_oid)).await?;
        Ok(())
    }

    async fn variables(&mut self) -> DbResult<Vec<DebugVariable>> {
        if !self.stopped {
            return Ok(Vec::new());
        }
        let rows = self.simple(&format!("SELECT name, varclass::text, linenumber, pg_catalog.format_type(dtype, NULL), value FROM pldbg_get_variables({})", self.handle)).await?;
        let mut out = Vec::new();
        self.last_vars.clear();
        for m in rows {
            if let SimpleQueryMessage::Row(r) = m {
                let name = r.get(0).unwrap_or_default().to_string();
                let line = r.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                self.last_vars.push((name.clone(), line));
                out.push(DebugVariable {
                    name,
                    data_type: r.get(3).unwrap_or_default().to_string(),
                    value: r.get(4).unwrap_or_default().to_string(),
                    is_argument: r.get(1) == Some("A"),
                });
            }
        }
        Ok(out)
    }

    async fn stack(&mut self) -> DbResult<Vec<StackFrame>> {
        if !self.stopped {
            return Ok(Vec::new());
        }
        let rows = self.simple(&format!("SELECT level, targetname, linenumber, args FROM pldbg_get_stack({}) ORDER BY level", self.handle)).await?;
        let mut out = Vec::new();
        for m in rows {
            if let SimpleQueryMessage::Row(r) = m {
                out.push(StackFrame {
                    level: r.get(0).and_then(|s| s.parse().ok()).unwrap_or(0),
                    routine: r.get(1).unwrap_or_default().to_string(),
                    line: r.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                    args: r.get(3).unwrap_or_default().to_string(),
                });
            }
        }
        Ok(out)
    }

    async fn set_variable(&mut self, name: &str, value: &str) -> DbResult<()> {
        if !self.stopped {
            return Err(DbError::query("The routine is not stopped at a breakpoint"));
        }
        if !self.last_vars.iter().any(|(n, _)| n == name) {
            self.variables().await?;
        }
        let line = self.last_vars.iter().find(|(n, _)| n == name).map(|(_, l)| *l).unwrap_or(-1);
        let rows = self.simple(&format!("SELECT pldbg_deposit_value({}, {}, {line}, {})", self.handle, quote_literal(name), quote_literal(value))).await?;
        if first_row(&rows).and_then(|r| r.get(0)) == Some("t") { Ok(()) } else { Err(DbError::query(format!("Cannot assign '{value}' to {name}"))) }
    }

    async fn abort(&mut self) -> DbResult<()> {
        if self.finished.is_none() {
            if self.attached && self.broken.is_none() {
                let sql = format!("SELECT * FROM pldbg_abort_target({})", self.handle);
                if let Ok(proxy) = self.proxy() {
                    let _ = tokio::time::timeout(Duration::from_secs(10), proxy.simple_query(&sql)).await;
                }
            } else {
                // not attached yet or channel unusable: interrupt the target directly
                let _ = self.target_cancel.send().await;
            }
        }
        // closing the proxy also releases a target waiting for debugger commands
        self.proxy = None;
        if let Some(t) = self.target.take()
            && tokio::time::timeout(Duration::from_secs(10), t).await.is_err()
        {
            let _ = self.target_cancel.send().await;
        }
        self.finished = Some(DebugState { line: None, finished: true, result: None, messages: self.messages() });
        self.stopped = false;
        if let Some(t) = self.proxy_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
        if let Some(t) = self.target_conn_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
        Ok(())
    }
}

impl Drop for PgDebugSession {
    fn drop(&mut self) {
        if self.target.is_some()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            // unblock a target still waiting for the debugger
            let cancel = self.target_cancel.clone();
            rt.spawn(async move {
                let _ = cancel.send().await;
            });
        }
    }
}
