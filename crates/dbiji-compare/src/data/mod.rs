//! Data compare between two tables (PRD FR-M3).
//!
//! Rows are matched by key columns. When every key column has a logical kind
//! whose sort order is the same in every database (numbers, dates, times,
//! booleans, UUIDs), both sides are read `ORDER BY key` and merge-joined in
//! constant memory. Text keys sort by collation, which differs between
//! servers, so they are matched with a hash join: the source rows are held in
//! memory up to [`DataCompareOptions::max_hash_rows`] /
//! [`DataCompareOptions::max_hash_bytes`], above which the comparison stops
//! with a clear error.
//!
//! The merge join keeps two cursors open at the same time, so the source and
//! target providers must use different sessions. The hash join reads the
//! source completely and closes its cursor before querying the target, so
//! both providers may share one session.

mod values;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use dbiji_core::driver::{ExecOptions, QueryCursor, ResultHeader, Session};
use dbiji_core::{ColumnMeta, DbError, DbKind, DbResult, Dialect, ObjectRef, Row, Value, tr, trf};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use tokio_util::sync::CancellationToken;
use values::{cross_key_repr, key_repr, keys_cmp, orderable_kind, values_equal};

/// One side of a data comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSpec {
    /// The table (or view) to read.
    pub obj: ObjectRef,
    /// Columns identifying a row (primary key or another unique key).
    pub key_columns: Vec<String>,
    /// Columns to compare; empty = every column returned by `SELECT *`
    /// (matched by name between the two sides). When both sides list
    /// columns they are matched by position.
    pub columns: Vec<String>,
    /// Optional filter (SQL expression without `WHERE`).
    pub where_clause: Option<String>,
}

impl TableSpec {
    /// Compare every column of `obj`, identifying rows by `key_columns`.
    pub fn new(obj: ObjectRef, key_columns: Vec<String>) -> Self {
        TableSpec { obj, key_columns, columns: Vec::new(), where_clause: None }
    }
}

/// How rows of the two sides are matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareStrategy {
    /// Merge join for numeric / temporal keys, hash join otherwise.
    #[default]
    Auto,
    /// Always merge join (only correct when both servers sort the keys identically).
    MergeJoin,
    /// Always hash join.
    HashJoin,
}

/// Options of a data comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataCompareOptions {
    /// Compare text case-insensitively (also applies to text keys).
    pub ignore_case: bool,
    /// Ignore trailing whitespace of text values (`CHAR(n)` padding).
    pub trim_text: bool,
    /// Numbers whose difference is at most this value are equal (0 = exact).
    pub numeric_tolerance: f64,
    /// Maximum number of differing rows kept in [`DataDiff::samples`].
    pub max_samples: usize,
    /// How rows are matched.
    pub strategy: CompareStrategy,
    /// Maximum rows held in memory by the hash join.
    pub max_hash_rows: usize,
    /// Maximum (approximate) bytes held in memory by the hash join.
    pub max_hash_bytes: usize,
    /// Rows fetched per round trip.
    pub fetch_size: usize,
    /// Generate INSERT / UPDATE / DELETE statements making the target equal to the source.
    pub generate_sync: bool,
    /// Maximum number of sync statements kept.
    pub max_sync_statements: usize,
}

impl Default for DataCompareOptions {
    fn default() -> Self {
        DataCompareOptions {
            ignore_case: false,
            trim_text: false,
            numeric_tolerance: 0.0,
            max_samples: 100,
            strategy: CompareStrategy::Auto,
            max_hash_rows: 1_000_000,
            max_hash_bytes: 512 * 1024 * 1024,
            fetch_size: 2000,
            generate_sync: true,
            max_sync_statements: 100_000,
        }
    }
}

/// A data comparison to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataCompareJob {
    /// The reference data.
    pub source: TableSpec,
    /// The data made equal to the source by the sync statements.
    pub target: TableSpec,
    /// Matching and normalisation options.
    pub options: DataCompareOptions,
}

