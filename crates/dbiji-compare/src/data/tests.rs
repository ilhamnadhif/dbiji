use super::*;
use dbiji_core::dialect::GenericDialect;
use dbiji_core::driver::MemoryCursor;
use dbiji_core::{DataKind, ErrorKind, ServerMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

/// Cursor that clears the shared "session busy" flag when closed or dropped.
struct Tracked {
    inner: MemoryCursor,
    open: Arc<AtomicBool>,
}

#[async_trait]
impl QueryCursor for Tracked {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        self.inner.next_result().await
    }
    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        self.inner.fetch(max_rows).await
    }
    async fn close(&mut self) -> DbResult<()> {
        self.open.store(false, AtomicOrdering::SeqCst);
        self.inner.close().await
    }
    fn take_messages(&mut self) -> Vec<ServerMessage> {
        self.inner.take_messages()
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.open.store(false, AtomicOrdering::SeqCst);
    }
}

/// Fake session: serves stored tables (rows returned as stored) and refuses
/// a query while a previous cursor of the same "session" is still open.
struct Fake {
    tables: Vec<(String, Vec<ColumnMeta>, Vec<Row>)>,
    queries: Vec<String>,
    open: Arc<AtomicBool>,
}

impl Fake {
    fn new(open: Arc<AtomicBool>) -> Self {
        Fake { tables: Vec::new(), queries: Vec::new(), open }
    }
    fn table(mut self, name: &str, cols: &[(&str, DataKind)], rows: Vec<Row>) -> Self {
        let meta = cols.iter().map(|(n, k)| ColumnMeta::new(*n, format!("{k:?}").to_lowercase(), *k)).collect();
        self.tables.push((name.to_string(), meta, rows));
        self
    }
}

#[async_trait]
impl RowProvider for Fake {
    async fn query(&mut self, sql: &str) -> DbResult<Box<dyn QueryCursor>> {
        if self.open.load(AtomicOrdering::SeqCst) {
            return Err(DbError::internal("another cursor is still open on this session"));
        }
        self.queries.push(sql.to_string());
        let (_, meta, rows) = self
            .tables
            .iter()
            .find(|(n, _, _)| sql.contains(&format!(" FROM {n}")))
            .ok_or_else(|| DbError::query(format!("unknown table in {sql}")))?;
        self.open.store(true, AtomicOrdering::SeqCst);
        Ok(Box::new(Tracked { inner: MemoryCursor::rows(meta.clone(), rows.clone()), open: self.open.clone() }))
    }
}

fn int(i: i64) -> Value {
    Value::Int(i)
}
fn txt(s: &str) -> Value {
    Value::text(s)
}
fn dec(s: &str) -> Value {
    Value::decimal(s)
}

const ITEM_COLS: &[(&str, DataKind)] = &[("id", DataKind::Int), ("name", DataKind::Text), ("price", DataKind::Decimal)];

fn job(src: &str, tgt: &str, keys: &[&str]) -> DataCompareJob {
    let k: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
    let (ss, sn) = src.split_once('.').unwrap_or(("", src));
    let (ts, tn) = tgt.split_once('.').unwrap_or(("", tgt));
    DataCompareJob {
        source: TableSpec::new(ObjectRef::table(Some(ss.to_string()), sn), k.clone()),
        target: TableSpec::new(ObjectRef::table(Some(ts.to_string()), tn), k),
        options: DataCompareOptions::default(),
    }
}

async fn run(job: &DataCompareJob, src: &mut Fake, tgt: &mut Fake) -> DbResult<DataDiff> {
    let d = GenericDialect::new();
    let mut progress = |_: &DataCompareProgress| {};
    run_data_compare(job, src, tgt, &d, &d, &mut progress, &CancellationToken::new()).await
}

fn sessions() -> (Fake, Fake) {
    (Fake::new(Arc::new(AtomicBool::new(false))), Fake::new(Arc::new(AtomicBool::new(false))))
}

