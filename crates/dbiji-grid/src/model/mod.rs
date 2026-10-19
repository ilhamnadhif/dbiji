//! The result set model: columns, fetched rows, client side view, pending
//! edits, virtual columns, dictionaries and fetch state.
//!
//! [`ResultModel`] is pure logic (no egui types). Rows live in a
//! [`SharedRowStore`] that the application's fetch task appends to
//! concurrently; the model locks it briefly. Pending edits are an overlay on
//! top of the stored rows until [`ResultModel::apply_saved`] writes them back.

mod changes;
mod editability;

pub use changes::{CellValue, ChangeSet, ChangedValue, KeyValue, RowDelete, RowInsert, RowUpdate};
pub use editability::{EditTarget, ReadOnlyReason, determine_editability, source_table};

use crate::expr::{EvalError, Expr, ExprError, RowValues};
use crate::view::{JobPoll, RowBitSet, ViewContext, ViewIndex, ViewJob, ViewSpec};
use dbiji_core::{ColumnMeta, DataKind, Dialect, ObjectRef, Row, RowStore, SharedRowStore, Value, tr, trf};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// Progress of fetching the result (FR-D1 incremental fetch).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchState {
    /// A fetch is in progress.
    pub fetching: bool,
    /// The server has more rows than fetched so far.
    pub has_more: bool,
    /// Total row count when known (e.g. after "calculate total count").
    pub total_count: Option<u64>,
    /// A total count query is running.
    pub counting: bool,
    /// Query execution / fetch time.
    pub elapsed: Option<Duration>,
    /// Last fetch error, shown in the status line.
    pub error: Option<String>,
}

/// Value descriptions for a column ("kamus data", FR-D5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dictionary {
    map: HashMap<String, String>,
    entries: Vec<(String, String)>,
}

impl Dictionary {
    /// Build from `key text → description`.
    pub fn new(map: HashMap<String, String>) -> Self {
        let mut entries: Vec<(String, String)> = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by(|a, b| natural_key_cmp(&a.0, &b.0));
        Dictionary { map, entries }
    }

    /// Description of `key`.
    pub fn describe(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// Entries sorted by key.
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Sort numeric keys numerically, others lexicographically.
fn natural_key_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// A foreign key from columns of this result to another table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyLink {
    /// Constraint name.
    pub name: Option<String>,
    /// Result column indices (in constraint order).
    pub columns: Vec<usize>,
    /// Referenced table.
    pub ref_table: ObjectRef,
    /// Referenced column names (same order as `columns`).
    pub ref_columns: Vec<String>,
}

/// A computed, read-only column appended after the result columns.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualColumn {
    /// Header label.
    pub name: String,
    /// Compiled expression (references result columns only).
    pub expression: Expr,
}

/// Where a display row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowSource {
    /// A fetched row (store index).
    Stored(u32),
    /// A pending inserted row (index into the inserted rows, in display order).
    Inserted(usize),
}

/// Stable identity of a row, independent of its display position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowKey {
    /// A fetched row (store index).
    Stored(u32),
    /// A pending inserted row (id).
    Inserted(u64),
}

/// Pending state of a row, used for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowState {
    /// No pending change.
    Unchanged,
    /// Some cells are modified.
    Modified,
    /// New row, not yet saved.
    Inserted,
    /// Marked for deletion.
    Deleted,
}

/// Counts of pending changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingCounts {
    /// Rows with modified cells.
    pub updated: usize,
    /// New rows.
    pub inserted: usize,
    /// Rows marked for deletion.
    pub deleted: usize,
}

impl PendingCounts {
    /// Total number of changed rows.
    pub fn total(&self) -> usize {
        self.updated + self.inserted + self.deleted
    }
}

/// A cell value as seen through the pending edit overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CellRef<'a> {
    /// A value (stored or edited).
    Value(&'a Value),
    /// Column default of a new row.
    Default,
    /// The column does not exist in this row.
    Missing,
}

impl<'a> CellRef<'a> {
    /// The value, NULL for defaults and missing cells.
    pub fn value(&self) -> &'a Value {
        static NULL: Value = Value::Null;
        match self {
            CellRef::Value(v) => v,
            _ => &NULL,
        }
    }
}

#[derive(Debug, Clone)]
struct InsertedRow {
    id: u64,
    /// Store row the new row was inserted after (`None` = top).
    anchor: Option<u32>,
    /// Number of base rows displayed before this row.
    after_base: usize,
    values: Vec<CellValue>,
}

#[derive(Debug, Clone)]
enum Undo {
    /// Previous overlay values (`None` = no overlay) of stored or inserted rows.
    Cells(Vec<(RowKey, usize, Option<CellValue>)>),
    /// Inserted row ids.
    Insert(Vec<u64>),
    /// Rows marked deleted and inserted rows removed (index, row).
    Delete { stored: Vec<u32>, inserted: Vec<(usize, InsertedRow)> },
    /// Several entries undone together (last first).
    Group(Vec<Undo>),
}

#[derive(Debug, Clone, PartialEq)]
enum ViewKind {
    /// Store order (the view may be materialized to hide saved deletions).
    Identity,
    /// Computed by a client sort/filter.
    Client(ViewSpec),
    /// Supplied through [`ResultModel::set_view`].
    External,
}

/// The data behind a result grid.
pub struct ResultModel {
    columns: Vec<ColumnMeta>,
    store: SharedRowStore,
    store_len: usize,
    view: Option<Vec<u32>>,
    view_kind: ViewKind,
    view_source_len: usize,
    view_job: Option<ViewJob>,
    removed: Arc<RowBitSet>,
    updates: HashMap<u32, Vec<(usize, CellValue)>>,
    deleted: HashSet<u32>,
    inserted: Vec<InsertedRow>,
    next_insert_id: u64,
    undo: Vec<Undo>,
    virtual_columns: Vec<VirtualColumn>,
    editability: Result<EditTarget, ReadOnlyReason>,
    dictionaries: HashMap<usize, Dictionary>,
    enum_values: HashMap<usize, Vec<String>>,
    foreign_keys: Vec<ForeignKeyLink>,
    generation: u64,
    /// Fetch progress, updated by the application.
    pub fetch: FetchState,
}

impl std::fmt::Debug for ResultModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultModel").field("columns", &self.columns.len()).field("rows", &self.row_count()).field("pending", &self.pending_counts()).finish()
    }
}

const MAX_UNDO: usize = 200;

impl ResultModel {
    /// Model over `store`. The result is read-only until
    /// [`ResultModel::set_editability`] is called with an [`EditTarget`].
    pub fn new(columns: Vec<ColumnMeta>, store: SharedRowStore) -> Self {
        let store_len = store.lock().len();
        ResultModel {
            columns,
            store,
            store_len,
            view: None,
            view_kind: ViewKind::Identity,
            view_source_len: store_len,
            view_job: None,
            removed: Arc::new(RowBitSet::new()),
            updates: HashMap::new(),
            deleted: HashSet::new(),
            inserted: Vec::new(),
            next_insert_id: 1,
            undo: Vec::new(),
            virtual_columns: Vec::new(),
            editability: Err(ReadOnlyReason::NoSourceTable),
            dictionaries: HashMap::new(),
            enum_values: HashMap::new(),
            foreign_keys: Vec::new(),
            generation: 1,
            fetch: FetchState::default(),
        }
    }

