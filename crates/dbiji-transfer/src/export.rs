//! Export engine: pumping a source into a sink, and multi-object export jobs.

use crate::error::with_item;
use crate::options::{ExportFormat, ExportOptions};
use crate::progress::Progress;
use crate::sink::{ExportSummary, ExportTarget, RowSink, create_sink};
use crate::source::{CursorSource, RowSource};
use async_trait::async_trait;
use chrono::NaiveDateTime;
use dbiji_core::driver::{CancelHandle, ExecOptions, Session, query_scalar};
use dbiji_core::{DbError, DbResult, Dialect, ErrorKind, ObjectRef};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Rows per batch used by [`run_export`].
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// Default file name pattern of export jobs; the extension is appended.
pub const DEFAULT_FILE_NAME_PATTERN: &str = "${table}_${timestamp}";

/// Opens sessions for background transfers. Implemented by the application
/// (it knows how to resolve secrets, tunnels and the connection settings).
#[async_trait]
pub trait SessionFactory: Send + Sync {
    /// Open a new session (or hand out a dedicated one).
    async fn open(&self) -> DbResult<Box<dyn Session>>;

    /// Dialect of the sessions this factory opens.
    fn dialect(&self) -> Arc<dyn Dialect>;
}

/// Export every row of `source` into `sink` in batches of [`DEFAULT_BATCH_SIZE`].
///
/// On failure or cancellation the sink is aborted (partial files are deleted
/// unless `keep_partial_file` is set) and the error is returned; cancellation
/// yields an error of kind [`ErrorKind::Cancelled`] within one batch.
pub async fn run_export(source: &mut dyn RowSource, sink: &mut dyn RowSink, progress: &Progress, cancel: &CancellationToken) -> DbResult<ExportSummary> {
    run_export_batched(source, sink, DEFAULT_BATCH_SIZE, progress, cancel).await
}

/// [`run_export`] with an explicit batch size.
pub async fn run_export_batched(source: &mut dyn RowSource, sink: &mut dyn RowSink, batch_size: usize, progress: &Progress, cancel: &CancellationToken) -> DbResult<ExportSummary> {
    let started = Instant::now();
    if let Some(total) = source.total_rows() {
        progress.add_total_rows(total);
    }
    let pumped = pump(source, sink, batch_size.max(1), progress, cancel).await;
    let closed = source.close().await;
    let reported = match pumped {
        Ok(reported) => reported,
        Err(e) => {
            sink.abort();
            return Err(e);
        }
    };
    let mut summary = match sink.finish() {
        Ok(s) => s,
        Err(e) => {
            sink.abort();
            return Err(e);
        }
    };
    progress.add_bytes(summary.bytes.saturating_sub(reported));
    summary.warnings.extend(source.take_warnings());
    if let Err(e) = closed {
        summary.warnings.push(format!("Closing the source failed: {e}"));
    }
    summary.duration_ms = started.elapsed().as_millis() as u64;
    Ok(summary)
}

/// Copy all rows; returns the number of bytes already reported to `progress`.
async fn pump(source: &mut dyn RowSource, sink: &mut dyn RowSink, batch_size: usize, progress: &Progress, cancel: &CancellationToken) -> DbResult<u64> {
    if cancel.is_cancelled() {
        return Err(DbError::cancelled());
    }
    let columns = source.columns().to_vec();
    let table = source.table().cloned();
    sink.begin(&columns, table.as_ref())?;
    let mut reported = 0u64;
    loop {
        let batch = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DbError::cancelled()),
            batch = source.next_batch(batch_size) => batch?,
        };
        if batch.is_empty() {
            break;
        }
        for row in &batch {
            sink.write_row(row)?;
        }
        progress.add_rows(batch.len() as u64);
        let bytes = sink.bytes_written();
        progress.add_bytes(bytes.saturating_sub(reported));
        reported = reported.max(bytes);
        if cancel.is_cancelled() {
            return Err(DbError::cancelled());
        }
    }
    Ok(reported)
}