#[tokio::test]
async fn merge_join_on_numeric_keys() {
    let (src, tgt) = sessions();
    let mut src = src.table(
        "src.items",
        ITEM_COLS,
        vec![
            vec![int(1), txt("A"), dec("1.50")],
            vec![int(2), txt("B"), dec("2.00")],
            vec![int(3), txt("C"), dec("3.00")],
            vec![int(4), txt("D"), dec("10")],
            vec![int(5), txt("it's"), dec("5")],
        ],
    );
    let mut tgt = tgt.table(
        "dst.items",
        ITEM_COLS,
        vec![
            vec![int(1), txt("A"), dec("1.5")],
            vec![int(2), txt("b"), dec("2.00")],
            vec![int(4), txt("D"), Value::Float(10.0)],
            vec![int(6), txt("F"), dec("6")],
        ],
    );
    let j = job("src.items", "dst.items", &["id"]);
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert_eq!(d.strategy, CompareStrategy::MergeJoin);
    assert_eq!((d.identical, d.different, d.only_in_source, d.only_in_target), (2, 1, 2, 1));
    assert_eq!((d.source_rows, d.target_rows), (5, 4));
    assert!(!d.is_identical());
    assert_eq!(d.columns, vec!["name", "price"]);
    assert_eq!(
        d.sync_statements,
        vec![
            "DELETE FROM dst.items WHERE id = 6".to_string(),
            "UPDATE dst.items SET name = 'B' WHERE id = 2".to_string(),
            "INSERT INTO dst.items (id, name, price) VALUES (3, 'C', 3.00)".to_string(),
            "INSERT INTO dst.items (id, name, price) VALUES (5, 'it''s', 5)".to_string(),
        ]
    );
    let diff = d.samples.iter().find(|s| s.kind == RowDiffKind::Different).unwrap();
    assert_eq!(diff.key, vec![int(2)]);
    assert_eq!(diff.columns, vec!["name"]);
    assert_eq!(src.queries, vec!["SELECT * FROM src.items ORDER BY id"]);
    assert_eq!(tgt.queries, vec!["SELECT * FROM dst.items ORDER BY id"]);
    assert!(!src.open.load(AtomicOrdering::SeqCst) && !tgt.open.load(AtomicOrdering::SeqCst), "cursors are closed");
}

#[tokio::test]
async fn text_keys_use_a_hash_join_independent_of_collation() {
    let cols: &[(&str, DataKind)] = &[("code", DataKind::Text), ("qty", DataKind::Int)];
    let (src, tgt) = sessions();
    // case-sensitive order on one side, case-insensitive on the other
    let mut src = src.table("a.t", cols, vec![vec![txt("B"), int(1)], vec![txt("a"), int(2)], vec![txt("c"), int(3)]]);
    let mut tgt = tgt.table("b.t", cols, vec![vec![txt("a"), int(2)], vec![txt("B"), int(9)], vec![txt("d"), int(4)]]);
    let j = job("a.t", "b.t", &["code"]);
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert_eq!(d.strategy, CompareStrategy::HashJoin);
    assert_eq!((d.identical, d.different, d.only_in_source, d.only_in_target), (1, 1, 1, 1));
    assert_eq!(src.queries, vec!["SELECT * FROM a.t ORDER BY code"]);
    assert_eq!(tgt.queries, vec!["SELECT * FROM b.t"], "no ORDER BY needed for the probe side");
    assert!(d.sync_statements.contains(&"UPDATE b.t SET qty = 1 WHERE code = 'B'".to_string()));
    assert!(d.sync_statements.contains(&"INSERT INTO b.t (code, qty) VALUES ('c', 3)".to_string()));
    assert!(d.sync_statements.contains(&"DELETE FROM b.t WHERE code = 'd'".to_string()));
}

#[tokio::test]
async fn hash_join_works_with_one_shared_session() {
    let cols: &[(&str, DataKind)] = &[("code", DataKind::Text), ("qty", DataKind::Int)];
    let busy = Arc::new(AtomicBool::new(false));
    let mut src = Fake::new(busy.clone()).table("a.t", cols, vec![vec![txt("x"), int(1)]]);
    let mut tgt = Fake::new(busy.clone()).table("a.t2", cols, vec![vec![txt("x"), int(1)]]);
    let j = job("a.t", "a.t2", &["code"]);
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert!(d.is_identical());
    // the merge join needs two sessions: with one it is refused cleanly
    let mut src = Fake::new(busy.clone()).table("a.items", ITEM_COLS, vec![vec![int(1), txt("A"), dec("1")]]);
    let mut tgt = Fake::new(busy.clone()).table("a.items2", ITEM_COLS, vec![vec![int(1), txt("A"), dec("1")]]);
    let err = run(&job("a.items", "a.items2", &["id"]), &mut src, &mut tgt).await.unwrap_err();
    assert!(err.message.contains("still open"), "{err}");
    assert!(!busy.load(AtomicOrdering::SeqCst), "the source cursor was released");
    // forcing the hash join makes it work on one session
    let mut j = job("a.items", "a.items2", &["id"]);
    j.options.strategy = CompareStrategy::HashJoin;
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert!(d.is_identical());
}

#[tokio::test]
async fn null_keys_are_skipped_and_reported() {
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", ITEM_COLS, vec![vec![Value::Null, txt("x"), dec("1")], vec![int(1), txt("A"), dec("1")]]);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![vec![Value::Null, txt("y"), dec("2")], vec![int(1), txt("A"), dec("1")]]);
    let d = run(&job("s.t", "t.t", &["id"]), &mut src, &mut tgt).await.unwrap();
    assert!(d.is_identical());
    assert_eq!(d.skipped_null_keys, 2);
}