/// Something that can run a query and return its cursor (a session, or a
/// test fake).
#[async_trait]
pub trait RowProvider: Send {
    /// Execute `sql` and return its cursor.
    async fn query(&mut self, sql: &str) -> DbResult<Box<dyn QueryCursor>>;
}

#[async_trait]
impl RowProvider for &mut dyn Session {
    async fn query(&mut self, sql: &str) -> DbResult<Box<dyn QueryCursor>> {
        (**self).execute(sql, &[], &ExecOptions::default()).await
    }
}

#[async_trait]
impl RowProvider for Box<dyn Session> {
    async fn query(&mut self, sql: &str) -> DbResult<Box<dyn QueryCursor>> {
        (**self).execute(sql, &[], &ExecOptions::default()).await
    }
}

/// Kind of a row difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowDiffKind {
    /// Same key, different values.
    Different,
    /// The key exists only in the source (INSERT into the target).
    OnlyInSource,
    /// The key exists only in the target (DELETE from the target).
    OnlyInTarget,
}

/// A differing row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowDiff {
    /// What differs.
    pub kind: RowDiffKind,
    /// Key values (source values when the row exists in the source).
    pub key: Vec<Value>,
    /// Full source row as returned by the source query.
    pub source: Option<Row>,
    /// Full target row as returned by the target query.
    pub target: Option<Row>,
    /// Names (source side) of the columns that differ.
    pub columns: Vec<String>,
}

/// Progress of a running comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataCompareProgress {
    /// Source rows read so far.
    pub source_rows: u64,
    /// Target rows read so far.
    pub target_rows: u64,
    /// Matching rows with equal values.
    pub identical: u64,
    /// Matching rows with different values.
    pub different: u64,
    /// Rows only in the source.
    pub only_in_source: u64,
    /// Rows only in the target.
    pub only_in_target: u64,
}

/// Result of [`run_data_compare`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DataDiff {
    /// Matching rows with equal values.
    pub identical: u64,
    /// Matching rows with different values.
    pub different: u64,
    /// Rows only in the source.
    pub only_in_source: u64,
    /// Rows only in the target.
    pub only_in_target: u64,
    /// Source rows read.
    pub source_rows: u64,
    /// Target rows read.
    pub target_rows: u64,
    /// Rows skipped because a key column was NULL.
    pub skipped_null_keys: u64,
    /// Strategy actually used.
    pub strategy: CompareStrategy,
    /// Compared (non key) columns, source names.
    pub columns: Vec<String>,
    /// Columns present on one side only (not compared).
    pub unmatched_columns: Vec<String>,
    /// Up to [`DataCompareOptions::max_samples`] differing rows.
    pub samples: Vec<RowDiff>,
    /// Statements making the target equal to the source: deletes, updates, inserts.
    pub sync_statements: Vec<String>,
    /// More sync statements were needed than [`DataCompareOptions::max_sync_statements`].
    pub sync_truncated: bool,
    /// Notes for the user (unmatched columns ...).
    pub warnings: Vec<String>,
}

impl DataDiff {
    /// No row differs.
    pub fn is_identical(&self) -> bool {
        self.different == 0 && self.only_in_source == 0 && self.only_in_target == 0
    }

    fn progress(&self) -> DataCompareProgress {
        DataCompareProgress {
            source_rows: self.source_rows,
            target_rows: self.target_rows,
            identical: self.identical,
            different: self.different,
            only_in_source: self.only_in_source,
            only_in_target: self.only_in_target,
        }
    }
}

struct Stream {
    cursor: Box<dyn QueryCursor>,
    meta: Vec<ColumnMeta>,
    buf: VecDeque<Row>,
    done: bool,
    fetch: usize,
    rows: u64,
}

