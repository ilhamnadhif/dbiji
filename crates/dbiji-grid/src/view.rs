//! Client side sorting and filtering of fetched rows.
//!
//! The functions here compute a *view index* (display position → store row)
//! and are designed to run on a background thread: they lock the shared
//! [`RowStore`](dbiji_core::RowStore) in short chunks so the UI thread (which
//! locks it once per frame) and the fetch task never wait long.

use crate::expr::{Expr, ExprError, Node, RowValues, compare_values};
use dbiji_core::{SharedRowStore, Value, tr, trf};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Rows processed per lock acquisition.
const CHUNK_ROWS: usize = 4096;

/// One sort criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SortKey {
    /// Model column index (virtual columns follow the result columns).
    pub column: usize,
    /// Sort descending.
    pub descending: bool,
}

impl SortKey {
    /// Ascending key on `column`.
    pub fn asc(column: usize) -> Self {
        SortKey { column, descending: false }
    }

    /// Descending key on `column`.
    pub fn desc(column: usize) -> Self {
        SortKey { column, descending: true }
    }
}

/// Comparison used by "filter by this value".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterOp {
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `LIKE` (the value is used as the pattern; `%` wildcards are kept)
    Like,
    /// `NOT LIKE`
    NotLike,
    /// Case-insensitive "contains" (`ILIKE '%value%'`).
    Contains,
    /// `IS NULL`
    IsNull,
    /// `IS NOT NULL`
    IsNotNull,
}

impl FilterOp {
    /// Operators offered in menus, in display order.
    pub const ALL: [FilterOp; 11] = [
        FilterOp::Eq,
        FilterOp::NotEq,
        FilterOp::Gt,
        FilterOp::GtEq,
        FilterOp::Lt,
        FilterOp::LtEq,
        FilterOp::Like,
        FilterOp::NotLike,
        FilterOp::Contains,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
    ];

    /// SQL spelling of the operator.
    pub fn sql(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::NotEq => "<>",
            FilterOp::Gt => ">",
            FilterOp::GtEq => ">=",
            FilterOp::Lt => "<",
            FilterOp::LtEq => "<=",
            FilterOp::Like => "LIKE",
            FilterOp::NotLike => "NOT LIKE",
            FilterOp::Contains => "LIKE",
            FilterOp::IsNull => "IS NULL",
            FilterOp::IsNotNull => "IS NOT NULL",
        }
    }

    /// True for operators without a right-hand value.
    pub fn is_unary(self) -> bool {
        matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }
}

/// A compiled client side filter.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientFilter {
    expr: Expr,
}

impl ClientFilter {
    /// Parse a filter expression (see [`crate::expr`]) against the model column names
    /// (result columns followed by virtual columns).
    pub fn parse<S: AsRef<str>>(text: &str, column_names: &[S]) -> Result<ClientFilter, ExprError> {
        Ok(ClientFilter { expr: Expr::compile(text, column_names)? })
    }

    /// Wrap an already compiled expression.
    pub fn from_expr(expr: Expr) -> ClientFilter {
        ClientFilter { expr }
    }

    /// `column op value` condition, e.g. built from "filter by this value".
    pub fn condition(column: usize, column_name: &str, op: FilterOp, value: &Value) -> ClientFilter {
        let col = Box::new(Node::Column(column));
        let lit = |v: Value| Box::new(Node::Literal(v));
        let node = match op {
            FilterOp::IsNull => Node::IsNull { expr: col, negated: false },
            FilterOp::IsNotNull => Node::IsNull { expr: col, negated: true },
            FilterOp::Like | FilterOp::NotLike => {
                Node::Like { expr: col, pattern: lit(Value::text(value.to_text().into_owned())), negated: op == FilterOp::NotLike, case_insensitive: false }
            }
            FilterOp::Contains => Node::Like { expr: col, pattern: lit(Value::text(format!("%{}%", escape_like(&value.to_text())))), negated: false, case_insensitive: true },
            _ => {
                let bop = match op {
                    FilterOp::Eq => crate::expr::BinaryOp::Eq,
                    FilterOp::NotEq => crate::expr::BinaryOp::NotEq,
                    FilterOp::Gt => crate::expr::BinaryOp::Gt,
                    FilterOp::GtEq => crate::expr::BinaryOp::GtEq,
                    FilterOp::Lt => crate::expr::BinaryOp::Lt,
                    _ => crate::expr::BinaryOp::LtEq,
                };
                if value.is_null() {
                    // `= NULL` never matches in SQL; users mean IS [NOT] NULL.
                    Node::IsNull { expr: col, negated: bop != crate::expr::BinaryOp::Eq }
                } else {
                    Node::Binary(bop, col, lit(value.clone()))
                }
            }
        };
        let text = condition_text(column_name, op, value);
        ClientFilter { expr: Expr::from_node(node, text) }
    }