    /// Convenience: model over an in-memory store filled with `rows`.
    pub fn from_rows(columns: Vec<ColumnMeta>, rows: Vec<Row>) -> Self {
        let store = RowStore::shared(dbiji_core::rowstore::DEFAULT_STORE_BUDGET);
        store.lock().extend(rows);
        Self::new(columns, store)
    }

    /// Replace columns and rows (e.g. after re-running the query). Pending
    /// edits, the client view and virtual columns referencing missing columns are dropped.
    pub fn reset(&mut self, columns: Vec<ColumnMeta>, store: SharedRowStore) {
        let virtuals = std::mem::take(&mut self.virtual_columns);
        let dictionaries = std::mem::take(&mut self.dictionaries);
        let fetch = std::mem::take(&mut self.fetch);
        let editability = std::mem::replace(&mut self.editability, Err(ReadOnlyReason::NoSourceTable));
        let same_columns = columns.len() == self.columns.len() && columns.iter().zip(&self.columns).all(|(a, b)| a.name == b.name);
        *self = ResultModel::new(columns, store);
        self.fetch = fetch;
        if same_columns {
            self.dictionaries = dictionaries;
            self.editability = editability;
            for v in virtuals {
                let _ = self.add_virtual_column(&v.name, v.expression.source());
            }
        }
    }

    // ------------------------------------------------------------------ columns

    /// Result columns (without virtual columns).
    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    /// Number of result columns plus virtual columns.
    pub fn column_count(&self) -> usize {
        self.columns.len() + self.virtual_columns.len()
    }

    /// Number of result columns.
    pub fn base_column_count(&self) -> usize {
        self.columns.len()
    }

    /// True for virtual (computed) columns.
    pub fn is_virtual_column(&self, col: usize) -> bool {
        col >= self.columns.len() && col < self.column_count()
    }

    /// Header label of a column.
    pub fn column_name(&self, col: usize) -> &str {
        if col < self.columns.len() { &self.columns[col].name } else { self.virtual_columns.get(col - self.columns.len()).map_or("", |v| v.name.as_str()) }
    }

    /// All column names (result columns, then virtual columns).
    pub fn column_names(&self) -> Vec<String> {
        (0..self.column_count()).map(|c| self.column_name(c).to_string()).collect()
    }

    /// Metadata of a result column (`None` for virtual columns).
    pub fn column_meta(&self, col: usize) -> Option<&ColumnMeta> {
        self.columns.get(col)
    }

    /// Logical kind of a column (`Other` for virtual columns).
    pub fn column_kind(&self, col: usize) -> DataKind {
        self.columns.get(col).map_or(DataKind::Other, |c| c.kind)
    }

    /// Virtual columns.
    pub fn virtual_columns(&self) -> &[VirtualColumn] {
        &self.virtual_columns
    }

    /// Append a virtual column computed by `expression` (see [`crate::expr`]).
    /// Returns its model column index.
    pub fn add_virtual_column(&mut self, name: &str, expression: &str) -> Result<usize, ExprError> {
        let names: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
        let expr = Expr::compile(expression, &names)?;
        let name = if name.trim().is_empty() { expression.to_string() } else { name.trim().to_string() };
        self.virtual_columns.push(VirtualColumn { name, expression: expr });
        self.generation += 1;
        Ok(self.column_count() - 1)
    }

    /// Change name and expression of virtual column `index` (0-based among virtual columns).
    pub fn update_virtual_column(&mut self, index: usize, name: &str, expression: &str) -> Result<(), ExprError> {
        let names: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
        let expr = Expr::compile(expression, &names)?;
        if let Some(v) = self.virtual_columns.get_mut(index) {
            v.name = if name.trim().is_empty() { expression.to_string() } else { name.trim().to_string() };
            v.expression = expr;
            self.generation += 1;
        }
        Ok(())
    }

    /// Remove virtual column `index` (0-based among virtual columns). A client
    /// view sorting/filtering by virtual columns is reset.
    pub fn remove_virtual_column(&mut self, index: usize) {
        if index < self.virtual_columns.len() {
            self.virtual_columns.remove(index);
            if let ViewKind::Client(spec) = &self.view_kind {
                let base = self.columns.len();
                let uses_virtual = spec.sort.iter().any(|k| k.column >= base) || spec.filter.as_ref().is_some_and(|f| f.expr().referenced_columns().iter().any(|c| *c >= base));
                if uses_virtual {
                    self.clear_view();
                }
            }
            self.generation += 1;
        }
    }

    /// Value of a virtual column for a row (with pending edits applied).
    pub fn virtual_value(&self, row: usize, col: usize) -> Option<Result<Value, EvalError>> {
        let v = self.virtual_columns.get(col.checked_sub(self.columns.len())?)?;
        let mut out = None;
        self.visit_rows(row..row + 1, |_, cells| out = Some(v.expression.eval(&cells)));
        out
    }

    // --------------------------------------------------------------- dictionary

    /// Set value descriptions for column `col` (FR-D5 "kamus data").
    pub fn set_dictionary(&mut self, col: usize, entries: HashMap<String, String>) {
        if entries.is_empty() {
            self.dictionaries.remove(&col);
        } else {
            self.dictionaries.insert(col, Dictionary::new(entries));
        }
        self.generation += 1;
    }

    /// Dictionary of column `col`.
    pub fn dictionary(&self, col: usize) -> Option<&Dictionary> {
        self.dictionaries.get(&col)
    }

    /// Description of `value` in the dictionary of `col`.
    pub fn describe_value(&self, col: usize, value: &Value) -> Option<&str> {
        if value.is_null() {
            return None;
        }
        self.dictionaries.get(&col)?.describe(&value.to_text())
    }

    /// Allowed values of an enum column (offered as a dropdown when editing).
    pub fn set_enum_values(&mut self, col: usize, values: Vec<String>) {
        if values.is_empty() {
            self.enum_values.remove(&col);
        } else {
            self.enum_values.insert(col, values);
        }
        self.generation += 1;
    }

    /// Allowed values of an enum column.
    pub fn enum_values(&self, col: usize) -> Option<&[String]> {
        self.enum_values.get(&col).map(Vec::as_slice)
    }

    /// Foreign keys from result columns to other tables (cells are drawn as links).
    pub fn set_foreign_keys(&mut self, fks: Vec<ForeignKeyLink>) {
        self.foreign_keys = fks;
        self.generation += 1;
    }

    /// Foreign keys of the result.
    pub fn foreign_keys(&self) -> &[ForeignKeyLink] {
        &self.foreign_keys
    }

    /// Index of the first foreign key containing column `col`.
    pub fn foreign_key_for_column(&self, col: usize) -> Option<usize> {
        self.foreign_keys.iter().position(|fk| fk.columns.contains(&col))
    }

    // ------------------------------------------------------------------- rows

