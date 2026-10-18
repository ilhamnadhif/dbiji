//! Incremental fetching of one result set into a bounded-memory `RowStore`
//! (FR-D1): first page on execute, "fetch next page", "fetch all", refresh,
//! total row count. When the cursor was closed in between (another statement
//! needed the session) the query is transparently re-executed and the
//! already fetched rows are skipped.

use crate::backend::{Backend, Job, Loadable};
use crate::conn::session::{FetchError, SessionHandle};
use crate::ops::OpHandle;
use dbiji_core::driver::{ExecOptions, ResultHeader, ServerMessage};
use dbiji_core::rowstore::{RowStore, SharedRowStore};
use dbiji_core::{ColumnMeta, DbError, DbResult, Dialect, Value};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Outcome of a fetch job.
pub struct FetchBatch {
    pub added: usize,
    pub exhausted: bool,
    pub token: Option<u64>,
    pub messages: Vec<ServerMessage>,
}

pub struct ResultSource {
    pub session: SessionHandle,
    pub sql: String,
    pub params: Vec<Value>,
    pub columns: Vec<ColumnMeta>,
    pub store: SharedRowStore,
    pub token: Option<u64>,
    pub exhausted: bool,
    fetch_job: Option<Job<DbResult<FetchBatch>>>,
    fetch_op: Option<u64>,
    pub error: Option<DbError>,
    pub messages: Vec<ServerMessage>,
    pub executed_at: chrono::DateTime<chrono::Local>,
    pub exec_duration: Duration,
    pub fetch_started: Option<Instant>,
    pub total_count: Loadable<u64>,
    /// Incremented whenever rows were appended (grid refresh hint).
    pub generation: u64,
    pub budget: usize,
}

impl ResultSource {
    /// Wrap a freshly executed cursor whose first header was `Rows(columns)`.
    pub fn new(session: SessionHandle, sql: String, params: Vec<Value>, columns: Vec<ColumnMeta>, token: Option<u64>, budget: usize) -> Self {
        ResultSource {
            session,
            sql,
            params,
            columns,
            store: RowStore::shared(budget),
            token,
            exhausted: token.is_none(),
            fetch_job: None,
            fetch_op: None,
            error: None,
            messages: Vec::new(),
            executed_at: chrono::Local::now(),
            exec_duration: Duration::ZERO,
            fetch_started: None,
            total_count: Loadable::NotLoaded,
            generation: 0,
            budget,
        }
    }

    /// Wrap rows already fetched by the execution engine.
    #[allow(clippy::too_many_arguments)]
    pub fn from_executed(
        session: SessionHandle,
        sql: String,
        params: Vec<Value>,
        columns: Vec<ColumnMeta>,
        store: SharedRowStore,
        token: Option<u64>,
        exhausted: bool,
        budget: usize,
    ) -> Self {
        let mut s = Self::new(session, sql, params, columns, token, budget);
        s.store = store;
        s.exhausted = exhausted;
        s
    }

    pub fn row_count(&self) -> usize {
        self.store.lock().len()
    }

    pub fn is_fetching(&self) -> bool {
        self.fetch_job.is_some()
    }

    pub fn has_more(&self) -> bool {
        !self.exhausted
    }

    /// Fetch up to `n` more rows (`usize::MAX` = all) in the background.
    pub fn fetch_more(&mut self, backend: &Backend, n: usize, op: Option<OpHandle>) {
        if self.fetch_job.is_some() || self.exhausted {
            return;
        }
        self.error = None;
        self.fetch_started = Some(Instant::now());
        self.fetch_op = op.as_ref().map(|o| o.id);
        let session = self.session.clone();
        let store = self.store.clone();
        let sql = self.sql.clone();
        let params = self.params.clone();
        let token = self.token;
        self.fetch_job = Some(backend.spawn(fetch_rows(session, store, sql, params, token, n, op)));
    }

    /// Apply finished fetches. Returns the id of the finished operation, if any.
    pub fn poll(&mut self) -> Option<(u64, Result<usize, String>)> {
        let mut finished_op = None;
        if let Some(job) = self.fetch_job.as_mut() {
            if let Some(res) = job.poll() {
                self.fetch_job = None;
                match res {
                    Ok(b) => {
                        self.exhausted = b.exhausted;
                        self.token = b.token;
                        self.messages.extend(b.messages);
                        self.generation += 1;
                        if let Some(op) = self.fetch_op.take() {
                            finished_op = Some((op, Ok(b.added)));
                        }
                    }
                    Err(e) => {
                        if let Some(op) = self.fetch_op.take() {
                            finished_op = Some((op, Err(e.to_string())));
                        }
                        self.error = Some(e);
                    }
                }
            } else {
                // rows are appended progressively by the job
                self.generation += 1;
            }
        }
        self.total_count.poll();
        finished_op
    }