    /// Combine with another filter using `AND`.
    pub fn and(self, other: ClientFilter) -> ClientFilter {
        let text = format!("({}) AND ({})", self.expr.source(), other.expr.source());
        let node = Node::Binary(crate::expr::BinaryOp::And, Box::new(self.expr.node().clone()), Box::new(other.expr.node().clone()));
        ClientFilter { expr: Expr::from_node(node, text) }
    }

    /// Source text of the filter (as typed, or generated).
    pub fn text(&self) -> &str {
        self.expr.source()
    }

    /// The compiled expression.
    pub fn expr(&self) -> &Expr {
        &self.expr
    }

    /// Does `row` pass the filter?
    pub fn matches<R: RowValues + ?Sized>(&self, row: &R) -> bool {
        self.expr.matches(row)
    }
}

/// Escape `%`, `_` and `\` for use inside a LIKE pattern.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn quote_expr_ident(name: &str) -> String {
    if !name.is_empty()
        && name.chars().next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_alphanumeric())
        && !["and", "or", "not", "is", "null", "true", "false", "like", "ilike", "in", "between"].contains(&name.to_ascii_lowercase().as_str())
    {
        name.to_string()
    } else {
        // `[…]` cannot hold `]`: such a name is written as "…" (both are quoted names).
        if name.contains(']') { format!("\"{}\"", name.replace('"', "\"\"")) } else { format!("[{name}]") }
    }
}

fn expr_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if *b {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => {
            let t = v.to_text();
            if t.parse::<f64>().is_ok() { t.into_owned() } else { format!("'{}'", t.replace('\'', "''")) }
        }
        other => format!("'{}'", other.to_text().replace('\'', "''")),
    }
}

/// Text form of a condition in the expression language.
pub fn condition_text(column_name: &str, op: FilterOp, value: &Value) -> String {
    let c = quote_expr_ident(column_name);
    match op {
        FilterOp::IsNull => format!("{c} IS NULL"),
        FilterOp::IsNotNull => format!("{c} IS NOT NULL"),
        FilterOp::Contains => {
            format!("{c} ILIKE {}", expr_literal(&Value::text(format!("%{}%", escape_like(&value.to_text())))))
        }
        _ if value.is_null() => {
            if op == FilterOp::Eq {
                format!("{c} IS NULL")
            } else {
                format!("{c} IS NOT NULL")
            }
        }
        FilterOp::Like | FilterOp::NotLike => {
            format!("{c} {} {}", op.sql(), expr_literal(&Value::text(value.to_text().into_owned())))
        }
        _ => format!("{c} {} {}", op.sql(), expr_literal(value)),
    }
}

/// Store rows that were deleted and saved; they are hidden from every view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowBitSet {
    words: Vec<u64>,
    count: usize,
}

impl RowBitSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `row`; returns true when it was not present.
    pub fn insert(&mut self, row: u32) -> bool {
        let (w, b) = (row as usize / 64, row as usize % 64);
        if self.words.len() <= w {
            self.words.resize(w + 1, 0);
        }
        let had = self.words[w] & (1 << b) != 0;
        self.words[w] |= 1 << b;
        if !had {
            self.count += 1;
        }
        !had
    }

    /// Is `row` in the set?
    pub fn contains(&self, row: u32) -> bool {
        let (w, b) = (row as usize / 64, row as usize % 64);
        self.words.get(w).is_some_and(|x| x & (1 << b) != 0)
    }

    /// Number of rows in the set.
    pub fn len(&self) -> usize {
        self.count
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Result of a view computation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewIndex {
    /// Display position → store row.
    pub rows: Vec<u32>,
    /// Store length the index was computed from; rows appended later are not included.
    pub source_len: usize,
}

/// What a client view represents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewSpec {
    /// Sort keys, most significant first. Empty keeps store order.
    pub sort: Vec<SortKey>,
    /// Optional row filter.
    pub filter: Option<ClientFilter>,
}

impl ViewSpec {
    /// True when the spec neither sorts nor filters.
    pub fn is_identity(&self) -> bool {
        self.sort.is_empty() && self.filter.is_none()
    }
}

/// Extended row: store values followed by computed virtual column values.
struct ExtRow<'a> {
    base: &'a [Value],
    base_len: usize,
    virt: &'a [Value],
}