    /// The shared row store.
    pub fn store(&self) -> &SharedRowStore {
        &self.store
    }

    /// Change counter; increases whenever displayed data may have changed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Pick up rows appended by the fetch task. Call once per frame (the grid
    /// does). Returns true when the row count changed.
    pub fn sync(&mut self) -> bool {
        let len = self.store.lock().len();
        self.sync_to(len)
    }

    /// Like [`ResultModel::sync`] but never waits: when the fetch task holds
    /// the store lock the row count is picked up on a later call.
    pub fn try_sync(&mut self) -> bool {
        let Some(guard) = self.store.try_lock() else { return false };
        let len = guard.len();
        drop(guard);
        self.sync_to(len)
    }

    fn sync_to(&mut self, len: usize) -> bool {
        if len == self.store_len {
            return false;
        }
        if len < self.store_len {
            // The store was cleared: row indices are no longer valid.
            self.store_len = len;
            self.view = None;
            self.view_kind = ViewKind::Identity;
            self.view_source_len = len;
            self.removed = Arc::new(RowBitSet::new());
            self.updates.clear();
            self.deleted.clear();
            self.inserted.clear();
            self.undo.clear();
            self.generation += 1;
            return true;
        }
        let old = self.store_len;
        self.store_len = len;
        if self.view_kind == ViewKind::Identity {
            if let Some(view) = &mut self.view {
                let removed = &self.removed;
                view.extend((old..len).map(|r| r as u32).filter(|r| !removed.contains(*r)));
            }
            self.view_source_len = len;
        }
        self.generation += 1;
        true
    }

    /// Number of fetched rows in the store (including rows hidden by a filter).
    pub fn stored_row_count(&self) -> usize {
        self.store_len
    }

    fn base_len(&self) -> usize {
        self.view.as_ref().map_or(self.store_len, Vec::len)
    }

    /// Number of displayed rows (view rows plus pending inserted rows).
    pub fn row_count(&self) -> usize {
        self.base_len() + self.inserted.len()
    }

    fn base_to_store(&self, b: usize) -> Option<u32> {
        match &self.view {
            Some(v) => v.get(b).copied(),
            None => (b < self.store_len).then_some(b as u32),
        }
    }

