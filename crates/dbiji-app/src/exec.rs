//! Statement execution engine for SQL editors (FR-S2, FR-S3).
//!
//! The UI prepares a list of [`StmtPlan`]s (split, classified, parameters
//! substituted, confirmations done) and starts an [`Execution`]. The engine
//! runs the statements in order on the editor's session and streams
//! [`ExecEvent`]s back; the first page of every row result is fetched here so
//! it arrives together with its columns.

use crate::backend::{Backend, Job};
use crate::conn::session::SessionHandle;
use dbiji_core::driver::{ExecOptions, ResultHeader};
use dbiji_core::rowstore::{RowStore, SharedRowStore};
use dbiji_core::{ColumnMeta, DbError};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Client-side command (FR-S8) executed by the engine instead of the server.
#[derive(Debug, Clone)]
pub enum ClientAction {
    Set {
        name: String,
        value: String,
    },
    Echo(String),
    /// Export the next result set with the given options.
    Export(std::collections::BTreeMap<String, String>),
}

/// One statement ready to run.
#[derive(Debug, Clone)]
pub struct StmtPlan {
    pub index: usize,
    /// SQL sent to the server (parameters already substituted).
    pub sql: String,
    /// Original text exactly as in the editor (history, error markers).
    pub original: String,
    /// Byte range in the editor text.
    pub range: Range<usize>,
    pub is_write: bool,
    pub client: Option<ClientAction>,
}

#[derive(Debug, Clone)]
pub struct ExecOpts {
    pub fetch_size: usize,
    pub stop_on_error: bool,
    pub resolve_sources: bool,
    pub timeout: Option<Duration>,
    pub store_budget: usize,
    /// Smart commit: switch the session to manual commit before running.
    pub switch_to_manual: bool,
}

impl Default for ExecOpts {
    fn default() -> Self {
        ExecOpts { fetch_size: 200, stop_on_error: true, resolve_sources: true, timeout: None, store_budget: dbiji_core::rowstore::DEFAULT_STORE_BUDGET, switch_to_manual: false }
    }
}

/// History record produced for every executed statement (FR-S7).
#[derive(Debug, Clone)]
pub struct HistoryRecord {
    pub sql: String,
    pub started: chrono::DateTime<chrono::Local>,
    pub duration: Duration,
    pub rows: Option<u64>,
    pub error: Option<String>,
    pub cancelled: bool,
}

pub type HistorySink = Arc<dyn Fn(HistoryRecord) + Send + Sync>;

pub enum ExecEvent {
    Started { index: usize },
    Rows { index: usize, sql: String, columns: Vec<ColumnMeta>, store: SharedRowStore, token: Option<u64>, exhausted: bool, elapsed: Duration },
    Update { index: usize, rows: Option<u64>, tag: Option<String>, last_insert_id: Option<u64> },
    Message { index: Option<usize>, severity: String, text: String },
    Error { index: usize, error: DbError },
    Client { index: usize, action: ClientAction },
    Finished { index: usize, elapsed: Duration, ok: bool },
    Done { elapsed: Duration, cancelled: bool, executed: usize, failed: usize },
}

/// A running execution.
pub struct Execution {
    rx: mpsc::UnboundedReceiver<ExecEvent>,
    pub cancel: CancellationToken,
    pub session: SessionHandle,
    pub started: Instant,
    job: Job<()>,
    pub total: usize,
    pub current: Option<usize>,
}

impl Execution {
    pub fn start(backend: &Backend, session: SessionHandle, plans: Vec<StmtPlan>, opts: ExecOpts, history: Option<HistorySink>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let total = plans.len();
        let repaint = backend.repainter();
        let job = backend.spawn(run(session.clone(), plans, opts, history, tx, cancel.clone(), repaint));
        Execution { rx, cancel, session, started: Instant::now(), job, total, current: None }
    }

    /// Drain pending events.
    pub fn poll(&mut self) -> Vec<ExecEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            if let ExecEvent::Started { index } = &ev {
                self.current = Some(*index);
            }
            out.push(ev);
        }
        out
    }

    pub fn is_finished(&self) -> bool {
        self.job.is_finished() && self.rx.is_empty()
    }

    /// Cancel: stop after the current statement and ask the server to abort it.
    pub fn cancel(&self, backend: &Backend) {
        self.cancel.cancel();
        let s = self.session.clone();
        backend.spawn_detached(async move {
            let _ = s.cancel().await;
        });
    }
}