impl RowValues for ExtRow<'_> {
    fn value(&self, col: usize) -> Option<&Value> {
        if col < self.base_len { self.base.get(col) } else { self.virt.get(col - self.base_len) }
    }
}

/// Context needed to evaluate model columns in a background job.
#[derive(Clone, Default)]
pub struct ViewContext {
    /// Number of result (store) columns; virtual columns follow.
    pub base_columns: usize,
    /// Virtual column expressions (bound to result columns only).
    pub virtual_columns: Vec<Expr>,
    /// Saved deletions to exclude.
    pub removed: Arc<RowBitSet>,
}

/// Compute a view (filter, then stable sort) over the store. Returns `None`
/// when `cancel` was set.
pub fn compute_view(store: &SharedRowStore, spec: &ViewSpec, ctx: &ViewContext, cancel: &AtomicBool) -> Option<ViewIndex> {
    let source_len = store.lock().len();
    // Which virtual columns must be computed per row.
    let mut needed: Vec<usize> = Vec::new();
    if let Some(f) = &spec.filter {
        needed.extend(f.expr().referenced_columns());
    }
    needed.extend(spec.sort.iter().map(|k| k.column));
    let virt_needed: Vec<usize> = {
        let mut v: Vec<usize> = needed.iter().filter(|c| **c >= ctx.base_columns).map(|c| c - ctx.base_columns).filter(|c| *c < ctx.virtual_columns.len()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let sort_cols: Vec<usize> = spec.sort.iter().map(|k| k.column).collect();
    let mut rows: Vec<u32> = Vec::with_capacity(if spec.filter.is_some() { 1024 } else { source_len });
    // Sort keys extracted per kept row: flattened `rows.len() * sort_cols.len()`.
    let mut keys: Vec<Value> = Vec::with_capacity(if sort_cols.is_empty() { 0 } else { source_len * sort_cols.len() });
    let mut virt_values: Vec<Value> = vec![Value::Null; ctx.virtual_columns.len()];
    let mut start = 0usize;
    while start < source_len {
        if cancel.load(AtomicOrdering::Relaxed) {
            return None;
        }
        let end = (start + CHUNK_ROWS).min(source_len);
        let mut guard = store.lock();
        guard.for_each_in(start, end, |i, row| {
            let idx = i as u32;
            if ctx.removed.contains(idx) {
                return;
            }
            for &v in &virt_needed {
                virt_values[v] = ctx.virtual_columns[v].eval(&row[..]).unwrap_or(Value::Null);
            }
            let ext = ExtRow { base: row, base_len: ctx.base_columns, virt: &virt_values };
            if let Some(f) = &spec.filter
                && !f.matches(&ext)
            {
                return;
            }
            rows.push(idx);
            for &c in &sort_cols {
                keys.push(ext.value(c).cloned().unwrap_or(Value::Null));
            }
        });
        drop(guard);
        start = end;
    }
    if !sort_cols.is_empty() {
        if cancel.load(AtomicOrdering::Relaxed) {
            return None;
        }
        let n = sort_cols.len();
        let mut order: Vec<u32> = (0..rows.len() as u32).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            for (k, key) in spec.sort.iter().enumerate() {
                let o = sort_cmp(&keys[a * n + k], &keys[b * n + k]);
                let o = if key.descending { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
        rows = order.into_iter().map(|i| rows[i as usize]).collect();
    }
    Some(ViewIndex { rows, source_len })
}

/// Total order for sorting: NULLs first, numbers numerically, text naturally.
pub fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        // Decimals exactly, but only against numbers: comparing them numerically with
        // text too would make the order intransitive (sort_by may then panic).
        _ => {
            let numeric = |v: &Value| matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_));
            if (matches!(a, Value::Decimal(_)) || matches!(b, Value::Decimal(_))) && numeric(a) && numeric(b) {
                compare_values(a, b).unwrap_or_else(|| a.cmp_for_sort(b))
            } else {
                a.cmp_for_sort(b)
            }
        }
    }
}

/// Stable sort of all stored rows by `keys` (store columns only).
pub fn compute_sort_index(store: &SharedRowStore, keys: &[SortKey]) -> Vec<u32> {
    let base_columns = usize::MAX;
    let ctx = ViewContext { base_columns, ..Default::default() };
    let spec = ViewSpec { sort: keys.to_vec(), filter: None };
    compute_view(store, &spec, &ctx, &AtomicBool::new(false)).map(|v| v.rows).unwrap_or_default()
}