/// Await `fut` unless `cancel` fires first; on cancellation the statement
/// running on the session is cancelled out of band (best effort).
pub(crate) async fn cancellable<T>(fut: impl Future<Output = DbResult<T>>, cancel: &CancellationToken, handle: Option<Arc<dyn CancelHandle>>) -> DbResult<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            if let Some(h) = handle
                && let Err(e) = h.cancel().await {
                    tracing::debug!("cancelling the running statement failed: {e}");
                }
            Err(DbError::cancelled())
        }
        r = fut => r,
    }
}

/// One object of an export job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExportItem {
    /// The result of a query.
    Query {
        /// Statement text.
        sql: String,
        /// Name used for `${table}` in file names (default `query`).
        name: Option<String>,
    },
    /// All (or filtered) rows of a table or view.
    Table {
        /// The table.
        table: ObjectRef,
        /// Optional filter without the `WHERE` keyword.
        where_clause: Option<String>,
    },
}

impl ExportItem {
    /// Name shown in progress and used for `${table}`.
    pub fn name(&self) -> String {
        match self {
            ExportItem::Query { name: Some(n), .. } if !n.trim().is_empty() => n.trim().to_string(),
            ExportItem::Query { .. } => "query".into(),
            ExportItem::Table { table, .. } => table.name.clone(),
        }
    }

    fn table(&self) -> Option<&ObjectRef> {
        match self {
            ExportItem::Table { table, .. } => Some(table),
            ExportItem::Query { .. } => None,
        }
    }

    fn sql(&self, dialect: &dyn Dialect) -> String {
        match self {
            ExportItem::Query { sql, .. } => sql.clone(),
            ExportItem::Table { table, where_clause } => dialect.select_table(table, None, where_clause.as_deref(), None, None),
        }
    }
}

/// A saved multi-object export (FR-T1, FR-T2, FR-T5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportJob {
    /// Objects to export; each produces one file.
    pub items: Vec<ExportItem>,
    /// Output format.
    pub format: ExportFormat,
    /// Format options.
    pub options: ExportOptions,
    /// Folder receiving the files (created when missing).
    pub output_folder: PathBuf,
    /// File name pattern with `${table}`, `${schema}`, `${database}`,
    /// `${timestamp}`, `${date}`, `${index}` and `${extension}` (alias
    /// `${format}`); the extension is appended when the pattern has none.
    pub file_name_pattern: String,
    /// Items exported in parallel (each with its own session); only used
    /// with `open_new_connections`.
    pub threads: usize,
    /// Open sessions through the factory per worker. When false every item
    /// runs sequentially on a single session from the factory.
    pub open_new_connections: bool,
    /// Keep exporting the remaining items after one fails.
    pub continue_on_error: bool,
    /// Run `SELECT COUNT(*)` first to show determinate progress.
    pub count_rows: bool,
}

impl Default for ExportJob {
    fn default() -> Self {
        ExportJob {
            items: Vec::new(),
            format: ExportFormat::Csv,
            options: ExportOptions::default(),
            output_folder: PathBuf::new(),
            file_name_pattern: DEFAULT_FILE_NAME_PATTERN.into(),
            threads: 1,
            open_new_connections: true,
            continue_on_error: true,
            count_rows: false,
        }
    }
}

/// Outcome of one item of an export job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExportItemSummary {
    /// Item name.
    pub name: String,
    /// Output file.
    pub file: Option<PathBuf>,
    /// Rows written.
    pub rows: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Warnings of the sink and source.
    pub warnings: Vec<String>,
    /// Error message when the item failed.
    pub error: Option<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

/// Outcome of an export job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExportJobSummary {
    /// Per item results, in job order.
    pub items: Vec<ExportItemSummary>,
    /// Rows written by all items.
    pub rows: u64,
    /// Bytes written by all items.
    pub bytes: u64,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

impl ExportJobSummary {
    /// Number of failed items.
    pub fn failed(&self) -> usize {
        self.items.iter().filter(|i| i.error.is_some()).count()
    }

    /// First error message, if any item failed.
    pub fn first_error(&self) -> Option<&str> {
        self.items.iter().find_map(|i| i.error.as_deref())
    }
}

