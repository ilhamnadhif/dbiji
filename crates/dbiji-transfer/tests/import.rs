//! Import: reader edge cases and `run_import` into SQLite.

mod common;

use chrono::NaiveDate;
use common::*;
use dbiji_core::{DataKind, ErrorKind, ObjectRef, Session, Value};
use dbiji_transfer::*;
use std::io::Cursor;

fn csv(text: &[u8], opts: CsvReadOptions) -> CsvSource {
    CsvSource::from_reader(Cursor::new(text.to_vec()), opts).unwrap()
}

#[tokio::test]
async fn csv_encodings_and_bom() {
    // windows-1252 without BOM is detected.
    let mut s = csv(b"name\ncaf\xe9\n", CsvReadOptions::default());
    assert_eq!(s.encoding(), "windows-1252");
    assert_eq!(drain(&mut s).await, vec![vec![Value::text("café")]]);
    // UTF-8 BOM is removed from the first header name.
    let mut s = csv(b"\xEF\xBB\xBFid,name\n1,x\n", CsvReadOptions::default());
    assert_eq!(s.columns()[0].name, "id");
    assert_eq!(drain(&mut s).await.len(), 1);
    // UTF-16LE with BOM.
    let mut bytes = vec![0xFF, 0xFE];
    for u in "a;b\n1;ü\n".encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let mut s = csv(&bytes, CsvReadOptions { delimiter: ";".into(), ..Default::default() });
    assert_eq!(s.encoding(), "UTF-16LE");
    assert_eq!(drain(&mut s).await, vec![vec![Value::text("1"), Value::text("ü")]]);
    // Explicit legacy encoding.
    let mut s = csv(b"v\n\xb3\n", CsvReadOptions { encoding: "ISO-8859-2".into(), ..Default::default() });
    assert_eq!(drain(&mut s).await, vec![vec![Value::text("ł")]]);
    // Invalid UTF-8 in a declared UTF-8 file is replaced and reported.
    let mut s = csv(b"v\nok\xff\n", CsvReadOptions { encoding: "utf-8".into(), ..Default::default() });
    assert_eq!(drain(&mut s).await, vec![vec![Value::text("ok\u{FFFD}")]]);
    assert!(s.take_warnings().iter().any(|w| w.contains("invalid")));
}

#[tokio::test]
async fn csv_ragged_rows_and_header_modes() {
    let data = b"a,b\n1\n2,3,4\n";
    let mut s = csv(data, CsvReadOptions::default());
    assert_eq!(s.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>(), vec!["a", "b", "column3"]);
    let rows = drain(&mut s).await;
    assert_eq!(rows[0], vec![Value::text("1"), Value::Null, Value::Null]);
    assert_eq!(rows[1], vec![Value::text("2"), Value::text("3"), Value::text("4")]);
    let w = s.take_warnings();
    assert!(w.iter().any(|x| x.contains("1 record(s)")), "{w:?}");
    // Rows longer than the sample width are truncated (and counted).
    let mut long = String::from("a,b\n");
    for _ in 0..5 {
        long.push_str("1,2\n");
    }
    long.push_str("1,2,3\n");
    let mut s = csv(long.as_bytes(), CsvReadOptions { sample_rows: 2, ..Default::default() });
    let rows = drain(&mut s).await;
    assert_eq!(rows.last().unwrap().len(), 2);
    assert!(!s.take_warnings().is_empty());
    // No header.
    let mut s = csv(b"x,y\n", CsvReadOptions { header: CsvHeader::None, ..Default::default() });
    assert_eq!(s.columns()[1].name, "column2");
    assert_eq!(drain(&mut s).await, vec![vec![Value::text("x"), Value::text("y")]]);
    // Auto detection.
    let s = csv(b"name,city\nAnn,Paris\nBob,Rome\n", CsvReadOptions { header: CsvHeader::Auto, ..Default::default() });
    assert_eq!(s.columns()[1].name, "city");
    let s = csv(b"2024-01-01,5\n2024-01-02,6\n", CsvReadOptions { header: CsvHeader::Auto, ..Default::default() });
    assert_eq!(s.columns()[0].name, "column1");
    // Duplicate and empty header names are made unique.
    let s = csv(b"a,a,\n1,2,3\n", CsvReadOptions::default());
    assert_eq!(s.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>(), vec!["a", "a_2", "column3"]);
}