#[tokio::test]
async fn duplicate_and_unsorted_keys_are_errors() {
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", ITEM_COLS, vec![vec![int(1), txt("A"), dec("1")], vec![int(1), txt("B"), dec("1")]]);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![vec![int(1), txt("A"), dec("1")]]);
    let err = run(&job("s.t", "t.t", &["id"]), &mut src, &mut tgt).await.unwrap_err();
    assert!(err.message.contains("Duplicate key"), "{err}");

    let (src, tgt) = sessions();
    let mut src = src.table("s.t", ITEM_COLS, vec![vec![int(2), txt("A"), dec("1")], vec![int(1), txt("B"), dec("1")]]);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![]);
    let err = run(&job("s.t", "t.t", &["id"]), &mut src, &mut tgt).await.unwrap_err();
    assert!(err.message.contains("not sorted"), "{err}");
    assert!(!src.open.load(AtomicOrdering::SeqCst) && !tgt.open.load(AtomicOrdering::SeqCst), "cursors closed on error");

    let cols: &[(&str, DataKind)] = &[("code", DataKind::Text), ("qty", DataKind::Int)];
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", cols, vec![vec![txt("a"), int(1)], vec![txt("a"), int(2)]]);
    let mut tgt = tgt.table("t.t", cols, vec![]);
    let err = run(&job("s.t", "t.t", &["code"]), &mut src, &mut tgt).await.unwrap_err();
    assert!(err.message.contains("Duplicate key"), "{err}");
}

#[tokio::test]
async fn hash_join_memory_limit() {
    let cols: &[(&str, DataKind)] = &[("code", DataKind::Text), ("qty", DataKind::Int)];
    let (src, tgt) = sessions();
    let rows = (0..10).map(|i| vec![txt(&format!("k{i}")), int(i)]).collect();
    let mut src = src.table("s.t", cols, rows);
    let mut tgt = tgt.table("t.t", cols, vec![]);
    let mut j = job("s.t", "t.t", &["code"]);
    j.options.max_hash_rows = 5;
    let err = run(&j, &mut src, &mut tgt).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(err.message.contains("more than 5 rows"), "{err}");
    assert!(tgt.queries.is_empty(), "the target is not read after the limit is hit");
}

#[tokio::test]
async fn cancellation_stops_the_comparison() {
    let (src, tgt) = sessions();
    let rows: Vec<Row> = (0..100).map(|i| vec![int(i), txt("x"), dec("1")]).collect();
    let mut src = src.table("s.t", ITEM_COLS, rows.clone());
    let mut tgt = tgt.table("t.t", ITEM_COLS, rows);
    let mut j = job("s.t", "t.t", &["id"]);
    j.options.fetch_size = 10;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let d = GenericDialect::new();
    let mut progress = |_: &DataCompareProgress| {};
    let err = run_data_compare(&j, &mut src, &mut tgt, &d, &d, &mut progress, &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!src.open.load(AtomicOrdering::SeqCst) && !tgt.open.load(AtomicOrdering::SeqCst));
}

#[tokio::test]
async fn normalisation_options() {
    let cols: &[(&str, DataKind)] = &[("id", DataKind::Int), ("name", DataKind::Text), ("amount", DataKind::Float)];
    let rows_s = vec![vec![int(1), txt("Alice  "), Value::Float(1.0)], vec![int(2), txt("BOB"), Value::Float(2.0)]];
    let rows_t = vec![vec![int(1), txt("alice"), Value::Float(1.004)], vec![int(2), txt("bob"), Value::Float(2.5)]];
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", cols, rows_s.clone());
    let mut tgt = tgt.table("t.t", cols, rows_t.clone());
    let d = run(&job("s.t", "t.t", &["id"]), &mut src, &mut tgt).await.unwrap();
    assert_eq!(d.different, 2);
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", cols, rows_s);
    let mut tgt = tgt.table("t.t", cols, rows_t);
    let mut j = job("s.t", "t.t", &["id"]);
    j.options = DataCompareOptions { ignore_case: true, trim_text: true, numeric_tolerance: 0.01, ..Default::default() };
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert_eq!((d.identical, d.different), (1, 1));
    assert_eq!(d.samples[0].columns, vec!["amount"]);
}

