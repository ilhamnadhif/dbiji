//! Export engine and multi-object export jobs: threads, sessions, file name
//! patterns, failures, progress and cancellation.

mod common;

use async_trait::async_trait;
use common::*;
use dbiji_core::{ColumnMeta, DataKind, DbResult, ErrorKind, ObjectRef, Row, RowStore, Value};
use dbiji_transfer::*;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn database(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT);
         CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT);
         CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT);",
    )
    .unwrap();
    for (t, n) in [("a", 10), ("b", 25), ("c", 3)] {
        for i in 0..n {
            conn.execute(&format!("INSERT INTO {t} VALUES (?, ?)"), rusqlite::params![i, format!("{t}{i}")]).unwrap();
        }
    }
}

fn job(dir: &Path, items: Vec<ExportItem>) -> ExportJob {
    ExportJob { items, format: ExportFormat::Csv, output_folder: dir.join("out"), file_name_pattern: "${schema}${table}_${index}".into(), ..Default::default() }
}

fn table(name: &str) -> ExportItem {
    ExportItem::Table { table: ObjectRef::table(None, name), where_clause: None }
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

#[tokio::test]
async fn parallel_job_with_new_connections() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let items = vec![
        table("a"),
        table("b"),
        ExportItem::Table { table: ObjectRef::table(None, "c"), where_clause: Some("id > 0".into()) },
        ExportItem::Query { sql: "SELECT id * 2 AS double FROM a".into(), name: Some("doubled".into()) },
    ];
    let mut j = job(dir.path(), items);
    j.threads = 3;
    j.count_rows = true;
    let progress = Progress::shared();
    let s = run_export_job(j, factory.clone(), progress.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(factory.opened(), 3);
    assert_eq!(s.failed(), 0, "{:?}", s.items);
    assert_eq!(s.items.iter().map(|i| i.rows).collect::<Vec<_>>(), vec![10, 25, 2, 10]);
    assert_eq!(s.rows, 47);
    let out = dir.path().join("out");
    assert_eq!(lines(&out.join("a_1.csv")), 11);
    assert_eq!(lines(&out.join("b_2.csv")), 26);
    assert_eq!(lines(&out.join("c_3.csv")), 3);
    assert_eq!(std::fs::read_to_string(out.join("doubled_4.csv")).unwrap().lines().nth(1), Some("0"));
    assert_eq!(progress.items_done(), 4);
    assert_eq!(progress.items_total(), 4);
    assert_eq!(progress.rows(), 47);
    assert_eq!(progress.total_rows(), Some(47));
    assert_eq!(progress.fraction(), Some(1.0));
    assert!(progress.bytes() > 0);
    assert!(progress.active_items().is_empty());
    assert_eq!(s.bytes, progress.bytes());
}

#[tokio::test]
async fn single_session_and_failures() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let items = vec![table("a"), table("missing"), table("c")];
    let mut j = job(dir.path(), items.clone());
    j.threads = 8;
    j.open_new_connections = false;
    let progress = Progress::shared();
    let s = run_export_job(j.clone(), factory.clone(), progress.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(factory.opened(), 1);
    assert_eq!(s.failed(), 1);
    assert!(s.items[1].error.as_deref().unwrap().contains("no such table"), "{:?}", s.items[1]);
    assert_eq!(s.items[1].file, None);
    assert_eq!(s.items[2].rows, 3);
    assert_eq!(progress.errors(), 1);
    assert!(progress.messages().iter().any(|m| m.level == MessageLevel::Error && m.item.as_deref() == Some("missing")));
    assert!(!dir.path().join("out/missing_2.csv").exists());

    // Stop at the first failure.
    j.continue_on_error = false;
    let err = run_export_job(j, factory, Progress::shared(), CancellationToken::new()).await.unwrap_err();
    assert!(err.full_text().contains("missing"), "{}", err.full_text());
}

#[tokio::test]
async fn sql_job_reads_table_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let mut j = job(dir.path(), vec![table("c")]);
    j.format = ExportFormat::Sql;
    j.file_name_pattern = "${table}.${extension}".into();
    j.options.sql.include_create_table = true;
    j.options.sql.mode = SqlInsertMode::Upsert;
    let s = run_export_job(j, factory, Progress::shared(), CancellationToken::new()).await.unwrap();
    assert_eq!(s.failed(), 0);
    let sql = std::fs::read_to_string(dir.path().join("out/c.sql")).unwrap();
    assert!(sql.starts_with("CREATE TABLE c (\n    id INTEGER,\n    v TEXT,\n    PRIMARY KEY (id)\n);"), "{sql}");
    assert!(sql.contains("ON CONFLICT (id) DO UPDATE SET v = excluded.v;"), "{sql}");
    // The script recreates the table elsewhere.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&sql).unwrap();
    assert_eq!(select(&conn, "SELECT count(*) FROM c"), vec![vec![Value::Int(3)]]);
}