/// Characters not allowed in file names on the supported platforms.
fn sanitize_file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "export".into() } else { s }
}

/// Expand a file name pattern for one item (see [`ExportJob::file_name_pattern`]).
/// `index` is 1-based.
pub fn expand_file_name(pattern: &str, item: &ExportItem, index: usize, format: ExportFormat, timestamp: NaiveDateTime) -> String {
    let pattern = if pattern.trim().is_empty() { DEFAULT_FILE_NAME_PATTERN } else { pattern };
    let table = item.table();
    let ext = format.extension();
    let has_ext = ["${extension}", "${ext}", "${format}"].iter().any(|v| pattern.contains(v));
    let mut name = pattern
        .replace("${table}", &item.name())
        .replace("${schema}", table.and_then(|t| t.schema.as_deref()).unwrap_or(""))
        .replace("${database}", table.and_then(|t| t.database.as_deref()).unwrap_or(""))
        .replace("${timestamp}", &timestamp.format("%Y%m%d%H%M%S").to_string())
        .replace("${date}", &timestamp.format("%Y%m%d").to_string())
        .replace("${index}", &index.to_string())
        .replace("${extension}", ext)
        .replace("${ext}", ext)
        .replace("${format}", ext);
    name = sanitize_file_name(&name);
    if !has_ext {
        name.push('.');
        name.push_str(ext);
    }
    name
}

/// Output paths for all items, made unique within the job.
fn plan_files(job: &ExportJob, timestamp: NaiveDateTime) -> Vec<PathBuf> {
    let mut names: Vec<String> = Vec::with_capacity(job.items.len());
    for (i, item) in job.items.iter().enumerate() {
        let name = expand_file_name(&job.file_name_pattern, item, i + 1, job.format, timestamp);
        let mut candidate = name.clone();
        let mut n = 2;
        while names.iter().any(|x| x.eq_ignore_ascii_case(&candidate)) {
            candidate = match name.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem}_{n}.{ext}"),
                None => format!("{name}_{n}"),
            };
            n += 1;
        }
        names.push(candidate);
    }
    names.into_iter().map(|n| job.output_folder.join(n)).collect()
}

/// Run a multi-object export job: every item is exported to its own file;
/// up to `threads` items run in parallel, each worker with its own session
/// from `factory`.
///
/// Returns per item results (failed items carry an error unless
/// `continue_on_error` is false, in which case the first error is returned).
/// Cancellation returns an error of kind [`ErrorKind::Cancelled`].
///
/// SQL scripts are written in the dialect of the source connection; use
/// [`run_export_job_with`] to target another database product.
pub async fn run_export_job(job: ExportJob, factory: Arc<dyn SessionFactory>, progress: Arc<Progress>, cancel: CancellationToken) -> DbResult<ExportJobSummary> {
    run_export_job_with(job, factory, None, progress, cancel).await
}