impl Stream {
    async fn open(p: &mut dyn RowProvider, sql: &str, fetch: usize) -> DbResult<Stream> {
        let mut cursor = p.query(sql).await?;
        loop {
            match cursor.next_result().await {
                Ok(Some(ResultHeader::Rows(meta))) => {
                    return Ok(Stream { cursor, meta, buf: VecDeque::new(), done: false, fetch: fetch.max(1), rows: 0 });
                }
                Ok(Some(ResultHeader::Update { .. })) => continue,
                Ok(None) => {
                    let _ = cursor.close().await;
                    return Err(DbError::query(trf("The query did not return rows: {}", &[&sql])));
                }
                Err(e) => {
                    let _ = cursor.close().await;
                    return Err(e);
                }
            }
        }
    }

    async fn next(&mut self, cancel: &CancellationToken) -> DbResult<Option<Row>> {
        if self.buf.is_empty() && !self.done {
            if cancel.is_cancelled() {
                return Err(DbError::cancelled());
            }
            let batch = self.cursor.fetch(self.fetch).await?;
            if batch.is_empty() {
                self.done = true;
            } else {
                self.buf.extend(batch);
            }
        }
        let r = self.buf.pop_front();
        if r.is_some() {
            self.rows += 1;
        }
        Ok(r)
    }

    async fn close(&mut self) {
        let _ = self.cursor.close().await;
    }
}

fn find_column(meta: &[ColumnMeta], name: &str) -> Option<usize> {
    meta.iter().position(|c| c.name == name).or_else(|| meta.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
}

fn select_sql(spec: &TableSpec, d: &dyn Dialect, order: bool) -> String {
    let cols: Option<Vec<String>> = if spec.columns.is_empty() {
        None
    } else {
        let mut c = spec.key_columns.clone();
        c.extend(spec.columns.iter().filter(|x| !spec.key_columns.contains(x)).cloned());
        Some(c)
    };
    let order_by = if order { Some(spec.key_columns.iter().map(|k| d.quote_ident(k)).collect::<Vec<_>>().join(", ")) } else { None };
    d.select_table(&spec.obj, cols.as_deref(), spec.where_clause.as_deref(), order_by.as_deref(), None)
}

/// Positions of key and compared columns in both results.
struct Layout {
    src_keys: Vec<usize>,
    tgt_keys: Vec<usize>,
    src_cols: Vec<usize>,
    tgt_cols: Vec<usize>,
}

fn target_keys(job: &DataCompareJob) -> Vec<String> {
    if job.target.key_columns.is_empty() { job.source.key_columns.clone() } else { job.target.key_columns.clone() }
}

fn key_positions(meta: &[ColumnMeta], keys: &[String], side: &str) -> DbResult<Vec<usize>> {
    keys.iter().map(|k| find_column(meta, k).ok_or_else(|| DbError::config(trf("Key column {} not found in the {} result", &[k, &side])))).collect()
}

fn layout(job: &DataCompareJob, src: &[ColumnMeta], tgt: &[ColumnMeta], diff: &mut DataDiff) -> DbResult<Layout> {
    let src_keys = key_positions(src, &job.source.key_columns, "source")?;
    let tgt_keys = key_positions(tgt, &target_keys(job), "target")?;
    let (mut src_cols, mut tgt_cols) = (Vec::new(), Vec::new());
    if job.source.columns.is_empty() && job.target.columns.is_empty() {
        for (i, c) in src.iter().enumerate() {
            if src_keys.contains(&i) {
                continue;
            }
            match find_column(tgt, &c.name).filter(|t| !tgt_keys.contains(t)) {
                Some(t) => {
                    src_cols.push(i);
                    tgt_cols.push(t);
                    diff.columns.push(c.name.clone());
                }
                None => diff.unmatched_columns.push(c.name.clone()),
            }
        }
        for (i, c) in tgt.iter().enumerate() {
            if !tgt_keys.contains(&i) && !tgt_cols.contains(&i) {
                diff.unmatched_columns.push(c.name.clone());
            }
        }
        if !diff.unmatched_columns.is_empty() {
            diff.warnings.push(trf("Columns present on one side only are not compared: {}", &[&diff.unmatched_columns.join(", ")]));
        }
    } else {
        let src_names = &job.source.columns;
        let tgt_names = if job.target.columns.is_empty() { &job.source.columns } else { &job.target.columns };
        if src_names.len() != tgt_names.len() {
            return Err(DbError::config(tr("Source and target must list the same number of columns")));
        }
        for (s, t) in src_names.iter().zip(tgt_names) {
            let si = find_column(src, s).ok_or_else(|| DbError::config(trf("Column {} not found in the source result", &[s])))?;
            let ti = find_column(tgt, t).ok_or_else(|| DbError::config(trf("Column {} not found in the target result", &[t])))?;
            src_cols.push(si);
            tgt_cols.push(ti);
            diff.columns.push(src[si].name.clone());
        }
    }
    Ok(Layout { src_keys, tgt_keys, src_cols, tgt_cols })
}

struct Engine<'j> {
    job: &'j DataCompareJob,
    dst: &'j dyn Dialect,
    lay: Layout,
    tgt_meta: Vec<ColumnMeta>,
    src_names: Vec<String>,
    diff: DataDiff,
    deletes: Vec<String>,
    updates: Vec<String>,
    inserts: Vec<String>,
}

