//! Row sources: where exported (or imported) rows come from.

use async_trait::async_trait;
use dbiji_core::driver::{ExecOptions, QueryCursor, ResultHeader, Session};
use dbiji_core::{ColumnMeta, DbError, DbResult, ObjectRef, Row, SharedRowStore, Value};
use std::collections::VecDeque;

/// A stream of rows read in batches.
#[async_trait]
pub trait RowSource: Send {
    /// Columns of every row.
    fn columns(&self) -> &[ColumnMeta];

    /// Up to `max` more rows; an empty vector means the source is exhausted.
    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>>;

    /// Total number of rows when known in advance (for progress).
    fn total_rows(&self) -> Option<u64> {
        None
    }

    /// Table the rows come from, when known.
    fn table(&self) -> Option<&ObjectRef> {
        None
    }

    /// Release resources (cursor, file). Called once after the last batch or on failure.
    async fn close(&mut self) -> DbResult<()> {
        Ok(())
    }

    /// Non fatal problems noticed while reading (ragged rows, invalid characters ...).
    fn take_warnings(&mut self) -> Vec<String> {
        Vec::new()
    }
}

#[async_trait]
impl<S: RowSource + ?Sized> RowSource for Box<S> {
    fn columns(&self) -> &[ColumnMeta] {
        (**self).columns()
    }
    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        (**self).next_batch(max).await
    }
    fn total_rows(&self) -> Option<u64> {
        (**self).total_rows()
    }
    fn table(&self) -> Option<&ObjectRef> {
        (**self).table()
    }
    async fn close(&mut self) -> DbResult<()> {
        (**self).close().await
    }
    fn take_warnings(&mut self) -> Vec<String> {
        (**self).take_warnings()
    }
}

/// Rows held in memory (tests, small results, clipboard data).
pub struct VecSource {
    columns: Vec<ColumnMeta>,
    rows: std::vec::IntoIter<Row>,
    total: u64,
    table: Option<ObjectRef>,
}

impl VecSource {
    /// Source over `rows`.
    pub fn new(columns: Vec<ColumnMeta>, rows: Vec<Row>) -> Self {
        let total = rows.len() as u64;
        VecSource { columns, rows: rows.into_iter(), total, table: None }
    }

    /// Declare the table the rows belong to.
    pub fn with_table(mut self, table: ObjectRef) -> Self {
        self.table = Some(table);
        self
    }
}

#[async_trait]
impl RowSource for VecSource {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        Ok(self.rows.by_ref().take(max.max(1)).collect())
    }
    fn total_rows(&self) -> Option<u64> {
        Some(self.total)
    }
    fn table(&self) -> Option<&ObjectRef> {
        self.table.as_ref()
    }
}

/// Rows streamed from a [`QueryCursor`] (a query or a table `SELECT`).
///
/// The cursor is `'static`, so the source does not borrow the session; the
/// session must not be used for anything else while the source is alive
/// (one open cursor per session).
pub struct CursorSource {
    cursor: Option<Box<dyn QueryCursor>>,
    columns: Vec<ColumnMeta>,
    table: Option<ObjectRef>,
    total: Option<u64>,
    done: bool,
    warnings: Vec<String>,
}

impl CursorSource {
    /// Advance `cursor` to its first row result (skipping update counts).
    pub async fn new(mut cursor: Box<dyn QueryCursor>) -> DbResult<Self> {
        loop {
            match cursor.next_result().await {
                Ok(Some(ResultHeader::Rows(columns))) => {
                    return Ok(CursorSource {
                        cursor: Some(cursor),
                        columns,
                        table: None,
                        total: None,
                        done: false,
                        warnings: Vec::new(),
                    });
                }
                Ok(Some(ResultHeader::Update { .. })) => continue,
                Ok(None) => {
                    let _ = cursor.close().await;
                    return Err(DbError::query("The statement did not return a result set"));
                }
                Err(e) => {
                    let _ = cursor.close().await;
                    return Err(e);
                }
            }
        }
    }

    /// Execute `sql` on `session` and stream its first result set.
    pub async fn execute(session: &mut dyn Session, sql: &str, params: &[Value]) -> DbResult<Self> {
        let cursor = session.execute(sql, params, &ExecOptions::default()).await?;
        Self::new(cursor).await
    }

    /// Declare the table being read (used for file names, sheet names, SQL table names).
    pub fn with_table(mut self, table: Option<ObjectRef>) -> Self {
        self.table = table;
        self
    }