    /// Stop a running "fetch all" (the cursor stays open).
    pub fn cancel_fetch(&mut self) {
        if let Some(mut j) = self.fetch_job.take() {
            j.abort();
        }
    }

    /// Re-run the query from scratch.
    pub fn refresh(&mut self, backend: &Backend, first_page: usize) {
        self.cancel_fetch();
        let budget = self.budget;
        self.store = RowStore::shared(budget);
        self.token = None;
        self.exhausted = false;
        self.total_count = Loadable::NotLoaded;
        self.executed_at = chrono::Local::now();
        self.fetch_more(backend, first_page, None);
    }

    /// Replace the SQL (server-side filter / sort) and re-run.
    pub fn requery(&mut self, backend: &Backend, sql: String, first_page: usize) {
        self.sql = sql;
        self.refresh(backend, first_page);
    }

    /// `SELECT COUNT(*)` over the query (FR "calculate total row count").
    pub fn count_total(&mut self, backend: &Backend, dialect: Arc<dyn Dialect>) {
        if self.total_count.is_loading() {
            return;
        }
        let sql = dialect.count_query(&self.sql);
        let session = self.session.clone();
        let params = self.params.clone();
        self.total_count = Loadable::Loading(backend.spawn(async move {
            let (_, rows) = session.query_all(&sql, &params).await?;
            rows.first()
                .and_then(|r| r.first())
                .and_then(Value::as_i64)
                .map(|n| n as u64)
                .ok_or_else(|| DbError::query("COUNT(*) returned no value"))
        }));
    }
}

/// Fetch `n` rows, re-executing the query when the cursor was closed.
async fn fetch_rows(
    session: SessionHandle,
    store: SharedRowStore,
    sql: String,
    params: Vec<Value>,
    mut token: Option<u64>,
    n: usize,
    op: Option<OpHandle>,
) -> DbResult<FetchBatch> {
    let already = store.lock().len();
    let mut messages = Vec::new();
    let page = 1000.min(n.max(1));
    let mut added = 0usize;
    // (Re)open the cursor if needed and skip what we already have.
    let reopen = match token {
        Some(t) => !session.cursor_open(t).await,
        None => true,
    };
    if reopen {
        let ex = session.execute(&sql, &params, &ExecOptions::default()).await?;
        messages.extend(ex.messages);
        match ex.header {
            Some(ResultHeader::Rows(_)) => {}
            _ => return Ok(FetchBatch { added: 0, exhausted: true, token: None, messages }),
        }
        token = Some(ex.token);
        let mut skip = already;
        while skip > 0 {
            let chunk = skip.min(5000);
            match session.fetch(ex.token, chunk).await {
                Ok(out) => {
                    let got = out.rows.len();
                    skip -= got.min(skip);
                    if out.exhausted || got == 0 {
                        return Ok(FetchBatch { added: 0, exhausted: true, token: None, messages });
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    let Some(tok) = token else { return Ok(FetchBatch { added: 0, exhausted: true, token: None, messages }) };
    let mut exhausted = false;
    while added < n {
        if let Some(op) = &op
            && op.cancel.is_cancelled() {
                break;
            }
        let want = page.min(n - added);
        match session.fetch(tok, want).await {
            Ok(out) => {
                let got = out.rows.len();
                messages.extend(out.messages);
                {
                    let mut s = store.lock();
                    s.extend(out.rows);
                }
                added += got;
                if let Some(op) = &op {
                    op.progress.done.store((already + added) as u64, Ordering::Relaxed);
                }
                if out.exhausted || got < want {
                    exhausted = true;
                    break;
                }
            }
            Err(FetchError::CursorClosed) => {
                return Err(DbError::internal("The cursor was closed while fetching; refresh the result."));
            }
            Err(FetchError::Db(e)) => return Err(e),
        }
    }
    if exhausted {
        session.close_cursor_token(tok).await;
    }
    Ok(FetchBatch { added, exhausted, token: if exhausted { None } else { Some(tok) }, messages })
}