impl Engine<'_> {
    fn o(&self) -> &DataCompareOptions {
        &self.job.options
    }

    fn lit(&self, tgt_col: usize, v: &Value) -> String {
        let ty = self.tgt_meta.get(tgt_col).map(|c| c.type_name.as_str()).filter(|t| !t.is_empty());
        self.dst.literal(v, ty)
    }

    fn table(&self) -> String {
        self.dst.qualified_ref(&self.job.target.obj)
    }

    fn sync_room(&mut self) -> bool {
        if !self.o().generate_sync {
            return false;
        }
        if self.deletes.len() + self.updates.len() + self.inserts.len() >= self.o().max_sync_statements {
            self.diff.sync_truncated = true;
            return false;
        }
        true
    }

    fn key_where(&self, t_row: &Row) -> String {
        self.lay.tgt_keys.iter().map(|&k| format!("{} = {}", self.dst.quote_ident(&self.tgt_meta[k].name), self.lit(k, cell(t_row, k)))).collect::<Vec<_>>().join(" AND ")
    }

    fn sample(&mut self, kind: RowDiffKind, key: Vec<Value>, source: Option<&Row>, target: Option<&Row>, columns: Vec<String>) {
        if self.diff.samples.len() < self.o().max_samples {
            self.diff.samples.push(RowDiff { kind, key, source: source.cloned(), target: target.cloned(), columns });
        }
    }

    fn compare(&mut self, key: Vec<Value>, s: &Row, t: &Row) {
        let mut differing = Vec::new();
        for (i, (&si, &ti)) in self.lay.src_cols.iter().zip(&self.lay.tgt_cols).enumerate() {
            if !values_equal(cell(s, si), cell(t, ti), &self.job.options) {
                differing.push(i);
            }
        }
        if differing.is_empty() {
            self.diff.identical += 1;
            return;
        }
        self.diff.different += 1;
        let names: Vec<String> = differing.iter().map(|&i| self.src_names[i].clone()).collect();
        if self.sync_room() {
            let sets: Vec<String> = differing
                .iter()
                .map(|&i| {
                    let (si, ti) = (self.lay.src_cols[i], self.lay.tgt_cols[i]);
                    format!("{} = {}", self.dst.quote_ident(&self.tgt_meta[ti].name), self.lit(ti, cell(s, si)))
                })
                .collect();
            let sql = format!("UPDATE {} SET {} WHERE {}", self.table(), sets.join(", "), self.key_where(t));
            self.updates.push(sql);
        }
        self.sample(RowDiffKind::Different, key, Some(s), Some(t), names);
    }

    fn only_source(&mut self, key: Vec<Value>, s: &Row) {
        self.diff.only_in_source += 1;
        if self.sync_room() {
            let mut cols = Vec::new();
            let mut vals = Vec::new();
            for (&sk, &tk) in self.lay.src_keys.iter().zip(&self.lay.tgt_keys) {
                cols.push(self.dst.quote_ident(&self.tgt_meta[tk].name));
                vals.push(self.lit(tk, cell(s, sk)));
            }
            for (&sc, &tc) in self.lay.src_cols.iter().zip(&self.lay.tgt_cols) {
                cols.push(self.dst.quote_ident(&self.tgt_meta[tc].name));
                vals.push(self.lit(tc, cell(s, sc)));
            }
            let sql = format!("INSERT INTO {} ({}) VALUES ({})", self.table(), cols.join(", "), vals.join(", "));
            self.inserts.push(sql);
        }
        self.sample(RowDiffKind::OnlyInSource, key, Some(s), None, Vec::new());
    }

    fn only_target(&mut self, key: Vec<Value>, t: &Row) {
        self.diff.only_in_target += 1;
        if self.sync_room() {
            let sql = format!("DELETE FROM {} WHERE {}", self.table(), self.key_where(t));
            self.deletes.push(sql);
        }
        self.sample(RowDiffKind::OnlyInTarget, key, None, Some(t), Vec::new());
    }

    fn finish(mut self, src_rows: u64, tgt_rows: u64) -> DataDiff {
        self.diff.source_rows = src_rows;
        self.diff.target_rows = tgt_rows;
        // SQL Server refuses explicit values for an identity column unless IDENTITY_INSERT is on.
        if self.dst.kind() == dbiji_core::DbKind::SqlServer && !self.inserts.is_empty() {
            let table = self.table();
            let names: Vec<String> = self.lay.tgt_keys.iter().chain(&self.lay.tgt_cols).map(|&i| self.dst.string_literal(&self.tgt_meta[i].name)).collect();
            let when =
                format!("IF EXISTS (SELECT 1 FROM sys.identity_columns WHERE object_id = OBJECT_ID({}) AND name IN ({}))", self.dst.string_literal(&table), names.join(", "));
            self.inserts.insert(0, format!("{when} SET IDENTITY_INSERT {table} ON"));
            self.inserts.push(format!("{when} SET IDENTITY_INSERT {table} OFF"));
        }
        let mut sync = std::mem::take(&mut self.deletes);
        sync.append(&mut self.updates);
        sync.append(&mut self.inserts);
        self.diff.sync_statements = sync;
        self.diff
    }
}