#[tokio::test]
async fn sql_job_for_another_dialect() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let mut j = job(dir.path(), vec![table("c")]);
    j.format = ExportFormat::Sql;
    j.file_name_pattern = "${table}".into();
    j.options.sql.mode = SqlInsertMode::Upsert;
    // The source dialect has a native upsert; the ANSI target does not.
    let s = run_export_job_with(j, factory, Some(generic()), Progress::shared(), CancellationToken::new()).await.unwrap();
    assert_eq!(s.failed(), 0);
    let sql = std::fs::read_to_string(dir.path().join("out/c.sql")).unwrap();
    assert!(sql.starts_with("DELETE FROM c WHERE id = 0;\nINSERT INTO c (id, v) VALUES (0, 'c0');\n"), "{sql}");
    assert!(!sql.contains("ON CONFLICT"));
}

#[tokio::test]
async fn every_format_through_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    for format in ExportFormat::ALL {
        let mut j = job(dir.path(), vec![table("b")]);
        j.format = format;
        j.file_name_pattern = format!("${{table}}_{:?}", format);
        let s = run_export_job(j, factory.clone(), Progress::shared(), CancellationToken::new()).await.unwrap();
        assert_eq!(s.failed(), 0, "{format:?}: {:?}", s.items);
        assert_eq!(s.rows, 25, "{format:?}");
        let file = s.items[0].file.clone().unwrap();
        assert!(file.to_string_lossy().ends_with(&format!(".{}", format.extension())), "{file:?}");
        assert!(std::fs::metadata(&file).unwrap().len() > 0, "{format:?}");
    }
}

/// A source that never ends.
struct Endless {
    columns: Vec<ColumnMeta>,
    next: i64,
    batches: usize,
    cancel_after: Option<(usize, CancellationToken)>,
    hang_after: Option<usize>,
}

impl Endless {
    fn new() -> Self {
        Endless {
            columns: vec![ColumnMeta::new("n", "int8", DataKind::Int), ColumnMeta::new("s", "text", DataKind::Text)],
            next: 0,
            batches: 0,
            cancel_after: None,
            hang_after: None,
        }
    }
}

#[async_trait]
impl RowSource for Endless {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        self.batches += 1;
        if let Some((n, token)) = &self.cancel_after
            && self.batches == *n
        {
            token.cancel();
        }
        if self.hang_after.is_some_and(|n| self.batches > n) {
            std::future::pending::<()>().await;
        }
        let rows = (0..max)
            .map(|i| {
                let n = self.next + i as i64;
                vec![Value::Int(n), Value::text(format!("value {n}"))]
            })
            .collect();
        self.next += max as i64;
        Ok(rows)
    }
}