#[tokio::test]
async fn csv_skip_comment_trim_null() {
    let data = b"junk line\n# comment\nid ; name ; note\n 1 ; \"  spaced  \" ; NULL\n# another\n2;x;\n";
    let opts = CsvReadOptions {
        delimiter: ";".into(),
        skip_rows: 1,
        comment_char: Some('#'),
        trim: true,
        null_string: "NULL".into(),
        ..Default::default()
    };
    let mut s = csv(data, opts);
    assert_eq!(s.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>(), vec!["id", "name", "note"]);
    let rows = drain(&mut s).await;
    assert_eq!(rows[0], vec![Value::text("1"), Value::text("  spaced  "), Value::Null]);
    // With a non-empty NULL marker an empty unquoted field is an empty text.
    assert_eq!(rows[1], vec![Value::text("2"), Value::text("x"), Value::text("")]);
    // Non-ASCII quote characters are rejected.
    let r = CsvSource::from_reader(
        Cursor::new(b"a".to_vec()),
        CsvReadOptions { quote_char: Some('«'), ..Default::default() },
    );
    assert!(r.is_err());
}

fn people_csv() -> &'static [u8] {
    b"id,name,price,joined,active\n1,Ann,12.50,2024-01-31,true\n2,Bob,7,2023-12-01,false\n3,Cid,,2022-02-02,true\n"
}

#[tokio::test]
async fn import_creates_table_from_inferred_types() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    let mut src = PeekableSource::new(csv(people_csv(), CsvReadOptions::default()));
    let columns = src.columns().to_vec();
    let sample = src.peek(100).await.unwrap().to_vec();
    let inferred = infer_columns(&columns, &sample, &InferOptions::default());
    let kinds: Vec<DataKind> = inferred.iter().map(|c| c.kind).collect();
    assert_eq!(kinds, vec![DataKind::Int, DataKind::Text, DataKind::Decimal, DataKind::Date, DataKind::Bool]);
    let dialect = sqlite_dialect();
    let mapping = ImportMapping::from_inferred(ObjectRef::table(None, "people"), &inferred, dialect.as_ref());
    assert_eq!(mapping.columns[2].target_type, "NUMERIC(4,2)");
    assert_eq!(mapping.columns[1].target_type, "VARCHAR(10)");
    let progress = Progress::new();
    let summary = run_import(&mut src, &mut session, &mapping, &progress, &CancellationToken::new()).await.unwrap();
    assert!(summary.created_table);
    assert_eq!((summary.rows_read, summary.rows_written, summary.rows_failed), (3, 3, 0));
    assert_eq!(progress.rows(), 3);
    let types: Vec<String> = select(&session.conn, "SELECT type FROM pragma_table_info('people')")
        .into_iter()
        .map(|r| r[0].to_text().into_owned())
        .collect();
    assert_eq!(types, vec!["INTEGER", "VARCHAR(10)", "NUMERIC(4,2)", "DATE", "BOOLEAN"]);
    let rows = select(&session.conn, "SELECT id, name, joined, active FROM people ORDER BY id");
    assert_eq!(rows[0], vec![Value::Int(1), Value::text("Ann"), Value::text("2024-01-31"), Value::Int(1)]);
    assert_eq!(select(&session.conn, "SELECT price FROM people WHERE id = 3"), vec![vec![Value::Null]]);
}

fn customers(session: &SqliteSession) {
    session.exec_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER CHECK (age >= 0), joined TEXT);
         INSERT INTO customers VALUES (1, 'old', 1, NULL);",
    );
}