/// [`run_export_job`] with the dialect the SQL format writes for (e.g.
/// MySQL `INSERT`s from a PostgreSQL table); `None` uses the source dialect.
pub async fn run_export_job_with(
    job: ExportJob,
    factory: Arc<dyn SessionFactory>,
    sql_dialect: Option<Arc<dyn Dialect>>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> DbResult<ExportJobSummary> {
    let started = Instant::now();
    let mut job = job;
    let dialect = factory.dialect();
    if job.options.sql.source_kind.is_none() {
        job.options.sql.source_kind = Some(dialect.kind());
    }
    let output_dialect = sql_dialect.unwrap_or_else(|| dialect.clone());
    if job.items.is_empty() {
        return Ok(ExportJobSummary::default());
    }
    if !job.output_folder.as_os_str().is_empty() && !job.output_folder.exists() {
        std::fs::create_dir_all(&job.output_folder).map_err(|e| DbError::io(format!("Cannot create folder {}: {e}", job.output_folder.display())))?;
    }
    let files = plan_files(&job, chrono::Local::now().naive_local());
    progress.set_items_total(job.items.len() as u64);

    let n = job.items.len();
    let workers = if job.open_new_connections { job.threads.clamp(1, n) } else { 1 };
    let job = Arc::new(job);
    let files = Arc::new(files);
    let next = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<Option<ExportItemSummary>>>> = Arc::new(Mutex::new(vec![None; n]));
    let open_error: Arc<Mutex<Option<DbError>>> = Arc::new(Mutex::new(None));
    let stop = cancel.child_token();

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..workers {
        let (job, files, next, results, open_error) = (job.clone(), files.clone(), next.clone(), results.clone(), open_error.clone());
        let (factory, dialect, progress, stop) = (factory.clone(), dialect.clone(), progress.clone(), stop.clone());
        let output_dialect = output_dialect.clone();
        set.spawn(async move {
            let mut session = match cancellable(factory.open(), &stop, None).await {
                Ok(s) => s,
                Err(e) => {
                    open_error.lock().get_or_insert(e);
                    return;
                }
            };
            loop {
                if stop.is_cancelled() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= job.items.len() {
                    break;
                }
                let item = &job.items[i];
                let name = item.name();
                progress.item_started(&name);
                let t0 = Instant::now();
                let outcome = export_item(session.as_mut(), item, &files[i], &job, (&dialect, &output_dialect), &progress, &stop).await;
                progress.item_finished(&name);
                let mut summary = ExportItemSummary { name: name.clone(), file: Some(files[i].clone()), duration_ms: t0.elapsed().as_millis() as u64, ..Default::default() };
                match outcome {
                    Ok(s) => {
                        summary.rows = s.rows;
                        summary.bytes = s.bytes;
                        for w in &s.warnings {
                            progress.warn(Some(&name), w.clone());
                        }
                        summary.warnings = s.warnings;
                        progress.info(Some(&name), format!("{} rows exported to {}", s.rows, files[i].display()));
                    }
                    Err(e) => {
                        if e.kind != ErrorKind::Cancelled {
                            progress.add_errors(1);
                            progress.error(Some(&name), e.full_text());
                            if !job.continue_on_error {
                                stop.cancel();
                            }
                        }
                        summary.file = None;
                        summary.error = Some(e.full_text());
                    }
                }
                results.lock()[i] = Some(summary);
                if session.is_closed() {
                    match cancellable(factory.open(), &stop, None).await {
                        Ok(s) => session = s,
                        Err(e) => {
                            open_error.lock().get_or_insert(e);
                            break;
                        }
                    }
                }
            }
            let _ = session.close().await;
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Err(e) = joined {
            progress.error(None, format!("Export worker failed: {e}"));
        }
    }

    if cancel.is_cancelled() {
        return Err(DbError::cancelled());
    }
    let open_error = open_error.lock().take();
    let collected = std::mem::take(&mut *results.lock());
    let mut summary = ExportJobSummary::default();
    for (i, r) in collected.into_iter().enumerate() {
        let item = match r {
            Some(s) => s,
            None => ExportItemSummary {
                name: job.items[i].name(),
                error: Some(match &open_error {
                    Some(e) => format!("Cannot open a session: {}", e.full_text()),
                    None => "Not exported because an earlier item failed".into(),
                }),
                ..Default::default()
            },
        };
        summary.rows += item.rows;
        summary.bytes += item.bytes;
        summary.items.push(item);
    }
    summary.duration_ms = started.elapsed().as_millis() as u64;
    if !job.continue_on_error
        && let Some((i, err)) = summary.items.iter().enumerate().find_map(|(i, s)| s.error.clone().map(|e| (i, e)))
    {
        return Err(with_item(DbError::query(err), &job.items[i].name()));
    }
    Ok(summary)
}

/// Export one item on `session`. `dialects` are the session's dialect (for
/// the queries) and the dialect SQL output is written in.
async fn export_item(
    session: &mut dyn Session,
    item: &ExportItem,
    path: &std::path::Path,
    job: &ExportJob,
    dialects: (&Arc<dyn Dialect>, &Arc<dyn Dialect>),
    progress: &Progress,
    cancel: &CancellationToken,
) -> DbResult<ExportSummary> {
    let (dialect, output_dialect) = dialects;
    let sql = item.sql(dialect.as_ref());
    let handle = session.cancel_handle();
    let mut table_def = None;
    if let (Some(table), ExportFormat::Sql) = (item.table(), job.format)
        && (job.options.sql.include_create_table || job.options.sql.mode == crate::options::SqlInsertMode::Upsert)
    {
        match cancellable(session.table_definition(table), cancel, handle.clone()).await {
            Ok(def) => table_def = Some(def),
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e) => progress.warn(Some(&item.name()), format!("Cannot read the table definition: {e}")),
        }
    }
    let mut total = None;
    if job.count_rows {
        let count_sql = dialect.count_query(&sql);
        match cancellable(query_scalar(session, &count_sql, &[]), cancel, handle.clone()).await {
            Ok(v) => total = v.as_i64().and_then(|n| u64::try_from(n).ok()),
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e) => progress.warn(Some(&item.name()), format!("Cannot count rows: {e}")),
        }
    }
    let cursor = cancellable(session.execute(&sql, &[], &ExecOptions::default()), cancel, handle.clone()).await?;
    let source = cancellable(CursorSource::new(cursor), cancel, handle.clone()).await?;
    let mut source = source.with_table(item.table().cloned()).with_total_rows(total);
    let mut sink = create_sink(job.format, &job.options, ExportTarget::Path(path.to_path_buf()), Some(output_dialect.clone()))?;
    if let Some(def) = &table_def {
        sink.set_table_definition(def);
    }
    let result = run_export_batched(&mut source, sink.as_mut(), job.options.fetch_size, progress, cancel).await;
    if let (Err(e), Some(h)) = (&result, &handle)
        && e.kind == ErrorKind::Cancelled
    {
        let _ = h.cancel().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn file_names() {
        let ts = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap().and_hms_opt(9, 8, 7).unwrap();
        let t = ExportItem::Table { table: ObjectRef::table(Some("public".into()), "users"), where_clause: None };
        assert_eq!(expand_file_name("${table}_${timestamp}", &t, 1, ExportFormat::Csv, ts), "users_20261018090807.csv");
        assert_eq!(expand_file_name("${schema}.${table}-${index}.${extension}", &t, 3, ExportFormat::Xlsx, ts), "public.users-3.xlsx");
        let q = ExportItem::Query { sql: "select 1".into(), name: Some("a/b".into()) };
        assert_eq!(expand_file_name("${table}_${date}", &q, 1, ExportFormat::JsonLines, ts), "a_b_20261018.jsonl");
        assert_eq!(expand_file_name("", &q, 1, ExportFormat::Sql, ts), "a_b_20261018090807.sql");
    }

    #[test]
    fn planned_files_are_unique() {
        let job = ExportJob {
            items: vec![ExportItem::Query { sql: "a".into(), name: None }, ExportItem::Query { sql: "b".into(), name: None }, ExportItem::Query { sql: "c".into(), name: None }],
            file_name_pattern: "${table}".into(),
            output_folder: PathBuf::from("out"),
            ..Default::default()
        };
        let ts = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let files = plan_files(&job, ts);
        assert_eq!(files, vec![PathBuf::from("out/query.csv"), PathBuf::from("out/query_2.csv"), PathBuf::from("out/query_3.csv")]);
    }

    #[test]
    fn job_serde() {
        let job = ExportJob {
            items: vec![ExportItem::Table { table: ObjectRef::table(Some("s".into()), "t"), where_clause: Some("id > 5".into()) }],
            format: ExportFormat::Parquet,
            threads: 4,
            ..Default::default()
        };
        let s = serde_json::to_string(&job).unwrap();
        let back: ExportJob = serde_json::from_str(&s).unwrap();
        assert_eq!(back, job);
        let minimal: ExportJob = serde_json::from_str(r#"{"items":[{"type":"query","sql":"select 1"}]}"#).unwrap();
        assert_eq!(minimal.items[0].name(), "query");
        assert_eq!(minimal.file_name_pattern, DEFAULT_FILE_NAME_PATTERN);
    }
}