static NULL_VALUE: Value = Value::Null;

/// Cell `i` of a row; `NULL` when a driver returned a short row.
fn cell(row: &Row, i: usize) -> &Value {
    row.get(i).unwrap_or(&NULL_VALUE)
}

fn key_of(row: &Row, idx: &[usize]) -> Option<Vec<Value>> {
    let k: Vec<Value> = idx.iter().map(|&i| row.get(i).cloned().unwrap_or(Value::Null)).collect();
    if k.iter().any(Value::is_null) { None } else { Some(k) }
}

/// Next row with a non-NULL key, checking the sort order of a merge join input.
async fn next_sorted(
    s: &mut Stream,
    idx: &[usize],
    last: &mut Option<Vec<Value>>,
    skipped: &mut u64,
    side: &str,
    cancel: &CancellationToken,
) -> DbResult<Option<(Vec<Value>, Row)>> {
    loop {
        let Some(row) = s.next(cancel).await? else { return Ok(None) };
        let Some(key) = key_of(&row, idx) else {
            *skipped += 1;
            continue;
        };
        if let Some(prev) = last.as_ref() {
            match keys_cmp(&key, prev) {
                Ordering::Greater => {}
                Ordering::Equal => {
                    return Err(DbError::query(trf("Duplicate key in the {} table: {}", &[&side, &key_text(&key)])));
                }
                Ordering::Less => {
                    return Err(DbError::query(trf("The {} rows are not sorted by key (use the hash join strategy): {}", &[&side, &key_text(&key)])));
                }
            }
        }
        *last = Some(key.clone());
        return Ok(Some((key, row)));
    }
}