fn customer_mapping(mode: ImportMode) -> ImportMapping {
    let mut m = ImportMapping::new(ObjectRef::table(None, "customers"));
    m.columns = vec![
        ColumnMapping::new(0, "id").typed("INTEGER", DataKind::Int),
        ColumnMapping::new(1, "name").typed("TEXT", DataKind::Text),
        ColumnMapping::new(2, "age").typed("INTEGER", DataKind::Int),
        ColumnMapping::new(3, "joined").typed("DATE", DataKind::Date),
    ];
    m.mode = mode;
    m.key_columns = vec!["id".into()];
    m.convert.date_formats = vec!["%d/%m/%Y".into()];
    m
}

const CUSTOMERS: &[u8] = b"id,name,age,joined\n1,Ann,30,31/01/2024\n2,Bob,40,2024-02-01\n3,Cid,x,\n4,Dan,50,\n5,,60,\n6,Eve,-1,\n7,Fay,70,\n";

#[tokio::test]
async fn import_modes() {
    // Append into an existing table: id 1 collides with the primary key.
    let mut session = SqliteSession::memory(sqlite_dialect());
    customers(&session);
    let mut m = customer_mapping(ImportMode::Append);
    m.on_error = OnError::SkipRow;
    m.batch_size = 3;
    m.rows_per_statement = 2;
    let mut src = csv(CUSTOMERS, CsvReadOptions::default());
    let s = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap();
    // Row 1: duplicate key, row 3: "x" is not an integer, row 5: NULL name, row 6: CHECK violation.
    let failed: Vec<u64> = s.errors.iter().map(|e| e.row).collect();
    assert_eq!(failed, vec![3, 1, 5, 6], "{:?}", s.errors);
    assert_eq!(s.errors[0].column.as_deref(), Some("age"));
    assert_eq!((s.rows_read, s.rows_written, s.rows_failed), (7, 3, 4));
    let ids: Vec<Value> =
        select(&session.conn, "SELECT id FROM customers ORDER BY id").into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(4), Value::Int(7)]);
    assert_eq!(
        select(&session.conn, "SELECT joined FROM customers WHERE id = 2"),
        vec![vec![Value::text("2024-02-01")]]
    );
    // Savepoints were used to isolate the failing rows inside the batch transactions.
    assert!(session.log.lock().iter().any(|s| s.starts_with("ROLLBACK TO SAVEPOINT dbiji_row")));

    // Truncate first.
    let mut m = customer_mapping(ImportMode::TruncateFirst);
    m.on_error = OnError::SkipRow;
    let mut src = csv(CUSTOMERS, CsvReadOptions::default());
    let s = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_written, 4);
    assert_eq!(select(&session.conn, "SELECT count(*) FROM customers"), vec![vec![Value::Int(4)]]);
    assert_eq!(
        select(&session.conn, "SELECT joined FROM customers WHERE id = 1"),
        vec![vec![Value::text("2024-01-31")]]
    );

    // Replace through the dialect's upsert, then through DELETE + INSERT.
    for dialect in [sqlite_dialect(), generic()] {
        let mut session = SqliteSession::memory(dialect.clone());
        customers(&session);
        let mut m = customer_mapping(ImportMode::Replace);
        m.on_error = OnError::SkipRow;
        let mut src = csv(CUSTOMERS, CsvReadOptions::default());
        let s = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap();
        assert_eq!(s.rows_written, 4, "{:?}", s.errors);
        assert_eq!(select(&session.conn, "SELECT name FROM customers WHERE id = 1"), vec![vec![Value::text("Ann")]]);
        let log = session.log.lock().clone();
        if dialect.kind() == dbiji_core::DbKind::Generic {
            assert!(log.iter().any(|s| s.starts_with("DELETE FROM customers WHERE (id = ?)")), "{log:?}");
        } else {
            assert!(log.iter().any(|s| s.contains("ON CONFLICT (id) DO UPDATE")), "{log:?}");
        }
    }
}