#[tokio::test]
async fn explicit_columns_are_matched_by_position() {
    let src_cols: &[(&str, DataKind)] = &[("id", DataKind::Int), ("name", DataKind::Text), ("price", DataKind::Decimal)];
    let tgt_cols: &[(&str, DataKind)] = &[("item_id", DataKind::Int), ("title", DataKind::Text), ("cost", DataKind::Decimal)];
    let (src, tgt) = sessions();
    let mut src = src.table("s.items", src_cols, vec![vec![int(1), txt("A"), dec("2")]]);
    let mut tgt = tgt.table("t.goods", tgt_cols, vec![vec![int(1), txt("Z"), dec("2.0")]]);
    let mut j = job("s.items", "t.goods", &["id"]);
    j.source.columns = vec!["name".into(), "price".into()];
    j.target.key_columns = vec!["item_id".into()];
    j.target.columns = vec!["title".into(), "cost".into()];
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert_eq!(d.different, 1);
    assert_eq!(d.sync_statements, vec!["UPDATE t.goods SET title = 'A' WHERE item_id = 1".to_string()]);
    assert_eq!(src.queries, vec!["SELECT id, name, price FROM s.items ORDER BY id"]);
    assert_eq!(tgt.queries, vec!["SELECT item_id, title, cost FROM t.goods ORDER BY item_id"]);
}

#[tokio::test]
async fn unmatched_columns_and_where_clause() {
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", &[("id", DataKind::Int), ("a", DataKind::Text), ("extra", DataKind::Text)], vec![vec![int(1), txt("x"), txt("e")]]);
    let mut tgt = tgt.table("t.t", &[("id", DataKind::Int), ("a", DataKind::Text), ("other", DataKind::Int)], vec![vec![int(1), txt("x"), int(5)]]);
    let mut j = job("s.t", "t.t", &["id"]);
    j.source.where_clause = Some("id < 100".into());
    let d = run(&j, &mut src, &mut tgt).await.unwrap();
    assert!(d.is_identical());
    assert_eq!(d.unmatched_columns, vec!["extra", "other"]);
    assert_eq!(d.warnings.len(), 1);
    assert_eq!(src.queries, vec!["SELECT * FROM s.t WHERE id < 100 ORDER BY id"]);
}

#[tokio::test]
async fn progress_samples_and_sync_limits() {
    let (src, tgt) = sessions();
    let rows: Vec<Row> = (0..12_000).map(|i| vec![int(i), txt("x"), dec("1")]).collect();
    let mut src = src.table("s.t", ITEM_COLS, rows);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![]);
    let mut j = job("s.t", "t.t", &["id"]);
    j.options.max_samples = 3;
    j.options.max_sync_statements = 10;
    let d = GenericDialect::new();
    let mut calls = 0usize;
    let mut last = DataCompareProgress::default();
    let mut progress = |p: &DataCompareProgress| {
        calls += 1;
        last = *p;
    };
    let diff = run_data_compare(&j, &mut src, &mut tgt, &d, &d, &mut progress, &CancellationToken::new()).await.unwrap();
    assert!(calls >= 3, "periodic progress plus the final report: {calls}");
    assert_eq!(last.only_in_source, 12_000);
    assert_eq!(diff.samples.len(), 3);
    assert_eq!(diff.sync_statements.len(), 10);
    assert!(diff.sync_truncated);
    let mut j2 = j.clone();
    j2.options.generate_sync = false;
    let (src2, tgt2) = sessions();
    let mut src2 = src2.table("s.t", ITEM_COLS, vec![vec![int(1), txt("x"), dec("1")]]);
    let mut tgt2 = tgt2.table("t.t", ITEM_COLS, vec![]);
    let d2 = run(&j2, &mut src2, &mut tgt2).await.unwrap();
    assert!(d2.sync_statements.is_empty() && !d2.sync_truncated);
}

#[tokio::test]
async fn configuration_errors() {
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", ITEM_COLS, vec![]);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![]);
    let err = run(&job("s.t", "t.t", &[]), &mut src, &mut tgt).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
    let err = run(&job("s.t", "t.t", &["missing"]), &mut src, &mut tgt).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
    assert!(!src.open.load(AtomicOrdering::SeqCst));
}

#[tokio::test]
async fn short_rows_do_not_panic() {
    let (src, tgt) = sessions();
    let mut src = src.table("s.t", ITEM_COLS, vec![vec![int(1), txt("A")]]);
    let mut tgt = tgt.table("t.t", ITEM_COLS, vec![vec![int(1), txt("A"), dec("1")]]);
    let d = run(&job("s.t", "t.t", &["id"]), &mut src, &mut tgt).await.unwrap();
    assert_eq!(d.different, 1);
    assert_eq!(d.sync_statements, vec!["UPDATE t.t SET price = NULL WHERE id = 1".to_string()]);
}

#[test]
fn sessions_are_row_providers() {
    fn provider<T: RowProvider>() {}
    provider::<&mut dyn Session>();
    provider::<Box<dyn Session>>();
}

#[test]
fn job_serde_roundtrip() {
    let j = job("s.t", "t.t", &["id"]);
    let json = serde_json::to_string(&j).unwrap();
    let back: DataCompareJob = serde_json::from_str(&json).unwrap();
    assert_eq!(back, j);
}