async fn run(
    session: SessionHandle,
    plans: Vec<StmtPlan>,
    opts: ExecOpts,
    history: Option<HistorySink>,
    tx: mpsc::UnboundedSender<ExecEvent>,
    cancel: CancellationToken,
    repaint: crate::backend::Repainter,
) {
    let send = |ev: ExecEvent| {
        let _ = tx.send(ev);
        repaint.request();
    };
    let started_all = Instant::now();
    let mut executed = 0usize;
    let mut failed = 0usize;
    let exec_opts = ExecOptions { resolve_sources: opts.resolve_sources, timeout: opts.timeout };
    let last_index = plans.len().saturating_sub(1);
    if opts.switch_to_manual {
        if let Err(e) = session.set_transaction_mode(dbiji_core::driver::TransactionMode::Manual).await {
            send(ExecEvent::Message { index: None, severity: "WARNING".into(), text: format!("Cannot switch to manual commit: {e}") });
        } else {
            send(ExecEvent::Message { index: None, severity: "INFO".into(), text: "Smart commit: switched to manual commit mode".into() });
        }
    }
    for (pos, plan) in plans.into_iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        let index = plan.index;
        send(ExecEvent::Started { index });
        let t0 = Instant::now();
        let wall = chrono::Local::now();
        if let Some(action) = plan.client.clone() {
            send(ExecEvent::Client { index, action });
            send(ExecEvent::Finished { index, elapsed: t0.elapsed(), ok: true });
            executed += 1;
            continue;
        }
        let mut rows_total: Option<u64> = None;
        let mut ok = true;
        let mut error_text: Option<String> = None;
        let result = tokio::select! {
            r = session.execute(&plan.sql, &[], &exec_opts) => Some(r),
            _ = cancel.cancelled() => None,
        };
        match result {
            None => {
                ok = false;
                error_text = Some("cancelled".into());
            }
            Some(Err(e)) => {
                ok = false;
                error_text = Some(e.to_string());
                send(ExecEvent::Error { index, error: e });
            }
            Some(Ok(ex)) => {
                for m in ex.messages {
                    send(ExecEvent::Message { index: Some(index), severity: m.severity, text: m.message });
                }
                let token = ex.token;
                let mut header = ex.header;
                let mut keep_open = false;
                loop {
                    match header.take() {
                        None => break,
                        Some(ResultHeader::Update { rows_affected, last_insert_id, tag }) => {
                            if let Some(n) = rows_affected {
                                rows_total = Some(rows_total.unwrap_or(0) + n);
                            }
                            send(ExecEvent::Update { index, rows: rows_affected, tag, last_insert_id });
                        }
                        Some(ResultHeader::Rows(columns)) => {
                            let store = RowStore::shared(opts.store_budget);
                            let fetched = tokio::select! {
                                r = session.fetch(token, opts.fetch_size.max(1)) => Some(r),
                                _ = cancel.cancelled() => None,
                            };
                            match fetched {
                                None => {
                                    ok = false;
                                    break;
                                }
                                Some(Err(e)) => {
                                    ok = false;
                                    let e: DbError = e.into();
                                    error_text = Some(e.to_string());
                                    send(ExecEvent::Error { index, error: e });
                                    break;
                                }
                                Some(Ok(out)) => {
                                    let n = out.rows.len();
                                    rows_total = Some(rows_total.unwrap_or(0) + n as u64);
                                    for m in out.messages {
                                        send(ExecEvent::Message { index: Some(index), severity: m.severity, text: m.message });
                                    }
                                    store.lock().extend(out.rows);
                                    let exhausted = out.exhausted;
                                    send(ExecEvent::Rows {
                                        index,
                                        sql: plan.sql.clone(),
                                        columns,
                                        store,
                                        token: if exhausted { None } else { Some(token) },
                                        exhausted,
                                        elapsed: t0.elapsed(),
                                    });
                                    if !exhausted {
                                        // Remaining rows are fetched on demand; the
                                        // cursor stays open only for the last statement.
                                        keep_open = pos == last_index;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    match session.next_result(token).await {
                        Ok(h) => header = h,
                        Err(crate::conn::session::FetchError::CursorClosed) => break,
                        Err(crate::conn::session::FetchError::Db(e)) => {
                            ok = false;
                            error_text = Some(e.to_string());
                            send(ExecEvent::Error { index, error: e });
                            break;
                        }
                    }
                }
                if !keep_open {
                    session.close_cursor_token(token).await;
                }
            }
        }
        let elapsed = t0.elapsed();
        if let Some(h) = &history {
            h(HistoryRecord { sql: plan.original.clone(), started: wall, duration: elapsed, rows: rows_total, cancelled: cancel.is_cancelled(), error: error_text });
        }
        executed += 1;
        if !ok {
            failed += 1;
        }
        send(ExecEvent::Finished { index, elapsed, ok });
        if !ok && opts.stop_on_error {
            break;
        }
    }
    send(ExecEvent::Done { elapsed: started_all.elapsed(), cancelled: cancel.is_cancelled(), executed, failed });
}