    /// Resolve a display row.
    pub fn row_source(&self, row: usize) -> Option<RowSource> {
        let base_len = self.base_len();
        let n = self.inserted.len();
        if row >= base_len + n {
            return None;
        }
        let display = |k: usize| self.inserted[k].after_base.min(base_len) + k;
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if display(mid) < row {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n && display(lo) == row {
            return Some(RowSource::Inserted(lo));
        }
        self.base_to_store(row - lo).map(RowSource::Stored)
    }

    /// Display row of a pending inserted row (by index).
    fn inserted_display(&self, k: usize) -> usize {
        self.inserted[k].after_base.min(self.base_len()) + k
    }

    /// Stable identity of a display row.
    pub fn row_key(&self, row: usize) -> Option<RowKey> {
        Some(match self.row_source(row)? {
            RowSource::Stored(s) => RowKey::Stored(s),
            RowSource::Inserted(k) => RowKey::Inserted(self.inserted[k].id),
        })
    }

    /// Current display row of a row identity (linear in the view size for stored rows).
    pub fn display_row_of(&self, key: RowKey) -> Option<usize> {
        match key {
            RowKey::Inserted(id) => {
                let k = self.inserted.iter().position(|r| r.id == id)?;
                Some(self.inserted_display(k))
            }
            RowKey::Stored(s) => {
                let b = match &self.view {
                    None => (s as usize) < self.store_len,
                    Some(_) => false,
                };
                let base = if b { s as usize } else { self.view.as_ref()?.iter().position(|&r| r == s)? };
                // Count inserted rows displayed before this base row.
                let before = self.inserted.iter().filter(|r| r.after_base.min(self.base_len()) <= base).count();
                Some(base + before)
            }
        }
    }

    /// Pending state of a display row.
    pub fn row_state(&self, row: usize) -> RowState {
        match self.row_source(row) {
            Some(RowSource::Inserted(_)) => RowState::Inserted,
            Some(RowSource::Stored(s)) => {
                if self.deleted.contains(&s) {
                    RowState::Deleted
                } else if self.updates.contains_key(&s) {
                    RowState::Modified
                } else {
                    RowState::Unchanged
                }
            }
            None => RowState::Unchanged,
        }
    }

    /// True when the cell has a pending modification.
    pub fn is_cell_modified(&self, row: usize, col: usize) -> bool {
        match self.row_source(row) {
            Some(RowSource::Stored(s)) => self.updates.get(&s).is_some_and(|e| e.iter().any(|(c, _)| *c == col)),
            Some(RowSource::Inserted(k)) => self.inserted[k].values.get(col).is_some_and(|v| *v != CellValue::Default),
            None => false,
        }
    }

    /// Visit display rows in `rows` with the store locked once. Rows that
    /// cannot be loaded (spill read error) are skipped.
    pub fn visit_rows(&self, rows: Range<usize>, f: impl FnMut(usize, RowCells<'_>)) {
        let end = rows.end.min(self.row_count());
        if rows.start >= end {
            return;
        }
        let mut guard = self.store.lock();
        self.visit_locked(&mut guard, rows.start..end, f);
    }

    /// Like [`ResultModel::visit_rows`] but waits at most `timeout` for the
    /// store lock (the fetch task may hold it while spilling pages to disk).
    /// Returns false, without visiting anything, when the lock was not acquired.
    pub fn visit_rows_for(&self, rows: Range<usize>, timeout: Duration, f: impl FnMut(usize, RowCells<'_>)) -> bool {
        let end = rows.end.min(self.row_count());
        if rows.start >= end {
            return true;
        }
        let Some(mut guard) = self.store.try_lock_for(timeout) else { return false };
        self.visit_locked(&mut guard, rows.start..end, f);
        true
    }

    /// Cell value without waiting: `None` when the store is busy.
    pub fn try_cell(&self, row: usize, col: usize, timeout: Duration) -> Option<Option<Value>> {
        let mut out = None;
        self.visit_rows_for(row..row + 1, timeout, |_, cells| out = cells.value_of(col)).then_some(out)
    }

    fn visit_locked(&self, guard: &mut RowStore, rows: Range<usize>, mut f: impl FnMut(usize, RowCells<'_>)) {
        for d in rows {
            match self.row_source(d) {
                Some(RowSource::Stored(s)) => {
                    let Some(base) = guard.get(s as usize) else { continue };
                    let cells = RowCells {
                        base: Some(base),
                        edits: self.updates.get(&s).map(Vec::as_slice),
                        inserted: None,
                        state: if self.deleted.contains(&s) {
                            RowState::Deleted
                        } else if self.updates.contains_key(&s) {
                            RowState::Modified
                        } else {
                            RowState::Unchanged
                        },
                        source: RowSource::Stored(s),
                        model: self,
                    };
                    f(d, cells);
                }
                Some(RowSource::Inserted(k)) => {
                    let cells =
                        RowCells { base: None, edits: None, inserted: Some(&self.inserted[k].values), state: RowState::Inserted, source: RowSource::Inserted(k), model: self };
                    f(d, cells);
                }
                None => {}
            }
        }
    }

    /// Value of a cell with pending edits applied (`Default` reads as NULL).
    /// Virtual columns are evaluated (errors read as NULL).
    pub fn cell(&self, row: usize, col: usize) -> Option<Value> {
        let mut out = None;
        self.visit_rows(row..row + 1, |_, cells| out = cells.value_of(col));
        out
    }

    /// Values of all columns (result and virtual) of a row.
    pub fn row_values(&self, row: usize) -> Option<Vec<Value>> {
        let mut out = None;
        let n = self.column_count();
        self.visit_rows(row..row + 1, |_, cells| out = Some((0..n).map(|c| cells.value_of(c).unwrap_or(Value::Null)).collect()));
        out
    }

    /// Values of `cols` for each of `rows` (display indices), locking the store once.
    pub fn values(&self, rows: &[usize], cols: &[usize]) -> Vec<Vec<Value>> {
        let mut out = Vec::with_capacity(rows.len());
        let mut by_row: HashMap<usize, Vec<Value>> = HashMap::with_capacity(rows.len());
        // Visit in sorted order to keep spill page access sequential.
        let mut sorted = rows.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for chunk in sorted.chunk_by(|a, b| b == &(a + 1)) {
            let (first, last) = (chunk[0], chunk[chunk.len() - 1]);
            self.visit_rows(first..last + 1, |d, cells| {
                by_row.insert(d, cols.iter().map(|&c| cells.value_of(c).unwrap_or(Value::Null)).collect());
            });
        }
        for r in rows {
            out.push(by_row.get(r).cloned().unwrap_or_else(|| vec![Value::Null; cols.len()]));
        }
        out
    }

    // ------------------------------------------------------------- client view

    /// The current view (display position → store row), if any.
    pub fn view(&self) -> Option<&[u32]> {
        self.view.as_deref()
    }

    /// Install an externally computed view (`None` restores store order).
    /// Rows deleted and saved earlier are removed from it.
    pub fn set_view(&mut self, view: Option<Vec<u32>>) {
        self.cancel_view_job();
        match view {
            None => self.clear_view(),
            Some(mut v) => {
                let len = self.store_len as u32;
                let removed = &self.removed;
                v.retain(|r| *r < len && !removed.contains(*r));
                self.view = Some(v);
                self.view_kind = ViewKind::External;
                self.view_source_len = self.store_len;
                self.reanchor_inserted();
                self.generation += 1;
            }
        }
    }

    /// Install a view computed for `spec` (from [`crate::view::compute_view`]).
    pub fn set_view_index(&mut self, index: ViewIndex, spec: ViewSpec) {
        if spec.is_identity() {
            self.clear_view();
            return;
        }
        let len = self.store_len as u32;
        let mut rows = index.rows;
        rows.retain(|r| *r < len);
        self.view = Some(rows);
        self.view_kind = ViewKind::Client(spec);
        self.view_source_len = index.source_len.min(self.store_len);
        self.reanchor_inserted();
        self.generation += 1;
    }

    /// Remove client sorting/filtering (saved deletions stay hidden).
    pub fn clear_view(&mut self) {
        self.cancel_view_job();
        if self.removed.is_empty() {
            self.view = None;
        } else {
            let removed = &self.removed;
            self.view = Some((0..self.store_len as u32).filter(|r| !removed.contains(*r)).collect());
        }
        self.view_kind = ViewKind::Identity;
        self.view_source_len = self.store_len;
        self.reanchor_inserted();
        self.generation += 1;
    }

    /// Spec of the active client view (identity when none).
    pub fn client_view_spec(&self) -> ViewSpec {
        match &self.view_kind {
            ViewKind::Client(s) => s.clone(),
            _ => ViewSpec::default(),
        }
    }

    /// True when a client sort or filter is active.
    pub fn has_client_view(&self) -> bool {
        matches!(self.view_kind, ViewKind::Client(_) | ViewKind::External)
    }

    /// Rows were fetched after the client view was computed (they are not shown).
    pub fn is_view_stale(&self) -> bool {
        !matches!(self.view_kind, ViewKind::Identity) && self.view_source_len < self.store_len
    }

    /// Number of fetched rows not reflected in a stale view.
    pub fn rows_outside_view(&self) -> usize {
        if self.is_view_stale() { self.store_len - self.view_source_len } else { 0 }
    }

    /// Context for evaluating model columns on another thread.
    pub fn view_context(&self) -> ViewContext {
        ViewContext { base_columns: self.columns.len(), virtual_columns: self.virtual_columns.iter().map(|v| v.expression.clone()).collect(), removed: self.removed.clone() }
    }

    /// Start computing a client view on a background thread; `on_done` is
    /// called from the worker when it finishes (e.g. to request a repaint).
    /// An identity spec clears the view immediately.
    pub fn start_client_view(&mut self, spec: ViewSpec, on_done: Box<dyn Fn() + Send>) {
        self.cancel_view_job();
        if spec.is_identity() {
            self.clear_view();
            return;
        }
        let ctx = self.view_context();
        self.view_job = Some(ViewJob::spawn(self.store.clone(), spec, ctx, on_done));
    }

    /// Spec of the view being computed in the background, if any.
    pub fn pending_view_spec(&self) -> Option<&ViewSpec> {
        self.view_job.as_ref().map(ViewJob::spec)
    }

    /// True while a background view job runs.
    pub fn is_view_job_running(&self) -> bool {
        self.view_job.is_some()
    }

    /// Install the background result when ready. Returns `Some(Ok(()))` when a
    /// view was installed, `Some(Err(msg))` when the job failed.
    pub fn poll_view_job(&mut self) -> Option<Result<(), String>> {
        let job = self.view_job.as_ref()?;
        match job.poll() {
            JobPoll::Running => None,
            JobPoll::Done(index) => {
                let spec = job.spec().clone();
                self.view_job = None;
                self.set_view_index(index, spec);
                Some(Ok(()))
            }
            JobPoll::Failed(msg) => {
                self.view_job = None;
                Some(Err(msg))
            }
        }
    }

    fn cancel_view_job(&mut self) {
        if let Some(job) = self.view_job.take() {
            job.cancel();
        }
    }

    /// Recompute display positions of inserted rows after the base changed.
    fn reanchor_inserted(&mut self) {
        if self.inserted.is_empty() {
            return;
        }
        let base_len = self.base_len();
        let positions: HashMap<u32, usize> = match &self.view {
            None => HashMap::new(),
            Some(v) => {
                let anchors: HashSet<u32> = self.inserted.iter().filter_map(|r| r.anchor).collect();
                v.iter().enumerate().filter(|(_, r)| anchors.contains(r)).map(|(i, r)| (*r, i)).collect()
            }
        };
        let identity = self.view.is_none();
        let store_len = self.store_len;
        for r in &mut self.inserted {
            r.after_base = match r.anchor {
                None => 0,
                Some(a) if identity => {
                    if (a as usize) < store_len {
                        a as usize + 1
                    } else {
                        base_len
                    }
                }
                Some(a) => positions.get(&a).map_or(base_len, |p| p + 1),
            };
        }
        self.inserted.sort_by_key(|r| r.after_base);
    }

    // ---------------------------------------------------------------- editing

    /// Editability of the result.
    pub fn editability(&self) -> &Result<EditTarget, ReadOnlyReason> {
        &self.editability
    }

    /// Set editability (usually from [`determine_editability`]).
    pub fn set_editability(&mut self, editability: Result<EditTarget, ReadOnlyReason>) {
        self.editability = editability;
        self.generation += 1;
    }

    /// True when the result can be edited.
    pub fn is_editable(&self) -> bool {
        self.editability.is_ok()
    }

    /// Why the result is read-only.
    pub fn read_only_reason(&self) -> Option<&ReadOnlyReason> {
        self.editability.as_ref().err()
    }

    fn target(&self) -> Result<&EditTarget, String> {
        self.editability.as_ref().map_err(ReadOnlyReason::message)
    }

    /// Check whether a cell can be edited; returns the reason when not.
    pub fn can_edit_cell(&self, row: usize, col: usize) -> Result<(), String> {
        self.check_cell(row, col).map(|_| ())
    }

    fn check_cell(&self, row: usize, col: usize) -> Result<RowSource, String> {
        let target = self.target()?;
        if self.is_virtual_column(col) {
            return Err(tr("Virtual columns are computed and cannot be edited").to_string());
        }
        if col >= self.columns.len() {
            return Err(tr("No such column").to_string());
        }
        if !target.is_column_editable(col) {
            return Err(trf("Column '{}' is computed and cannot be edited", &[&self.columns[col].name]));
        }
        let src = self.row_source(row).ok_or_else(|| tr("No such row").to_string())?;
        if let RowSource::Stored(s) = src
            && self.deleted.contains(&s)
        {
            return Err(tr("The row is marked for deletion").to_string());
        }
        Ok(src)
    }

    /// Validate `value` for column `col` (NULL constraint, maximum length).
    fn validate(&self, col: usize, value: &CellValue, inserted: bool) -> Result<(), String> {
        let meta = &self.columns[col];
        match value {
            // The database knows the default; nothing to check.
            CellValue::Default => Ok(()),
            CellValue::Value(v) => {
                if v.is_null() && meta.nullable == Some(false) && !(inserted && meta.auto_increment == Some(true)) {
                    return Err(trf("Column '{}' does not allow NULL", &[&meta.name]));
                }
                if let (Some(max), Value::Text(s)) = (meta.max_length, v) {
                    let n = s.chars().count();
                    if max > 0 && n > max as usize && meta.kind == DataKind::Text {
                        return Err(trf("Value for '{}' is {} characters long; the column allows at most {}", &[&meta.name, &n, &max]));
                    }
                }
                Ok(())
            }
        }
    }

    /// Parse `text` according to the column type and set it (FR-D2).
    /// Empty input becomes NULL for non-text columns.
    pub fn set_cell(&mut self, row: usize, col: usize, text: &str) -> Result<(), String> {
        self.check_cell(row, col)?;
        let value = self.parse_for_column(col, text)?;
        self.set_cells(vec![(row, col, CellValue::Value(value))])
    }

    /// Parse user input for column `col` without changing anything.
    pub fn parse_for_column(&self, col: usize, text: &str) -> Result<Value, String> {
        let meta = self.columns.get(col).ok_or_else(|| tr("No such column").to_string())?;
        let kind = match meta.kind {
            // Keep arrays, intervals, geometry ... as text for the database to parse.
            DataKind::Array
            | DataKind::Interval
            | DataKind::Geometry
            | DataKind::Network
            | DataKind::Range
            | DataKind::Bit
            | DataKind::TimeTz
            | DataKind::Other
            | DataKind::Xml
            | DataKind::Enum => DataKind::Text,
            k => k,
        };
        Value::parse_as(text, kind).map_err(|e| trf("Invalid value for '{}': {}", &[&meta.name, &e]))
    }

    /// Set a cell to a value.
    pub fn set_value(&mut self, row: usize, col: usize, value: Value) -> Result<(), String> {
        self.set_cells(vec![(row, col, CellValue::Value(value))])
    }

    /// Set a cell to NULL.
    pub fn set_null(&mut self, row: usize, col: usize) -> Result<(), String> {
        self.set_value(row, col, Value::Null)
    }

    /// Set a cell to the column default (`DEFAULT`).
    pub fn set_default(&mut self, row: usize, col: usize) -> Result<(), String> {
        self.set_cells(vec![(row, col, CellValue::Default)])
    }

    /// Apply several cell changes as one undoable step. Validation is
    /// all-or-nothing: on error nothing is changed.
    pub fn set_cells(&mut self, edits: Vec<(usize, usize, CellValue)>) -> Result<(), String> {
        let undo = self.set_cells_inner(edits)?;
        if !undo.is_empty() {
            self.push_undo(Undo::Cells(undo));
        }
        Ok(())
    }

    fn set_cells_inner(&mut self, edits: Vec<(usize, usize, CellValue)>) -> Result<Vec<(RowKey, usize, Option<CellValue>)>, String> {
        let mut resolved = Vec::with_capacity(edits.len());
        for (row, col, value) in edits {
            let src = self.check_cell(row, col)?;
            self.validate(col, &value, matches!(src, RowSource::Inserted(_))).map_err(|e| trf("Row {}: {}", &[&(row + 1), &e]))?;
            resolved.push((src, col, value));
        }
        // Originals of stored rows, read with one lock.
        let mut originals: HashMap<(u32, usize), Value> = HashMap::new();
        {
            let mut guard = self.store.lock();
            for (src, col, _) in &resolved {
                if let RowSource::Stored(s) = src {
                    let v = guard.cell(*s as usize, *col).cloned().unwrap_or(Value::Null);
                    originals.insert((*s, *col), v);
                }
            }
        }
        let mut undo = Vec::with_capacity(resolved.len());
        for (src, col, value) in resolved {
            match src {
                RowSource::Stored(s) => {
                    let original = originals.remove(&(s, col)).unwrap_or(Value::Null);
                    let entry = self.updates.entry(s).or_default();
                    let pos = entry.iter().position(|(c, _)| *c == col);
                    let prev = pos.map(|p| entry[p].1.clone());
                    let same = matches!(&value, CellValue::Value(v) if *v == original);
                    match (pos, same) {
                        (Some(p), true) => {
                            entry.remove(p);
                        }
                        (Some(p), false) => entry[p].1 = value,
                        (None, true) => {}
                        (None, false) => entry.push((col, value)),
                    }
                    if entry.is_empty() {
                        self.updates.remove(&s);
                    }
                    undo.push((RowKey::Stored(s), col, prev));
                }
                RowSource::Inserted(k) => {
                    let row = &mut self.inserted[k];
                    if row.values.len() <= col {
                        row.values.resize(col + 1, CellValue::Default);
                    }
                    let prev = std::mem::replace(&mut row.values[col], value);
                    undo.push((RowKey::Inserted(row.id), col, Some(prev)));
                }
            }
        }
        self.generation += 1;
        Ok(undo)
    }

    /// Insert a new row after display row `after` (`None` appends at the end).
    /// Returns the display index of the new row.
    pub fn add_row(&mut self, after: Option<usize>) -> Result<usize, String> {
        self.target()?;
        let (row, undo) = self.insert_row_inner(after, vec![CellValue::Default; self.columns.len()]);
        self.push_undo(Undo::Insert(vec![undo]));
        self.generation += 1;
        Ok(row)
    }

    /// Insert a new row before display row `row`. Returns its display index.
    pub fn add_row_before(&mut self, row: usize) -> Result<usize, String> {
        if row > 0 {
            return self.add_row(Some(row - 1));
        }
        self.target()?;
        let id = self.next_insert_id;
        self.next_insert_id += 1;
        let anchor = self.base_to_store(0);
        self.inserted.insert(0, InsertedRow { id, anchor, after_base: 0, values: vec![CellValue::Default; self.columns.len()] });
        self.push_undo(Undo::Insert(vec![id]));
        self.generation += 1;
        Ok(self.inserted_display(0))
    }

    fn insert_row_inner(&mut self, after: Option<usize>, values: Vec<CellValue>) -> (usize, u64) {
        let id = self.next_insert_id;
        self.next_insert_id += 1;
        let base_len = self.base_len();
        let (index, after_base, anchor) = match after.and_then(|a| self.row_source(a)) {
            Some(RowSource::Inserted(k)) => (k + 1, self.inserted[k].after_base, self.inserted[k].anchor),
            Some(RowSource::Stored(s)) => {
                let d = after.unwrap_or(0);
                // Base position of display row d.
                let before = self.inserted.iter().enumerate().filter(|(k, r)| r.after_base.min(base_len) + k < d).count();
                let b = d - before;
                let idx = self.inserted.partition_point(|r| r.after_base.min(base_len) <= b);
                (idx, b + 1, Some(s))
            }
            None => {
                let anchor = base_len.checked_sub(1).and_then(|b| self.base_to_store(b));
                (self.inserted.len(), base_len, anchor)
            }
        };
        self.inserted.insert(index, InsertedRow { id, anchor, after_base, values });
        (self.inserted_display(index), id)
    }

    /// Insert copies of `rows` (display indices) after the last of them.
    /// Auto-increment columns are left at their default. Returns the new display rows.
    pub fn duplicate_rows(&mut self, rows: &[usize]) -> Result<Vec<usize>, String> {
        self.target()?;
        let mut sorted: Vec<usize> = rows.iter().copied().filter(|&r| r < self.row_count()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.is_empty() {
            return Ok(Vec::new());
        }
        let n = self.columns.len();
        let mut copies: Vec<Vec<CellValue>> = Vec::with_capacity(sorted.len());
        let auto: Vec<bool> = self.columns.iter().map(|c| c.auto_increment == Some(true)).collect();
        for &r in &sorted {
            let mut vals = Vec::with_capacity(n);
            self.visit_rows(r..r + 1, |_, cells| {
                for (c, is_auto) in auto.iter().enumerate() {
                    vals.push(if *is_auto {
                        CellValue::Default
                    } else {
                        match cells.get(c) {
                            CellRef::Value(v) => CellValue::Value(v.clone()),
                            _ => CellValue::Default,
                        }
                    });
                }
            });
            if vals.len() == n {
                copies.push(vals);
            }
        }
        let mut after = sorted.last().copied();
        let mut ids = Vec::with_capacity(copies.len());
        let mut out = Vec::with_capacity(copies.len());
        for vals in copies {
            let (row, id) = self.insert_row_inner(after, vals);
            after = Some(row);
            ids.push(id);
        }
        for id in &ids {
            if let Some(k) = self.inserted.iter().position(|r| r.id == *id) {
                out.push(self.inserted_display(k));
            }
        }
        self.push_undo(Undo::Insert(ids));
        self.generation += 1;
        Ok(out)
    }

    /// Mark rows for deletion (new rows are removed immediately).
    /// Returns the number of affected rows.
    pub fn delete_rows(&mut self, rows: &[usize]) -> Result<usize, String> {
        self.target()?;
        let mut stored = Vec::new();
        let mut inserted_idx = Vec::new();
        for &r in rows {
            match self.row_source(r) {
                Some(RowSource::Stored(s)) => {
                    if !self.deleted.contains(&s) && !stored.contains(&s) {
                        stored.push(s);
                    }
                }
                Some(RowSource::Inserted(k)) => inserted_idx.push(k),
                None => {}
            }
        }
        inserted_idx.sort_unstable();
        inserted_idx.dedup();
        let count = stored.len() + inserted_idx.len();
        if count == 0 {
            return Ok(0);
        }
        for s in &stored {
            self.deleted.insert(*s);
        }
        let mut removed = Vec::with_capacity(inserted_idx.len());
        for &k in inserted_idx.iter().rev() {
            let row = self.inserted.remove(k);
            removed.push((k, row));
        }
        self.push_undo(Undo::Delete { stored, inserted: removed });
        self.generation += 1;
        Ok(count)
    }

    /// Paste a block of text values starting at display row `row`; value `j`
    /// of each line goes to model column `columns[j]` (the visible columns from
    /// the active cell rightwards). Rows are added when the block extends past
    /// the end. All-or-nothing; one undo step. Returns the number of cells set.
    pub fn paste_block(&mut self, row: usize, block: &[Vec<String>], columns: &[usize]) -> Result<usize, String> {
        self.target()?;
        if block.is_empty() {
            return Ok(0);
        }
        // Validate parsing first.
        let mut parsed: Vec<(usize, usize, CellValue)> = Vec::new();
        let total_rows = self.row_count();
        let needed_new = (row + block.len()).saturating_sub(total_rows);
        for (i, line) in block.iter().enumerate() {
            for (j, text) in line.iter().enumerate() {
                let Some(&c) = columns.get(j) else { break };
                if self.is_virtual_column(c) || c >= self.columns.len() {
                    continue;
                }
                let value = self.parse_for_column(c, text).map_err(|e| trf("Row {}: {}", &[&(row + i + 1), &e]))?;
                parsed.push((row + i, c, CellValue::Value(value)));
            }
        }
        // Check editability of target cells that already exist.
        for (r, c, _) in &parsed {
            if *r < total_rows {
                self.check_cell(*r, *c).map_err(|e| trf("Row {}: {}", &[&(r + 1), &e]))?;
            }
        }
        let mut group = Vec::new();
        if needed_new > 0 {
            let mut ids = Vec::new();
            let mut after = total_rows.checked_sub(1);
            for _ in 0..needed_new {
                let (r, id) = self.insert_row_inner(after, vec![CellValue::Default; self.columns.len()]);
                after = Some(r);
                ids.push(id);
            }
            group.push(Undo::Insert(ids));
        }
        let n = parsed.len();
        match self.set_cells_inner(parsed) {
            Ok(cells) => group.push(Undo::Cells(cells)),
            Err(e) => {
                // Roll back the inserted rows.
                for u in group.into_iter().rev() {
                    self.apply_undo(u);
                }
                return Err(e);
            }
        }
        self.push_undo(Undo::Group(group));
        self.generation += 1;
        Ok(n)
    }

    fn push_undo(&mut self, u: Undo) {
        self.undo.push(u);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// Undo the last edit step. Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(u) = self.undo.pop() else { return false };
        self.apply_undo(u);
        self.generation += 1;
        true
    }

    /// True when [`ResultModel::undo`] would do something.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn apply_undo(&mut self, u: Undo) {
        match u {
            Undo::Cells(list) => {
                for (key, col, prev) in list.into_iter().rev() {
                    match key {
                        RowKey::Stored(s) => {
                            let entry = self.updates.entry(s).or_default();
                            let pos = entry.iter().position(|(c, _)| *c == col);
                            match (pos, prev) {
                                (Some(p), None) => {
                                    entry.remove(p);
                                }
                                (Some(p), Some(v)) => entry[p].1 = v,
                                (None, Some(v)) => entry.push((col, v)),
                                (None, None) => {}
                            }
                            if entry.is_empty() {
                                self.updates.remove(&s);
                            }
                        }
                        RowKey::Inserted(id) => {
                            if let Some(r) = self.inserted.iter_mut().find(|r| r.id == id)
                                && let Some(slot) = r.values.get_mut(col)
                            {
                                *slot = prev.unwrap_or(CellValue::Default);
                            }
                        }
                    }
                }
            }
            Undo::Insert(ids) => self.inserted.retain(|r| !ids.contains(&r.id)),
            Undo::Delete { stored, inserted } => {
                for s in stored {
                    self.deleted.remove(&s);
                }
                for (k, row) in inserted.into_iter().rev() {
                    let k = k.min(self.inserted.len());
                    self.inserted.insert(k, row);
                }
                self.reanchor_inserted();
            }
            Undo::Group(list) => {
                for u in list.into_iter().rev() {
                    self.apply_undo(u);
                }
            }
        }
    }

    /// True when there are unsaved changes.
    pub fn has_changes(&self) -> bool {
        !self.updates.is_empty() || !self.deleted.is_empty() || !self.inserted.is_empty()
    }

    /// Counts of pending changes (rows).
    pub fn pending_counts(&self) -> PendingCounts {
        PendingCounts { updated: self.updates.keys().filter(|s| !self.deleted.contains(s)).count(), inserted: self.inserted.len(), deleted: self.deleted.len() }
    }

    /// Drop all pending changes.
    pub fn discard_changes(&mut self) {
        self.updates.clear();
        self.deleted.clear();
        self.inserted.clear();
        self.undo.clear();
        self.generation += 1;
    }

    /// The pending changes, or `None` when the result is read-only.
    pub fn change_set(&self) -> Option<ChangeSet> {
        let target = self.editability.as_ref().ok()?;
        let col_info = |c: usize| -> Option<(String, String)> {
            let name = target.column_names.get(c)?.clone()?;
            Some((name, self.columns.get(c)?.type_name.clone()))
        };
        let mut deletes = Vec::new();
        let mut updates = Vec::new();
        {
            let mut guard = self.store.lock();
            let mut key_of = |s: u32| -> Vec<KeyValue> {
                let row = guard.get(s as usize);
                target
                    .key_columns
                    .iter()
                    .filter_map(|&k| {
                        let (name, type_name) = col_info(k)?;
                        let value = row.and_then(|r| r.get(k)).cloned().unwrap_or(Value::Null);
                        Some(KeyValue { column: k, name, type_name, value })
                    })
                    .collect()
            };
            let mut del: Vec<u32> = self.deleted.iter().copied().collect();
            del.sort_unstable();
            for s in del {
                deletes.push(RowDelete { row: s, key: key_of(s) });
            }
            let mut upd: Vec<u32> = self.updates.keys().copied().filter(|s| !self.deleted.contains(s)).collect();
            upd.sort_unstable();
            for s in upd {
                let mut changes: Vec<ChangedValue> = self.updates[&s]
                    .iter()
                    .filter_map(|(c, v)| {
                        let (name, type_name) = col_info(*c)?;
                        Some(ChangedValue { column: *c, name, type_name, value: v.clone() })
                    })
                    .collect();
                changes.sort_by_key(|c| c.column);
                if !changes.is_empty() {
                    updates.push(RowUpdate { row: s, key: key_of(s), changes });
                }
            }
        }
        let inserts = self
            .inserted
            .iter()
            .map(|r| RowInsert {
                id: r.id,
                values: r
                    .values
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| **v != CellValue::Default)
                    .filter_map(|(c, v)| {
                        let (name, type_name) = col_info(c)?;
                        Some(ChangedValue { column: c, name, type_name, value: v.clone() })
                    })
                    .collect(),
            })
            .collect();
        Some(ChangeSet { table: target.table.clone(), deletes, updates, inserts })
    }

    /// Parameterised statements saving the pending changes (deletes, updates,
    /// inserts) using [`Dialect::placeholder`]. Empty when read-only.
    pub fn change_statements(&self, dialect: &dyn Dialect) -> Vec<(String, Vec<Value>)> {
        self.change_set().map(|c| c.statements(dialect)).unwrap_or_default()
    }

    /// The same statements with literals, for the "preview SQL" dialog (PRD §10).
    pub fn change_preview_sql(&self, dialect: &dyn Dialect) -> String {
        self.change_set().map(|c| c.preview_sql(dialect)).unwrap_or_default()
    }

    /// After the application committed [`ResultModel::change_statements`]:
    /// write updates into the store, append inserted rows, hide deleted rows.
    /// `DEFAULT` cells become NULL in the store; refresh the result to see
    /// server generated values.
    pub fn apply_saved(&mut self) {
        if !self.has_changes() {
            return;
        }
        let n_cols = self.columns.len();
        let base_len = self.base_len();
        let old_store_len = self.store_len;
        // A materialized order is needed when rows disappear or new rows are not at the end.
        let all_at_end = self.inserted.iter().all(|r| r.after_base >= base_len);
        let need_view = !self.deleted.is_empty() || (!self.inserted.is_empty() && (self.view.is_some() || !all_at_end));
        let mut guard = self.store.lock();
        for (s, edits) in &self.updates {
            if self.deleted.contains(s) {
                continue;
            }
            if let Some(mut row) = guard.get_cloned(*s as usize) {
                for (c, v) in edits {
                    if let Some(slot) = row.get_mut(*c) {
                        *slot = v.value_or_null().clone();
                    }
                }
                guard.set(*s as usize, row);
            }
        }
        let first_new = guard.len();
        for r in &self.inserted {
            let mut row: Row = r.values.iter().map(|v| v.value_or_null().clone()).collect();
            row.resize(n_cols, Value::Null);
            guard.push(row);
        }
        let new_len = guard.len();
        drop(guard);
        if need_view {
            // Still uses the pre-save state (old store length, view and inserted rows).
            let total = self.row_count();
            let mut order: Vec<u32> = Vec::with_capacity(total);
            for d in 0..total {
                match self.row_source(d) {
                    Some(RowSource::Stored(s)) => {
                        if !self.deleted.contains(&s) {
                            order.push(s);
                        }
                    }
                    Some(RowSource::Inserted(k)) => order.push((first_new + k) as u32),
                    None => {}
                }
            }
            if !self.deleted.is_empty() {
                let mut removed = (*self.removed).clone();
                for s in &self.deleted {
                    removed.insert(*s);
                }
                self.removed = Arc::new(removed);
            }
            self.view = Some(order);
        }
        self.store_len = new_len;
        match self.view_kind {
            ViewKind::Identity => self.view_source_len = new_len,
            _ => {
                if self.view_source_len >= old_store_len {
                    self.view_source_len = new_len;
                }
            }
        }
        self.updates.clear();
        self.deleted.clear();
        self.inserted.clear();
        self.undo.clear();
        self.generation += 1;
    }
}

/// Kind of SQL generated for rows ("Generate SQL" menu).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlKind {
    /// `SELECT * ... WHERE key`
    Select,
    /// `INSERT` with all values.
    Insert,
    /// `UPDATE ... SET non-key values WHERE key`
    Update,
    /// `DELETE ... WHERE key`
    Delete,
}

impl ResultModel {
    /// SQL statements for the given display rows using current values (with
    /// pending edits). Rows are identified by the edit target key, or by all
    /// columns when the result has no key. `table` is the qualified table name.
    pub fn generate_rows_sql(&self, kind: SqlKind, rows: &[usize], dialect: &dyn Dialect, table: &str) -> String {
        let target = self.editability.as_ref().ok();
        // (result column, SQL column name, type name)
        let cols: Vec<(usize, String, String)> = (0..self.columns.len())
            .filter_map(|c| {
                let name = match target {
                    Some(t) => t.column_names.get(c)?.clone()?,
                    None => self.columns[c].source.as_ref().map_or_else(|| self.columns[c].name.clone(), |s| s.column.clone()),
                };
                Some((c, name, self.columns[c].type_name.clone()))
            })
            .collect();
        let keys: Vec<usize> = match target {
            Some(t) => t.key_columns.clone(),
            None => cols.iter().map(|(c, _, _)| *c).collect(),
        };
        let lit = |v: &Value, t: &str| dialect.literal(v, (!t.is_empty()).then_some(t));
        let mut out = Vec::new();
        let mut sorted = rows.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for chunk in sorted.chunk_by(|a, b| b == &(a + 1)) {
            self.visit_rows(chunk[0]..chunk[chunk.len() - 1] + 1, |_, cells| {
                let current = |c: usize| cells.get(c).value().clone();
                let original = |c: usize| cells.original(c).cloned().unwrap_or_else(|| cells.get(c).value().clone());
                let where_clause = cols
                    .iter()
                    .filter(|(c, _, _)| keys.contains(c))
                    .map(|(c, name, t)| {
                        let v = original(*c);
                        let q = dialect.quote_ident(name);
                        if v.is_null() { format!("{q} IS NULL") } else { format!("{q} = {}", lit(&v, t)) }
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let stmt = match kind {
                    SqlKind::Select => format!("SELECT * FROM {table} WHERE {where_clause};"),
                    SqlKind::Delete => format!("DELETE FROM {table} WHERE {where_clause};"),
                    SqlKind::Insert => {
                        let names: Vec<String> = cols.iter().map(|(_, n, _)| dialect.quote_ident(n)).collect();
                        let vals: Vec<String> = cols.iter().map(|(c, _, t)| lit(&current(*c), t)).collect();
                        format!("INSERT INTO {table} ({}) VALUES ({});", names.join(", "), vals.join(", "))
                    }
                    SqlKind::Update => {
                        let sets: Vec<String> = cols
                            .iter()
                            .filter(|(c, _, _)| target.is_none() || !keys.contains(c))
                            .map(|(c, n, t)| format!("{} = {}", dialect.quote_ident(n), lit(&current(*c), t)))
                            .collect();
                        format!("UPDATE {table} SET {} WHERE {where_clause};", sets.join(", "))
                    }
                };
                out.push(stmt);
            });
        }
        out.join("\n")
    }
}

/// Cells of one row with pending edits applied (see [`ResultModel::visit_rows`]).
pub struct RowCells<'a> {
    base: Option<&'a Row>,
    edits: Option<&'a [(usize, CellValue)]>,
    inserted: Option<&'a [CellValue]>,
    state: RowState,
    source: RowSource,
    model: &'a ResultModel,
}

impl<'a> RowCells<'a> {
    /// Cell of a result column.
    pub fn get(&self, col: usize) -> CellRef<'a> {
        if let Some(values) = self.inserted {
            return match values.get(col) {
                Some(CellValue::Value(v)) => CellRef::Value(v),
                Some(CellValue::Default) => CellRef::Default,
                None if col < self.model.columns.len() => CellRef::Default,
                None => CellRef::Missing,
            };
        }
        if let Some(e) = self.edits
            && let Some((_, v)) = e.iter().find(|(c, _)| *c == col)
        {
            return match v {
                CellValue::Value(v) => CellRef::Value(v),
                CellValue::Default => CellRef::Default,
            };
        }
        match self.base.and_then(|r| r.get(col)) {
            Some(v) => CellRef::Value(v),
            None => CellRef::Missing,
        }
    }

    /// Original (stored) value, ignoring pending edits.
    pub fn original(&self, col: usize) -> Option<&'a Value> {
        self.base.and_then(|r| r.get(col))
    }

    /// True when the cell has a pending change.
    pub fn is_modified(&self, col: usize) -> bool {
        match self.inserted {
            Some(v) => v.get(col).is_some_and(|v| *v != CellValue::Default),
            None => self.edits.is_some_and(|e| e.iter().any(|(c, _)| *c == col)),
        }
    }

    /// Owned value of any model column (virtual columns are evaluated; errors read as NULL).
    pub fn value_of(&self, col: usize) -> Option<Value> {
        if col < self.model.columns.len() {
            match self.get(col) {
                CellRef::Value(v) => Some(v.clone()),
                CellRef::Default => Some(Value::Null),
                CellRef::Missing => None,
            }
        } else {
            let v = self.model.virtual_columns.get(col - self.model.columns.len())?;
            Some(v.expression.eval(self).unwrap_or(Value::Null))
        }
    }

    /// Evaluate virtual column `col` (model index).
    pub fn virtual_value(&self, col: usize) -> Option<Result<Value, EvalError>> {
        let v = self.model.virtual_columns.get(col.checked_sub(self.model.columns.len())?)?;
        Some(v.expression.eval(self))
    }

    /// Pending state of the row.
    pub fn state(&self) -> RowState {
        self.state
    }

    /// Where the row comes from.
    pub fn source(&self) -> RowSource {
        self.source
    }
}

impl RowValues for RowCells<'_> {
    fn value(&self, col: usize) -> Option<&Value> {
        match self.get(col) {
            CellRef::Value(v) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