fn key_text(key: &[Value]) -> String {
    key.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
}

fn row_bytes(row: &Row) -> usize {
    std::mem::size_of::<Row>() + row.iter().map(|v| std::mem::size_of::<Value>() + v.heap_size()).sum::<usize>()
}

const PROGRESS_EVERY: u64 = 5000;

/// Compare the rows of `job.source` (read through `source` with
/// `src_dialect`) with `job.target` (read through `target` with
/// `dst_dialect`). `progress` is called periodically; `cancel` stops the
/// comparison with [`dbiji_core::ErrorKind::Cancelled`].
pub async fn run_data_compare(
    job: &DataCompareJob,
    source: &mut dyn RowProvider,
    target: &mut dyn RowProvider,
    src_dialect: &dyn Dialect,
    dst_dialect: &dyn Dialect,
    progress: &mut (dyn FnMut(&DataCompareProgress) + Send),
    cancel: &CancellationToken,
) -> DbResult<DataDiff> {
    if job.source.key_columns.is_empty() {
        return Err(DbError::config(tr("Data compare needs at least one key column")));
    }
    // The comparison reads rows with SQL (filters, orderings, counts): a
    // document or key/value store has none.
    for d in [src_dialect, dst_dialect] {
        if d.kind().is_shell() {
            return Err(DbError::unsupported(format!("Data compare needs SQL on both sides; {} speaks its own commands", d.kind().display_name())));
        }
    }
    if !job.target.key_columns.is_empty() && job.target.key_columns.len() != job.source.key_columns.len() {
        return Err(DbError::config(tr("Source and target must have the same number of key columns")));
    }
    let o = &job.options;
    let uuid_ok = src_dialect.kind() != DbKind::SqlServer && dst_dialect.kind() != DbKind::SqlServer;
    let mut src = Stream::open(source, &select_sql(&job.source, src_dialect, o.strategy != CompareStrategy::HashJoin), o.fetch_size).await?;
    let src_key_idx = match key_positions(&src.meta, &job.source.key_columns, "source") {
        Ok(k) => k,
        Err(e) => {
            src.close().await;
            return Err(e);
        }
    };
    let orderable = |meta: &[ColumnMeta], idx: &[usize]| idx.iter().all(|&i| orderable_kind(meta[i].kind, uuid_ok));
    let merge = match o.strategy {
        CompareStrategy::MergeJoin => true,
        CompareStrategy::HashJoin => false,
        CompareStrategy::Auto => orderable(&src.meta, &src_key_idx),
    };
    let result = if merge {
        let mut tgt = match Stream::open(target, &select_sql(&job.target, dst_dialect, true), o.fetch_size).await {
            Ok(t) => t,
            Err(e) => {
                src.close().await;
                return Err(e);
            }
        };
        let r = compare_open(job, &mut src, &mut tgt, dst_dialect, progress, cancel, uuid_ok).await;
        src.close().await;
        tgt.close().await;
        r
    } else {
        hash_join(job, &mut src, target, dst_dialect, progress, cancel).await
    };
    let diff = result?;
    progress(&diff.progress());
    Ok(diff)
}