#[tokio::test]
async fn import_abort_and_transactions() {
    // Whole transaction: an error rolls everything back.
    let mut session = SqliteSession::memory(sqlite_dialect());
    customers(&session);
    let mut m = customer_mapping(ImportMode::TruncateFirst);
    m.transaction = TransactionScope::Whole;
    m.batch_size = 1;
    let mut src = csv(CUSTOMERS, CsvReadOptions::default());
    let err = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.message.contains("Row 3, column age"), "{}", err.full_text());
    // The truncate was rolled back too.
    assert_eq!(select(&session.conn, "SELECT name FROM customers"), vec![vec![Value::text("old")]]);
    assert!(!session.in_transaction_now());

    // Per batch: committed batches stay.
    let mut session = SqliteSession::memory(sqlite_dialect());
    customers(&session);
    let mut m = customer_mapping(ImportMode::Append);
    m.batch_size = 1;
    let data = b"id,name,age,joined\n10,A,1,\n11,B,2,\n12,,3,\n13,D,4,\n";
    let mut src = csv(data, CsvReadOptions::default());
    let err = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.full_text().contains("Row 3"), "{}", err.full_text());
    assert_eq!(select(&session.conn, "SELECT count(*) FROM customers"), vec![vec![Value::Int(3)]]);

    // Auto-commit statements with continue on error.
    let mut session = SqliteSession::memory(sqlite_dialect());
    customers(&session);
    let mut m = customer_mapping(ImportMode::Append);
    m.transaction = TransactionScope::None;
    m.on_error = OnError::LogAndContinue;
    let progress = Progress::new();
    let mut src = csv(data, CsvReadOptions::default());
    let s = run_import(&mut src, &mut session, &m, &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!((s.rows_written, s.rows_failed), (3, 1));
    assert!(progress.messages().iter().any(|msg| msg.level == MessageLevel::Error && msg.text.starts_with("Row 3")));
    assert_eq!(progress.errors(), 1);

    // Too many errors.
    let mut session = SqliteSession::memory(sqlite_dialect());
    customers(&session);
    let mut m = customer_mapping(ImportMode::Append);
    m.on_error = OnError::SkipRow;
    m.max_errors = Some(1);
    m.batch_size = 2;
    let mut src = csv(CUSTOMERS, CsvReadOptions::default());
    let err = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.message.contains("stopped after 2 failed rows"), "{}", err.message);
}

trait InTxn {
    fn in_transaction_now(&self) -> bool;
}

impl InTxn for SqliteSession {
    fn in_transaction_now(&self) -> bool {
        !self.conn.is_autocommit()
    }
}

#[tokio::test]
async fn import_cancellation_rolls_back() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    session.exec_batch("CREATE TABLE n (v INTEGER)");
    let rows: Vec<dbiji_core::Row> = (0..10_000).map(|i| vec![Value::Int(i)]).collect();
    // A source that cancels the token after the second batch.
    struct Cancelling {
        inner: VecSource,
        cancel: CancellationToken,
        batches: usize,
    }
    #[async_trait::async_trait]
    impl RowSource for Cancelling {
        fn columns(&self) -> &[dbiji_core::ColumnMeta] {
            self.inner.columns()
        }
        async fn next_batch(&mut self, max: usize) -> dbiji_core::DbResult<Vec<dbiji_core::Row>> {
            self.batches += 1;
            if self.batches == 3 {
                self.cancel.cancel();
            }
            self.inner.next_batch(max).await
        }
    }
    let cancel = CancellationToken::new();
    let mut src = Cancelling {
        inner: VecSource::new(vec![dbiji_core::ColumnMeta::new("v", "int", DataKind::Int)], rows),
        cancel: cancel.clone(),
        batches: 0,
    };
    let mut m = ImportMapping::new(ObjectRef::table(None, "n"));
    m.columns = vec![ColumnMapping::new(0, "v")];
    m.batch_size = 100;
    m.transaction = TransactionScope::Whole;
    let err = run_import(&mut src, &mut session, &m, &Progress::new(), &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(select(&session.conn, "SELECT count(*) FROM n"), vec![vec![Value::Int(0)]]);
    assert!(src.batches <= 3);
}