/// Store rows (ascending) that pass `filter` (store columns only).
pub fn compute_filter_index(store: &SharedRowStore, filter: &ClientFilter) -> Vec<u32> {
    let ctx = ViewContext { base_columns: usize::MAX, ..Default::default() };
    let spec = ViewSpec { sort: Vec::new(), filter: Some(filter.clone()) };
    compute_view(store, &spec, &ctx, &AtomicBool::new(false)).map(|v| v.rows).unwrap_or_default()
}

/// A view computation running on a background thread.
pub struct ViewJob {
    spec: ViewSpec,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<Option<ViewIndex>, String>>,
    started: Instant,
}

impl std::fmt::Debug for ViewJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewJob").field("spec", &self.spec).field("started", &self.started).finish()
    }
}

/// Outcome of polling a [`ViewJob`].
#[derive(Debug)]
pub enum JobPoll {
    /// Still running.
    Running,
    /// Finished with this index.
    Done(ViewIndex),
    /// Cancelled or the worker failed.
    Failed(String),
}

impl ViewJob {
    /// Start computing `spec` on a new thread. `on_done` is called from the
    /// worker when it finishes (e.g. `move || ctx.request_repaint()`).
    pub fn spawn(store: SharedRowStore, spec: ViewSpec, ctx: ViewContext, on_done: Box<dyn Fn() + Send>) -> ViewJob {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let job_spec = spec.clone();
        let flag = cancel.clone();
        let spawned = std::thread::Builder::new().name("dbiji-grid-view".into()).spawn(move || {
            let before = store.lock().read_failures();
            let result = compute_view(&store, &job_spec, &ctx, &flag);
            // Rows of the disk cache that could not be read: the view would silently miss them.
            let lost = store.lock().read_failures() != before;
            let _ = tx.send(if lost { Err(tr("Some fetched rows could not be read back from the disk cache; refresh the result").to_string()) } else { Ok(result) });
            on_done();
        });
        if let Err(e) = spawned {
            tracing::error!("cannot start view worker: {e}");
        }
        ViewJob { spec, cancel, rx, started: Instant::now() }
    }

    /// The spec being computed.
    pub fn spec(&self) -> &ViewSpec {
        &self.spec
    }

    /// Time since the job started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Ask the worker to stop.
    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }

    /// Non-blocking check for the result.
    pub fn poll(&self) -> JobPoll {
        match self.rx.try_recv() {
            Ok(Ok(Some(v))) => JobPoll::Done(v),
            Ok(Ok(None)) => JobPoll::Failed(tr("Sorting/filtering was cancelled").to_string()),
            Ok(Err(e)) => JobPoll::Failed(e),
            Err(mpsc::TryRecvError::Empty) => JobPoll::Running,
            Err(mpsc::TryRecvError::Disconnected) => JobPoll::Failed(trf("Sorting/filtering failed after {} ms", &[&self.started.elapsed().as_millis()])),
        }
    }

    /// Block until the job finishes (tests, synchronous callers).
    pub fn wait(self) -> JobPoll {
        match self.rx.recv() {
            Ok(Ok(Some(v))) => JobPoll::Done(v),
            Ok(Ok(None)) => JobPoll::Failed(tr("Sorting/filtering was cancelled").to_string()),
            Ok(Err(e)) => JobPoll::Failed(e),
            Err(_) => JobPoll::Failed(tr("Sorting/filtering failed").to_string()),
        }
    }
}