/// Both cursors are open: merge join when the target keys are orderable too.
async fn compare_open(
    job: &DataCompareJob,
    src: &mut Stream,
    tgt: &mut Stream,
    dst: &dyn Dialect,
    progress: &mut (dyn FnMut(&DataCompareProgress) + Send),
    cancel: &CancellationToken,
    uuid_ok: bool,
) -> DbResult<DataDiff> {
    let mut diff = DataDiff { strategy: CompareStrategy::MergeJoin, ..Default::default() };
    let lay = layout(job, &src.meta, &tgt.meta, &mut diff)?;
    let tgt_orderable = lay.tgt_keys.iter().all(|&i| orderable_kind(tgt.meta[i].kind, uuid_ok));
    if job.options.strategy == CompareStrategy::Auto && !tgt_orderable {
        diff.strategy = CompareStrategy::HashJoin;
        return hash_join_open(job, src, tgt, dst, progress, cancel).await;
    }
    let src_names = lay.src_cols.iter().map(|&i| src.meta[i].name.clone()).collect();
    let mut e = Engine { job, dst, lay, tgt_meta: tgt.meta.clone(), src_names, diff, deletes: Vec::new(), updates: Vec::new(), inserts: Vec::new() };
    let (mut s_last, mut t_last) = (None, None);
    let mut skipped = 0u64;
    let src_keys = e.lay.src_keys.clone();
    let tgt_keys = e.lay.tgt_keys.clone();
    let mut s_cur = next_sorted(src, &src_keys, &mut s_last, &mut skipped, "source", cancel).await?;
    let mut t_cur = next_sorted(tgt, &tgt_keys, &mut t_last, &mut skipped, "target", cancel).await?;
    let mut processed = 0u64;
    loop {
        let (adv_s, adv_t) = match (&s_cur, &t_cur) {
            (None, None) => break,
            (Some((k, r)), None) => {
                e.only_source(k.clone(), r);
                (true, false)
            }
            (None, Some((k, r))) => {
                e.only_target(k.clone(), r);
                (false, true)
            }
            (Some((sk, sr)), Some((tk, tr))) => match keys_cmp(sk, tk) {
                Ordering::Less => {
                    e.only_source(sk.clone(), sr);
                    (true, false)
                }
                Ordering::Greater => {
                    e.only_target(tk.clone(), tr);
                    (false, true)
                }
                Ordering::Equal => {
                    e.compare(sk.clone(), sr, tr);
                    (true, true)
                }
            },
        };
        if adv_s {
            s_cur = next_sorted(src, &src_keys, &mut s_last, &mut skipped, "source", cancel).await?;
        }
        if adv_t {
            t_cur = next_sorted(tgt, &tgt_keys, &mut t_last, &mut skipped, "target", cancel).await?;
        }
        processed += 1;
        if processed.is_multiple_of(PROGRESS_EVERY) {
            e.diff.source_rows = src.rows;
            e.diff.target_rows = tgt.rows;
            progress(&e.diff.progress());
        }
    }
    e.diff.skipped_null_keys = skipped;
    Ok(e.finish(src.rows, tgt.rows))
}

/// Source rows keyed by the canonical key text, and the key text of text keys read
/// as numbers / UUIDs ([`cross_key_repr`]; `None` when two keys give the same one).
type SourceRows = (HashMap<String, (Vec<Value>, Row)>, HashMap<String, Option<String>>);

/// Read the source into memory keyed by the canonical key text.
async fn load_source(job: &DataCompareJob, src: &mut Stream, cancel: &CancellationToken, skipped: &mut u64) -> DbResult<SourceRows> {
    let o = &job.options;
    let idx = key_positions(&src.meta, &job.source.key_columns, "source")?;
    let mut map: HashMap<String, (Vec<Value>, Row)> = HashMap::new();
    let mut cross: HashMap<String, Option<String>> = HashMap::new();
    let mut bytes = 0usize;
    while let Some(row) = src.next(cancel).await? {
        let Some(key) = key_of(&row, &idx) else {
            *skipped += 1;
            continue;
        };
        let repr = key_repr(&key, o);
        bytes += row_bytes(&row) + repr.len() * 2 + 64;
        if map.len() >= o.max_hash_rows || bytes > o.max_hash_bytes {
            return Err(DbError::unsupported(trf(
                "The source has more than {} rows (or {} MB) and its keys are not numeric or temporal: add a WHERE clause, compare a numeric key, or raise the hash join limits",
                &[&o.max_hash_rows, &(o.max_hash_bytes / (1024 * 1024))],
            )));
        }
        let alt = cross_key_repr(&key, o);
        if alt != repr {
            cross.entry(alt).and_modify(|e| *e = None).or_insert_with(|| Some(repr.clone()));
        }
        if map.insert(repr, (key.clone(), row)).is_some() {
            return Err(DbError::query(trf("Duplicate key in the {} table: {}", &[&"source", &key_text(&key)])));
        }
    }
    Ok((map, cross))
}