#[tokio::test]
async fn import_validates_mapping() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    let mut src = csv(people_csv(), CsvReadOptions::default());
    let mut m = ImportMapping::new(ObjectRef::table(None, "x"));
    let e = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(e.message.contains("No column"));
    m.columns = vec![ColumnMapping::new(9, "a")];
    let mut src = csv(people_csv(), CsvReadOptions::default());
    let e = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(e.message.contains("source column 10"), "{}", e.message);
    m.columns = vec![ColumnMapping::new(0, "a"), ColumnMapping::new(1, "A")];
    let mut src = csv(people_csv(), CsvReadOptions::default());
    let e = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(e.message.contains("mapped twice"));
    m.columns = vec![ColumnMapping::new(0, "a")];
    m.mode = ImportMode::Replace;
    let mut src = csv(people_csv(), CsvReadOptions::default());
    let e = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(e.message.contains("key columns"));
}

#[tokio::test]
async fn xlsx_import_typed_cells() {
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("in.xlsx");
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("People").unwrap();
    let date = Format::new().set_num_format("yyyy-mm-dd");
    let datetime = Format::new().set_num_format("yyyy-mm-dd hh:mm");
    // The table starts at B3 after a title row.
    ws.write_string(0, 0, "Report title").unwrap();
    for (c, h) in ["id", "name", "joined", "seen", "active", "score"].iter().enumerate() {
        ws.write_string(2, c as u16 + 1, *h).unwrap();
    }
    ws.write_number(3, 1, 1).unwrap();
    ws.write_string(3, 2, "Ann").unwrap();
    ws.write_datetime_with_format(3, 3, ExcelDateTime::from_ymd(2024, 1, 31).unwrap(), &date).unwrap();
    ws.write_datetime_with_format(
        3,
        4,
        ExcelDateTime::from_ymd(2024, 1, 31).unwrap().and_hms(10, 30, 0).unwrap(),
        &datetime,
    )
    .unwrap();
    ws.write_boolean(3, 5, true).unwrap();
    ws.write_number(3, 6, 9.75).unwrap();
    // Row 5 is empty and skipped; row 6 has gaps.
    ws.write_number(5, 1, 2).unwrap();
    ws.write_string(5, 2, "Bob").unwrap();
    let other = wb.add_worksheet();
    other.set_name("Other").unwrap();
    other.write_string(0, 0, "x").unwrap();
    wb.save(&path).unwrap();

    assert_eq!(xlsx_sheet_names(&path).unwrap(), vec!["People".to_string(), "Other".into()]);
    let opts = XlsxReadOptions { sheet: SheetSelector::Name("people".into()), skip_rows: 1, ..Default::default() };
    let mut src = XlsxSource::open(&path, opts).await.unwrap();
    let names: Vec<String> = src.columns().iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, vec!["id", "name", "joined", "seen", "active", "score"]);
    let rows = drain(&mut src).await;
    assert_eq!(rows.len(), 2);
    let d = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Float(1.0),
            Value::text("Ann"),
            Value::Date(d),
            Value::DateTime(d.and_hms_opt(10, 30, 0).unwrap()),
            Value::Bool(true),
            Value::Float(9.75),
        ]
    );
    assert_eq!(
        rows[1],
        vec![Value::Float(2.0), Value::text("Bob"), Value::Null, Value::Null, Value::Null, Value::Null]
    );
    let inferred = infer_columns(src.columns(), &rows, &InferOptions::default());
    assert_eq!(
        inferred.iter().map(|c| c.kind).collect::<Vec<_>>(),
        vec![DataKind::Int, DataKind::Text, DataKind::Date, DataKind::DateTime, DataKind::Bool, DataKind::Float]
    );
    // Missing sheets are reported.
    let bad = XlsxSource::open(&path, XlsxReadOptions { sheet: SheetSelector::Index(5), ..Default::default() }).await;
    assert!(bad.is_err());
    // Without a header row.
    let opts = XlsxReadOptions { sheet: SheetSelector::Index(1), header: false, ..Default::default() };
    let mut src = XlsxSource::open(&path, opts).await.unwrap();
    assert_eq!(src.columns()[0].name, "column1");
    assert_eq!(drain(&mut src).await, vec![vec![Value::text("x")]]);
}