impl Drop for ViewJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::RowStore;

    fn store(n: usize) -> SharedRowStore {
        let s = RowStore::shared(64 * 1024 * 1024);
        {
            let mut g = s.lock();
            for i in 0..n {
                let name = if i % 10 == 0 { Value::Null } else { Value::text(format!("n{}", i % 7)) };
                g.push(vec![Value::Int((i * 37 % 101) as i64), name, Value::Float(i as f64 / 2.0)]);
            }
        }
        s
    }

    #[test]
    fn sort_index_is_stable_and_complete() {
        let s = store(10_000);
        let idx = compute_sort_index(&s, &[SortKey::asc(0)]);
        assert_eq!(idx.len(), 10_000);
        let mut g = s.lock();
        let mut prev: Option<(i64, u32)> = None;
        for &r in &idx {
            let v = g.cell(r as usize, 0).unwrap().as_i64().unwrap();
            if let Some((pv, pr)) = prev {
                assert!(pv < v || (pv == v && pr < r), "not stable/sorted at {r}");
            }
            prev = Some((v, r));
        }
        drop(g);
        let desc = compute_sort_index(&s, &[SortKey::desc(1), SortKey::asc(0)]);
        let mut g = s.lock();
        // NULLs sort first ascending, so last when descending.
        assert!(g.cell(desc[desc.len() - 1] as usize, 1).unwrap().is_null());
        assert!(!g.cell(desc[0] as usize, 1).unwrap().is_null());
    }

    #[test]
    fn filter_index() {
        let s = store(1000);
        let f = ClientFilter::parse("c0 > 50 and c1 is not null", &["c0", "c1", "c2"]).unwrap();
        let idx = compute_filter_index(&s, &f);
        let mut g = s.lock();
        let expected: Vec<u32> = (0..1000u32)
            .filter(|&i| {
                let r = g.get(i as usize).unwrap();
                r[0].as_i64().unwrap() > 50 && !r[1].is_null()
            })
            .collect();
        assert_eq!(idx, expected);
    }

    #[test]
    fn view_with_virtual_columns_and_removed_rows() {
        let s = store(100);
        let virt = Expr::compile("c0 * 2", &["c0", "c1", "c2"]).unwrap();
        let mut removed = RowBitSet::new();
        removed.insert(5);
        removed.insert(6);
        assert_eq!(removed.len(), 2);
        let ctx = ViewContext { base_columns: 3, virtual_columns: vec![virt], removed: Arc::new(removed) };
        let filter = ClientFilter::parse("double >= 100", &["c0", "c1", "c2", "double"]).unwrap();
        let spec = ViewSpec { sort: vec![SortKey::desc(3)], filter: Some(filter) };
        let v = compute_view(&s, &spec, &ctx, &AtomicBool::new(false)).unwrap();
        assert_eq!(v.source_len, 100);
        assert!(!v.rows.contains(&5) && !v.rows.contains(&6));
        let mut g = s.lock();
        let vals: Vec<i64> = v.rows.iter().map(|&r| g.cell(r as usize, 0).unwrap().as_i64().unwrap() * 2).collect();
        assert!(vals.iter().all(|&x| x >= 100));
        assert!(vals.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn background_job_and_cancel() {
        let s = store(5000);
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let job = ViewJob::spawn(
            s.clone(),
            ViewSpec { sort: vec![SortKey::asc(2)], filter: None },
            ViewContext { base_columns: 3, ..Default::default() },
            Box::new(move || d2.store(true, AtomicOrdering::SeqCst)),
        );
        match job.wait() {
            JobPoll::Done(v) => assert_eq!(v.rows.len(), 5000),
            other => panic!("unexpected {other:?}"),
        }
        // The result is sent first, then the worker calls on_done.
        let t0 = Instant::now();
        while !done.load(AtomicOrdering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(done.load(AtomicOrdering::SeqCst));
        let cancelled = compute_view(&s, &ViewSpec { sort: vec![SortKey::asc(0)], filter: None }, &ViewContext { base_columns: 3, ..Default::default() }, &AtomicBool::new(true));
        assert!(cancelled.is_none());
    }

    #[test]
    fn conditions() {
        let row = vec![Value::Int(5), Value::text("50%_off"), Value::Null];
        let names = ["a", "b", "c"];
        let c = |col: usize, op: FilterOp, v: Value| ClientFilter::condition(col, names[col], op, &v);
        assert!(c(0, FilterOp::Eq, Value::Int(5)).matches(&row));
        assert!(c(0, FilterOp::GtEq, Value::Int(5)).matches(&row));
        assert!(!c(0, FilterOp::Gt, Value::Int(5)).matches(&row));
        assert!(c(1, FilterOp::Contains, Value::text("%_o")).matches(&row));
        assert!(!c(1, FilterOp::Contains, Value::text("x%")).matches(&row));
        assert!(c(1, FilterOp::Like, Value::text("50%")).matches(&row));
        assert!(c(2, FilterOp::IsNull, Value::Null).matches(&row));
        assert!(c(2, FilterOp::Eq, Value::Null).matches(&row));
        assert!(!c(2, FilterOp::NotEq, Value::Null).matches(&row));
        let both = c(0, FilterOp::Eq, Value::Int(5)).and(c(2, FilterOp::IsNull, Value::Null));
        assert!(both.matches(&row));
        assert_eq!(both.text(), "(a = 5) AND (c IS NULL)");
        assert_eq!(condition_text("my col", FilterOp::Eq, &Value::text("it's")), "[my col] = 'it''s'");
        assert_eq!(condition_text("x", FilterOp::Contains, &Value::text("a_b")), "x ILIKE '%a\\_b%'");
        // Generated text parses back to an equivalent filter.
        let f = ClientFilter::parse(both.text(), &names).unwrap();
        assert!(f.matches(&row));
        let f = ClientFilter::parse(&condition_text("b", FilterOp::Contains, &Value::text("%_o")), &names).unwrap();
        assert!(f.matches(&row));
    }
}