#[allow(clippy::too_many_arguments)]
async fn probe_target(
    job: &DataCompareJob,
    (mut map, cross): SourceRows,
    src_meta: &[ColumnMeta],
    tgt: &mut Stream,
    dst: &dyn Dialect,
    progress: &mut (dyn FnMut(&DataCompareProgress) + Send),
    cancel: &CancellationToken,
    mut skipped: u64,
    src_rows: u64,
) -> DbResult<DataDiff> {
    let mut diff = DataDiff { strategy: CompareStrategy::HashJoin, ..Default::default() };
    let lay = layout(job, src_meta, &tgt.meta, &mut diff)?;
    let src_names = lay.src_cols.iter().map(|&i| src_meta[i].name.clone()).collect();
    let tgt_keys = lay.tgt_keys.clone();
    let mut e = Engine { job, dst, lay, tgt_meta: tgt.meta.clone(), src_names, diff, deletes: Vec::new(), updates: Vec::new(), inserts: Vec::new() };
    let mut processed = 0u64;
    while let Some(row) = tgt.next(cancel).await? {
        let Some(key) = key_of(&row, &tgt_keys) else {
            skipped += 1;
            continue;
        };
        // Exactly the same key, else a text key of one side spelling the other's number / UUID.
        let repr = key_repr(&key, &job.options);
        let found = map.remove(&repr).or_else(|| {
            let alt = cross_key_repr(&key, &job.options);
            if alt != repr
                && let Some(hit) = map.remove(&alt)
            {
                return Some(hit);
            }
            cross.get(&repr).cloned().flatten().and_then(|r| map.remove(&r))
        });
        match found {
            Some((sk, sr)) => e.compare(sk, &sr, &row),
            None => e.only_target(key, &row),
        }
        processed += 1;
        if processed.is_multiple_of(PROGRESS_EVERY) {
            e.diff.source_rows = src_rows;
            e.diff.target_rows = tgt.rows;
            progress(&e.diff.progress());
        }
    }
    let mut rest: Vec<(Vec<Value>, Row)> = map.into_values().collect();
    rest.sort_by(|a, b| keys_cmp(&a.0, &b.0));
    for (k, r) in rest {
        e.only_source(k, &r);
    }
    e.diff.skipped_null_keys = skipped;
    Ok(e.finish(src_rows, tgt.rows))
}

/// Hash join with both cursors already open.
async fn hash_join_open(
    job: &DataCompareJob,
    src: &mut Stream,
    tgt: &mut Stream,
    dst: &dyn Dialect,
    progress: &mut (dyn FnMut(&DataCompareProgress) + Send),
    cancel: &CancellationToken,
) -> DbResult<DataDiff> {
    let mut skipped = 0;
    let map = load_source(job, src, cancel, &mut skipped).await?;
    let meta = src.meta.clone();
    probe_target(job, map, &meta, tgt, dst, progress, cancel, skipped, src.rows).await
}

/// Hash join reading the source first, closing it, then querying the target
/// (works with providers sharing one session).
async fn hash_join(
    job: &DataCompareJob,
    src: &mut Stream,
    target: &mut dyn RowProvider,
    dst: &dyn Dialect,
    progress: &mut (dyn FnMut(&DataCompareProgress) + Send),
    cancel: &CancellationToken,
) -> DbResult<DataDiff> {
    let mut skipped = 0;
    let loaded = load_source(job, src, cancel, &mut skipped).await;
    src.close().await;
    let map = loaded?;
    let meta = src.meta.clone();
    let src_rows = src.rows;
    let mut tgt = Stream::open(target, &select_sql(&job.target, dst, false), job.options.fetch_size).await?;
    let r = probe_target(job, map, &meta, &mut tgt, dst, progress, cancel, skipped, src_rows).await;
    tgt.close().await;
    r
}