#[tokio::test]
async fn xml_and_json_import_into_sqlite() {
    let xml = r#"<?xml version="1.0"?>
<orders>
  <order id="1"><customer>Ann</customer><total>9.50</total><placed>2024-01-02 10:00:00</placed></order>
  <order id="2"><customer>Bob &amp; Co</customer><total null="true"/><placed>2024-01-03 11:30:00</placed></order>
</orders>"#;
    let mut session = SqliteSession::memory(sqlite_dialect());
    let mut src = PeekableSource::new(
        XmlSource::from_reader(Cursor::new(xml.as_bytes().to_vec()), XmlReadOptions::default()).unwrap(),
    );
    let cols = src.columns().to_vec();
    let sample = src.peek(10).await.unwrap().to_vec();
    let inferred = infer_columns(&cols, &sample, &InferOptions::default());
    let m = ImportMapping::from_inferred(ObjectRef::table(None, "orders"), &inferred, session.dialect().as_ref());
    let s = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_written, 2);
    assert_eq!(
        select(&session.conn, "SELECT id, customer, total, placed FROM orders ORDER BY id"),
        // NUMERIC(3,2) has numeric affinity in SQLite, which stores 9.50 as REAL.
        vec![
            vec![Value::Int(1), Value::text("Ann"), Value::Float(9.5), Value::text("2024-01-02 10:00:00")],
            vec![Value::Int(2), Value::text("Bob & Co"), Value::Null, Value::text("2024-01-03 11:30:00")],
        ]
    );
    assert_eq!(m.columns[2].target_type, "NUMERIC(3,2)");

    let lines = "{\"id\": 1, \"tags\": [\"a\", \"b\"], \"price\": 12345678901234567890.25}\n{\"id\": 2, \"tags\": null, \"price\": 1}\n";
    let mut src = JsonSource::from_reader(Cursor::new(lines.as_bytes().to_vec()), JsonReadOptions::default()).unwrap();
    session.exec_batch("CREATE TABLE items (id INTEGER, tags TEXT, price TEXT)");
    let def = session.table_definition(&ObjectRef::table(None, "items")).await.unwrap();
    let mut m = ImportMapping::match_table(src.columns(), &def);
    m.columns[2].target_kind = Some(DataKind::Decimal);
    let s = run_import(&mut src, &mut session, &m, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_written, 2);
    assert_eq!(
        select(&session.conn, "SELECT * FROM items ORDER BY id"),
        vec![
            vec![Value::Int(1), Value::text("[\"a\", \"b\"]"), Value::text("12345678901234567890.25")],
            vec![Value::Int(2), Value::Null, Value::text("1")],
        ]
    );
}

#[tokio::test]
async fn saved_import_job_runs_with_automatic_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    let file = dir.path().join("people.csv");
    std::fs::write(&file, people_csv()).unwrap();
    let factory = SqliteFactory::new(&db, sqlite_dialect());
    let job = ImportJob {
        source: ImportSourceConfig::Csv { path: file.clone(), options: CsvReadOptions::default() },
        mapping: ImportMapping { create_table: true, ..ImportMapping::new(ObjectRef::table(None, "people")) },
        sample_rows: 50,
        ..Default::default()
    };
    // Saved as JSON and loaded back like a task would be.
    let saved = serde_json::to_string(&job).unwrap();
    let job: ImportJob = serde_json::from_str(&saved).unwrap();
    let progress = Progress::shared();
    let s = run_import_job(job, factory.clone(), progress.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_written, 3);
    assert_eq!(progress.items_done(), 1);
    // A second run appends into the now existing table, mapping by name.
    std::fs::write(&file, b"name,id\nZed,9\n").unwrap();
    let job = ImportJob {
        source: ImportSourceConfig::Csv { path: file, options: CsvReadOptions::default() },
        mapping: ImportMapping::new(ObjectRef::table(None, "people")),
        ..Default::default()
    };
    let s = run_import_job(job, factory, Progress::shared(), CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_written, 1);
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(select(&conn, "SELECT name FROM people WHERE id = 9"), vec![vec![Value::text("Zed")]]);
}