    /// Declare the expected number of rows (e.g. from `COUNT(*)`).
    pub fn with_total_rows(mut self, total: Option<u64>) -> Self {
        self.total = total;
        self
    }
}

#[async_trait]
impl RowSource for CursorSource {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        if self.done {
            return Ok(Vec::new());
        }
        let Some(cursor) = self.cursor.as_mut() else { return Ok(Vec::new()) };
        let rows = cursor.fetch(max.max(1)).await?;
        for m in cursor.take_messages() {
            if m.severity.eq_ignore_ascii_case("warning") {
                self.warnings.push(m.message);
            }
        }
        if rows.is_empty() {
            self.done = true;
        }
        Ok(rows)
    }

    fn total_rows(&self) -> Option<u64> {
        self.total
    }

    fn table(&self) -> Option<&ObjectRef> {
        self.table.as_ref()
    }

    async fn close(&mut self) -> DbResult<()> {
        self.done = true;
        match self.cursor.take() {
            Some(mut c) => c.close().await,
            None => Ok(()),
        }
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

enum Selection {
    /// Every row `next..end` (the rows present when the source was created).
    Range { next: usize, end: usize },
    /// Selected rows in the given order.
    Indices { list: Vec<usize>, pos: usize },
}

/// Rows already fetched into a grid's [`SharedRowStore`] ("use fetched rows"),
/// optionally restricted to selected rows and/or columns.
///
/// The store is locked only while one batch is copied out, so the UI stays responsive.
pub struct RowStoreSource {
    store: SharedRowStore,
    columns: Vec<ColumnMeta>,
    projection: Option<Vec<usize>>,
    selection: Selection,
    table: Option<ObjectRef>,
}

impl RowStoreSource {
    /// All rows currently in `store`; `columns` describes the store's rows.
    pub fn new(store: SharedRowStore, columns: Vec<ColumnMeta>) -> Self {
        let end = store.lock().len();
        RowStoreSource { store, columns, projection: None, selection: Selection::Range { next: 0, end }, table: None }
    }

    /// Only the rows with these indices, in this order (selected rows).
    pub fn with_rows(mut self, indices: Vec<usize>) -> Self {
        self.selection = Selection::Indices { list: indices, pos: 0 };
        self
    }

    /// Only these columns, in this order (selected columns). Invalid indices are ignored.
    pub fn with_columns(mut self, indices: Vec<usize>) -> Self {
        let valid: Vec<usize> = indices.into_iter().filter(|&i| i < self.columns.len()).collect();
        self.columns = valid.iter().map(|&i| self.columns[i].clone()).collect();
        self.projection = Some(valid);
        self
    }

    /// Declare the table the rows belong to.
    pub fn with_table(mut self, table: ObjectRef) -> Self {
        self.table = Some(table);
        self
    }

    fn copy_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let mut store = self.store.lock();
        let mut out = Vec::with_capacity(max.min(4096));
        while out.len() < max {
            let idx = match &mut self.selection {
                Selection::Range { next, end } => {
                    if *next >= *end {
                        break;
                    }
                    *next += 1;
                    *next - 1
                }
                Selection::Indices { list, pos } => {
                    let Some(&i) = list.get(*pos) else { break };
                    *pos += 1;
                    i
                }
            };
            if idx >= store.len() {
                continue;
            }
            let Some(row) = store.get(idx) else {
                return Err(DbError::io(format!("Cannot read cached row {idx} (spill file unavailable)")));
            };
            out.push(match &self.projection {
                Some(cols) => cols.iter().map(|&c| row.get(c).cloned().unwrap_or(Value::Null)).collect(),
                None => row.clone(),
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl RowSource for RowStoreSource {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        self.copy_batch(max.max(1))
    }

    fn total_rows(&self) -> Option<u64> {
        Some(match &self.selection {
            Selection::Range { next, end } => end.saturating_sub(*next) as u64,
            Selection::Indices { list, pos } => list.len().saturating_sub(*pos) as u64,
        })
    }

    fn table(&self) -> Option<&ObjectRef> {
        self.table.as_ref()
    }
}

/// Wraps a source and allows looking at the first rows before consuming
/// them, e.g. to infer column types from a sample before an import.
pub struct PeekableSource<S> {
    inner: S,
    buffer: VecDeque<Row>,
    exhausted: bool,
}

impl<S: RowSource> PeekableSource<S> {
    /// Wrap `inner`.
    pub fn new(inner: S) -> Self {
        PeekableSource { inner, buffer: VecDeque::new(), exhausted: false }
    }

    /// The first `n` rows (fewer when the source is shorter); they are still
    /// returned by later `next_batch` calls.
    pub async fn peek(&mut self, n: usize) -> DbResult<&[Row]> {
        while self.buffer.len() < n && !self.exhausted {
            let batch = self.inner.next_batch(n - self.buffer.len()).await?;
            if batch.is_empty() {
                self.exhausted = true;
            }
            self.buffer.extend(batch);
        }
        let all = self.buffer.make_contiguous();
        Ok(&all[..n.min(all.len())])
    }

    /// The wrapped source.
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

#[async_trait]
impl<S: RowSource> RowSource for PeekableSource<S> {
    fn columns(&self) -> &[ColumnMeta] {
        self.inner.columns()
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let max = max.max(1);
        if !self.buffer.is_empty() {
            let n = max.min(self.buffer.len());
            return Ok(self.buffer.drain(..n).collect());
        }
        if self.exhausted {
            return Ok(Vec::new());
        }
        self.inner.next_batch(max).await
    }

    fn total_rows(&self) -> Option<u64> {
        self.inner.total_rows()
    }

    fn table(&self) -> Option<&ObjectRef> {
        self.inner.table()
    }

    async fn close(&mut self) -> DbResult<()> {
        self.inner.close().await
    }

    fn take_warnings(&mut self) -> Vec<String> {
        self.inner.take_warnings()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::driver::MemoryCursor;
    use dbiji_core::{DataKind, RowStore};

    fn cols() -> Vec<ColumnMeta> {
        vec![ColumnMeta::new("a", "int", DataKind::Int), ColumnMeta::new("b", "text", DataKind::Text)]
    }

    fn rows(n: i64) -> Vec<Row> {
        (0..n).map(|i| vec![Value::Int(i), Value::text(format!("r{i}"))]).collect()
    }

    #[tokio::test]
    async fn vec_source_batches() {
        let mut s = VecSource::new(cols(), rows(5));
        assert_eq!(s.total_rows(), Some(5));
        assert_eq!(s.next_batch(2).await.unwrap().len(), 2);
        assert_eq!(s.next_batch(10).await.unwrap().len(), 3);
        assert!(s.next_batch(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cursor_source_skips_updates() {
        use dbiji_core::driver::MaterializedResult;
        let cursor = MemoryCursor::new(vec![
            MaterializedResult {
                header: ResultHeader::Update { rows_affected: Some(1), last_insert_id: None, tag: None },
                rows: vec![],
            },
            MaterializedResult { header: ResultHeader::Rows(cols()), rows: rows(3) },
        ]);
        let mut s = CursorSource::new(Box::new(cursor)).await.unwrap();
        assert_eq!(s.columns().len(), 2);
        assert_eq!(s.next_batch(2).await.unwrap().len(), 2);
        assert_eq!(s.next_batch(2).await.unwrap().len(), 1);
        assert!(s.next_batch(2).await.unwrap().is_empty());
        s.close().await.unwrap();
        let none = CursorSource::new(Box::new(MemoryCursor::update(Some(1), None))).await;
        assert!(none.is_err());
    }

    #[tokio::test]
    async fn row_store_selection_and_projection() {
        let store = RowStore::shared(1 << 20);
        store.lock().extend(rows(10));
        let mut s = RowStoreSource::new(store.clone(), cols()).with_rows(vec![7, 2, 99, 4]).with_columns(vec![1, 5]);
        assert_eq!(s.columns().len(), 1);
        assert_eq!(s.columns()[0].name, "b");
        let b = s.next_batch(10).await.unwrap();
        assert_eq!(b, vec![vec![Value::text("r7")], vec![Value::text("r2")], vec![Value::text("r4")]]);
        // rows added after creation are not part of a full-store source
        let mut all = RowStoreSource::new(store.clone(), cols());
        store.lock().push(vec![Value::Int(100), Value::Null]);
        let mut n = 0;
        loop {
            let b = all.next_batch(3).await.unwrap();
            if b.is_empty() {
                break;
            }
            n += b.len();
        }
        assert_eq!(n, 10);
    }

    #[tokio::test]
    async fn peekable_replays() {
        let mut p = PeekableSource::new(VecSource::new(cols(), rows(5)));
        assert_eq!(p.peek(3).await.unwrap().len(), 3);
        assert_eq!(p.peek(10).await.unwrap().len(), 5);
        let mut seen = Vec::new();
        loop {
            let b = p.next_batch(2).await.unwrap();
            if b.is_empty() {
                break;
            }
            seen.extend(b);
        }
        assert_eq!(seen, rows(5));
    }
}