#[tokio::test]
async fn cancel_mid_stream_stops_within_one_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("endless.csv");
    let cancel = CancellationToken::new();
    let mut src = Endless::new();
    src.cancel_after = Some((3, cancel.clone()));
    let mut sink = create_sink(ExportFormat::Csv, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    let progress = Progress::new();
    let err = run_export_batched(&mut src, sink.as_mut(), 100, &progress, &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    // The batch during which the token was cancelled is the last one written.
    assert_eq!(src.batches, 3);
    assert!(progress.rows() <= 300, "{}", progress.rows());
    assert!(!path.exists(), "partial file must be removed");

    // keep_partial_file keeps what was written.
    let cancel = CancellationToken::new();
    let mut src = Endless::new();
    src.cancel_after = Some((2, cancel.clone()));
    let opts = ExportOptions { keep_partial_file: true, ..Default::default() };
    let mut sink = create_sink(ExportFormat::Csv, &opts, ExportTarget::path(&path), None).unwrap();
    let err = run_export_batched(&mut src, sink.as_mut(), 50, &Progress::new(), &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(path.exists());
    assert!(lines(&path) <= 101);
}

#[tokio::test]
async fn cancel_while_waiting_for_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hang.parquet");
    let cancel = CancellationToken::new();
    let mut src = Endless::new();
    src.hang_after = Some(2);
    let mut sink = create_sink(ExportFormat::Parquet, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    let canceller = {
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.cancel();
        })
    };
    let t0 = Instant::now();
    let err = run_export(&mut src, sink.as_mut(), &Progress::new(), &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(5));
    canceller.await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn cancel_a_running_job() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    database(&db);
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let items = vec![
        ExportItem::Query { sql: "SELECT /*endless*/ 1".into(), name: Some("endless".into()) },
        ExportItem::Query { sql: "SELECT /*endless*/ 2".into(), name: Some("endless2".into()) },
    ];
    let mut j = job(dir.path(), items);
    j.threads = 2;
    j.options.fetch_size = 500;
    let cancel = CancellationToken::new();
    let progress = Progress::shared();
    let watcher = {
        let (c, p) = (cancel.clone(), progress.clone());
        tokio::spawn(async move {
            while p.rows() < 5_000 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            c.cancel();
        })
    };
    let err = run_export_job(j, factory, progress.clone(), cancel).await.unwrap_err();
    watcher.await.unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(!dir.path().join("out/endless_1.csv").exists());
    assert!(!dir.path().join("out/endless2_2.csv").exists());
}

#[tokio::test]
async fn row_store_selection_export() {
    // "Use fetched rows / selected rows or columns" from a grid's row store.
    let store = RowStore::shared(1 << 20);
    store.lock().extend((0..1000).map(|i| vec![Value::Int(i), Value::text(format!("r{i}")), Value::Bool(i % 2 == 0)]));
    let columns = vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("label", "text", DataKind::Text), ColumnMeta::new("even", "bool", DataKind::Bool)];
    let mut src = RowStoreSource::new(store.clone(), columns.clone()).with_rows(vec![5, 1, 999]).with_columns(vec![2, 0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sel.csv");
    let mut sink = create_sink(ExportFormat::Csv, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    let s = run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows, 3);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "even,id\nfalse,5\nfalse,1\nfalse,999\n");
    // All fetched rows into a writer target (clipboard style).
    let buf = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    struct Shared(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut src = RowStoreSource::new(store, columns);
    let mut sink = create_sink(ExportFormat::Markdown, &ExportOptions::default(), ExportTarget::writer(Shared(buf.clone())), None).unwrap();
    let progress = Progress::new();
    let s = run_export(&mut src, sink.as_mut(), &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows, 1000);
    assert_eq!(progress.total_rows(), Some(1000));
    assert!(s.files.is_empty());
    let text = String::from_utf8(buf.lock().clone()).unwrap();
    assert_eq!(text.lines().count(), 1002);
}

#[tokio::test]
async fn row_store_with_spilled_pages() {
    // A small budget forces most pages of the grid's store to disk.
    let store = std::sync::Arc::new(parking_lot::Mutex::new(RowStore::new(64 * 1024).with_page_rows(100)));
    store.lock().extend((0..20_000).map(|i| vec![Value::Int(i), Value::text(format!("row {i} with some padding"))]));
    assert!(store.lock().spilled_pages() > 0);
    let columns = vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("label", "text", DataKind::Text)];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.csv");
    let mut src = RowStoreSource::new(store, columns);
    let mut sink = create_sink(ExportFormat::Csv, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    let s = run_export_batched(&mut src, sink.as_mut(), 333, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows, 20_000);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 20_001);
    assert_eq!(lines[12_346], "12345,row 12345 with some padding");
}

#[tokio::test]
async fn cursor_source_from_session() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    session.exec_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2), (3);");
    let mut src = CursorSource::execute(&mut session, "SELECT x FROM t WHERE x > ?", &[Value::Int(1)]).await.unwrap();
    assert_eq!(src.columns()[0].name, "x");
    let rows = drain(&mut src).await;
    assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
    src.close().await.unwrap();
    let err = CursorSource::execute(&mut session, "UPDATE t SET x = 0", &[]).await.err().unwrap();
    assert!(err.message.contains("did not return a result set"));
}
