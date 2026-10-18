//! Execution, cursor protocol, values, errors, cancellation, transactions,
//! concurrency, read-only mode and backups.

mod common;

use chrono::{FixedOffset, NaiveDate, NaiveTime};
use common::*;
use dbiji_core::driver::{execute_update, query_all};
use dbiji_core::value::{ColumnSource, DataKind};
use dbiji_core::{ErrorKind, ExecOptions, ResultHeader, Session, TransactionMode, Value};
use dbiji_driver_sqlite::{
    BackupProgress, SqliteDriver, online_backup, online_backup_with_progress, vacuum_into_sql,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const COUNT_TO_A_BILLION: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000000) SELECT count(*) FROM c";
const ENDLESS: &str =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c";

fn opts() -> ExecOptions {
    ExecOptions::default()
}

#[tokio::test]
async fn connects_and_reports_version() {
    let mut s = memory().await;
    assert_eq!(s.server_info().product, "SQLite");
    assert!(s.server_info().at_least(3, 45), "{:?}", s.server_info());
    assert_eq!(s.kind(), dbiji_core::DbKind::Sqlite);
    s.ping().await.unwrap();
    assert!(!s.is_closed());
    assert_eq!(s.current_schema().await.unwrap().as_deref(), Some("main"));
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some(":memory:")
    );
    assert_eq!(s.list_databases().await.unwrap().len(), 1);
    s.set_current_schema("main").await.unwrap();
    assert_eq!(
        s.set_current_schema("aux").await.unwrap_err().kind,
        ErrorKind::Unsupported
    );
    assert_eq!(s.dialect().placeholder(1), "?");
    s.close().await.unwrap();
    assert!(s.is_closed());
    assert_eq!(s.ping().await.unwrap_err().kind, ErrorKind::Connection);
    assert_eq!(
        s.execute("SELECT 1", &[], &opts())
            .await
            .err()
            .map(|e| e.kind),
        Some(ErrorKind::Connection)
    );
    // closing twice is fine
    s.close().await.unwrap();
}

#[tokio::test]
async fn creates_missing_files_and_reports_open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "new.db");
    let mut s = connect(&path).await;
    exec(&mut s, "CREATE TABLE t(a)").await;
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("new.db")
    );
    close(s).await;
    assert!(Path::new(&path).exists());

    let driver = SqliteDriver::new();
    let missing = file_in(dir.path(), "missing.db");
    let err = driver
        .connect_sqlite(&params(&missing, &[("read_only", "true")]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Io);
    assert!(err.message.contains("does not exist"), "{err:?}");
    let err = driver
        .connect_sqlite(&params(&missing, &[("create", "false")]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Io);
    assert!(!Path::new(&missing).exists());
    let nested = file_in(dir.path(), "no/such/dir/x.db");
    let err = driver
        .connect_sqlite(&params(&nested, &[]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Io);
    assert!(err.message.contains("directory does not exist"), "{err:?}");
    let garbage = file_in(dir.path(), "garbage.db");
    std::fs::write(&garbage, vec![b'x'; 4096]).unwrap();
    let err = driver
        .connect_sqlite(&params(&garbage, &[]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Io);
    assert!(
        err.message.to_lowercase().contains("not a database"),
        "{err:?}"
    );
    let err = driver
        .connect_sqlite(&params(&path, &[("journal_mode", "sideways")]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Config);
}

#[tokio::test]
async fn connection_properties_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "props.db");
    let mut s = connect_with(
        &path,
        &[
            ("journal_mode", "WAL"),
            ("synchronous", "NORMAL"),
            ("cache_size", "-4096"),
            ("foreign_keys", "false"),
            ("pragma.temp_store", "MEMORY"),
        ],
    )
    .await;
    assert_eq!(
        scalar(&mut s, "PRAGMA journal_mode").await,
        Value::text("wal")
    );
    assert_eq!(int(&mut s, "PRAGMA synchronous").await, 1);
    assert_eq!(int(&mut s, "PRAGMA cache_size").await, -4096);
    assert_eq!(int(&mut s, "PRAGMA foreign_keys").await, 0);
    assert_eq!(int(&mut s, "PRAGMA temp_store").await, 2);
    close(s).await;
    let mut s = connect(&path).await;
    assert_eq!(
        int(&mut s, "PRAGMA foreign_keys").await,
        1,
        "foreign keys are on by default"
    );
}

#[tokio::test]
async fn values_round_trip() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE v(i INTEGER, r REAL, t TEXT, b BLOB, n NUMERIC, flag BOOLEAN, d DATE, dt DATETIME, \
         ts TIMESTAMP, tm TIME, j JSON, u UUID, dec DECIMAL(10,2), name VARCHAR(20), untyped)",
    )
    .await;
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let dt = date.and_hms_opt(13, 14, 15).unwrap();
    let tz = dt
        .and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap())
        .unwrap();
    let time = NaiveTime::from_hms_opt(23, 59, 58).unwrap();
    let u = uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
    let params = vec![
        Value::Int(i64::MIN),
        Value::Float(1.5),
        Value::text("héllo wörld ✓"),
        Value::bytes(vec![0u8, 1, 255]),
        Value::decimal("12.5"),
        Value::Bool(true),
        Value::Date(date),
        Value::DateTime(dt),
        Value::DateTimeTz(tz),
        Value::Time(time),
        Value::json(r#"{"a":[1,2]}"#),
        Value::Uuid(u),
        Value::decimal("10.25"),
        Value::text("short"),
        Value::Null,
    ];
    let n = execute_update(
        &mut s,
        "INSERT INTO v VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        &params,
    )
    .await
    .unwrap();
    assert_eq!(n, Some(1));
    let (cols, rows) = query_all(&mut s, "SELECT * FROM v", &[]).await.unwrap();
    let r = &rows[0];
    assert_eq!(r[0], Value::Int(i64::MIN));
    assert_eq!(r[1], Value::Float(1.5));
    assert_eq!(r[2], Value::text("héllo wörld ✓"));
    assert_eq!(r[3], Value::bytes(vec![0u8, 1, 255]));
    assert_eq!(
        r[4],
        Value::Float(12.5),
        "NUMERIC affinity stores 12.5 as REAL"
    );
    assert_eq!(r[5], Value::Bool(true));
    assert_eq!(r[6], Value::Date(date));
    assert_eq!(r[7], Value::DateTime(dt));
    assert_eq!(
        r[8],
        Value::text("2024-02-29 13:14:15+07:00"),
        "offsets are kept as text"
    );
    assert_eq!(r[9], Value::Time(time));
    assert_eq!(r[10], Value::json(r#"{"a":[1,2]}"#));
    assert_eq!(r[11], Value::Uuid(u));
    assert_eq!(r[12], Value::Float(10.25));
    assert_eq!(r[13], Value::text("short"));
    assert_eq!(r[14], Value::Null);

    let kinds: Vec<DataKind> = cols.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            DataKind::Int,
            DataKind::Float,
            DataKind::Text,
            DataKind::Binary,
            DataKind::Decimal,
            DataKind::Bool,
            DataKind::Date,
            DataKind::DateTime,
            DataKind::DateTime,
            DataKind::Time,
            DataKind::Json,
            DataKind::Uuid,
            DataKind::Decimal,
            DataKind::Text,
            DataKind::Other,
        ]
    );
    assert_eq!(cols[12].type_name, "DECIMAL(10,2)");
    assert_eq!((cols[12].precision, cols[12].scale), (Some(10), Some(2)));
    assert_eq!(cols[13].max_length, Some(20));
    assert_eq!(
        cols[2].source,
        Some(ColumnSource {
            database: None,
            schema: Some("main".into()),
            table: "v".into(),
            column: "t".into()
        })
    );

    // non canonical texts stay text; integer booleans other than 0/1 stay integers
    exec(&mut s, "DELETE FROM v").await;
    exec(&mut s, "INSERT INTO v(flag, d, dt, j, u) VALUES (2, '2024-1-2', '2024-01-02T03:04:05', 'not json', 'ABC')").await;
    let r = &rows_of(&mut s, "SELECT flag, d, dt, j, u FROM v").await[0];
    assert_eq!(
        r,
        &vec![
            Value::Int(2),
            Value::text("2024-1-2"),
            Value::text("2024-01-02T03:04:05"),
            Value::text("not json"),
            Value::text("ABC")
        ]
    );

    // expressions get their kind from the first value
    let (cols, rows) = query_all(
        &mut s,
        "SELECT 1 AS a, 'x' AS b, 2.5 AS c, x'00' AS d, NULL AS e",
        &[],
    )
    .await
    .unwrap();
    let described: Vec<(DataKind, &str)> = cols
        .iter()
        .map(|c| (c.kind, c.type_name.as_str()))
        .collect();
    assert_eq!(
        described,
        vec![
            (DataKind::Int, "INTEGER"),
            (DataKind::Text, "TEXT"),
            (DataKind::Float, "REAL"),
            (DataKind::Binary, "BLOB"),
            (DataKind::Other, "")
        ]
    );
    assert!(cols.iter().all(|c| c.source.is_none()));
    assert_eq!(rows[0][3], Value::bytes(vec![0u8]));

    // special numbers
    let r = &query_all(
        &mut s,
        "SELECT ?, ?, ?, ?",
        &[
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
            Value::UInt(u64::MAX),
            Value::Int(i64::MAX),
        ],
    )
    .await
    .unwrap()
    .1[0];
    assert_eq!(r[0], Value::Float(f64::INFINITY));
    assert_eq!(r[1], Value::Null, "SQLite stores NaN as NULL");
    assert_eq!(r[2], Value::text(u64::MAX.to_string()));
    assert_eq!(r[3], Value::Int(i64::MAX));
    let empty = &query_all(
        &mut s,
        "SELECT ?, ?",
        &[Value::bytes(Vec::new()), Value::text("")],
    )
    .await
    .unwrap()
    .1[0];
    assert_eq!(empty, &vec![Value::bytes(Vec::new()), Value::text("")]);
    let arr = Value::Array(vec![Value::Int(1), Value::text("a")].into_boxed_slice());
    assert_eq!(
        scalar_p(&mut s, "SELECT json_extract(?, '$[1]')", &[arr]).await,
        Value::text("a")
    );
}

async fn rows_of(s: &mut dbiji_driver_sqlite::SqliteSession, sql: &str) -> Vec<Vec<Value>> {
    rows(s, sql).await
}

async fn scalar_p(
    s: &mut dbiji_driver_sqlite::SqliteSession,
    sql: &str,
    params: &[Value],
) -> Value {
    query_all(s, sql, params)
        .await
        .unwrap()
        .1
        .remove(0)
        .remove(0)
}

#[tokio::test]
async fn result_columns_resolve_their_source() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL, note)",
    )
    .await;
    exec(&mut s, "CREATE TABLE logs(msg TEXT)").await;
    exec(
        &mut s,
        "CREATE TABLE seq(id INTEGER PRIMARY KEY AUTOINCREMENT, x)",
    )
    .await;
    exec(&mut s, "INSERT INTO users(name) VALUES ('a')").await;
    exec(&mut s, "INSERT INTO logs VALUES ('m')").await;
    let o = ExecOptions {
        resolve_sources: true,
        ..Default::default()
    };
    let mut c = s
        .execute(
            "SELECT u.id, u.name, u.note, u.id + 1 AS calc FROM users u",
            &[],
            &o,
        )
        .await
        .unwrap();
    let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
        panic!()
    };
    c.close().await.unwrap();
    assert_eq!(
        cols[0]
            .source
            .as_ref()
            .map(|s| (s.schema.as_deref(), s.table.as_str(), s.column.as_str())),
        Some((Some("main"), "users", "id"))
    );
    assert_eq!(
        cols[0].auto_increment,
        Some(true),
        "INTEGER PRIMARY KEY is assigned automatically"
    );
    assert_eq!(cols[1].nullable, Some(false));
    assert_eq!(cols[1].auto_increment, Some(false));
    assert_eq!(cols[2].nullable, Some(true));
    assert!(cols[3].source.is_none());
    let (cols, _) = query_all(&mut s, "SELECT rowid, msg FROM logs", &[])
        .await
        .unwrap();
    assert_eq!(
        cols[0].source.as_ref().map(|s| s.table.as_str()),
        Some("logs")
    );
    assert_eq!(cols[0].nullable, Some(false));
    let (cols, _) = query_all(&mut s, "SELECT id FROM seq", &[]).await.unwrap();
    assert_eq!(cols[0].auto_increment, Some(true));
}

#[tokio::test]
async fn multi_statement_results_follow_the_cursor_protocol() {
    let mut s = memory().await;
    let sql = "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);\n\
               INSERT INTO t(v) VALUES ('a'), ('b');\n\
               -- a comment between statements\n\
               SELECT id, v FROM t ORDER BY id;\n\
               UPDATE t SET v = 'c' WHERE id = 2;\n\
               DELETE FROM t WHERE id = 99;";
    let mut c = s.execute(sql, &[], &opts()).await.unwrap();
    match c.next_result().await.unwrap() {
        Some(ResultHeader::Update {
            rows_affected: None,
            tag: Some(tag),
            ..
        }) => assert_eq!(tag, "CREATE TABLE"),
        other => panic!("{other:?}"),
    }
    assert!(c.fetch(10).await.unwrap().is_empty());
    match c.next_result().await.unwrap() {
        Some(ResultHeader::Update {
            rows_affected: Some(2),
            last_insert_id: Some(2),
            tag: Some(tag),
        }) => assert_eq!(tag, "INSERT"),
        other => panic!("{other:?}"),
    }
    let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
        panic!("rows expected")
    };
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["id", "v"]
    );
    assert_eq!(
        c.fetch(1).await.unwrap(),
        vec![vec![Value::Int(1), Value::text("a")]]
    );
    assert_eq!(
        c.fetch(10).await.unwrap(),
        vec![vec![Value::Int(2), Value::text("b")]]
    );
    assert!(c.fetch(10).await.unwrap().is_empty());
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update {
            rows_affected: Some(1),
            ..
        })
    ));
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update {
            rows_affected: Some(0),
            last_insert_id: None,
            ..
        })
    ));
    assert!(c.next_result().await.unwrap().is_none());
    assert!(c.next_result().await.unwrap().is_none());
    assert!(c.take_messages().is_empty());
    c.close().await.unwrap();
    c.close().await.unwrap();

    // unread rows are discarded when advancing
    let mut c = s
        .execute("SELECT 1 UNION ALL SELECT 2; SELECT 3", &[], &opts())
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    assert_eq!(c.fetch(5).await.unwrap(), vec![vec![Value::Int(3)]]);
    drop(c);

    // empty input
    let mut c = s.execute("  -- nothing\n ;", &[], &opts()).await.unwrap();
    assert!(c.next_result().await.unwrap().is_none());
    drop(c);

    // trigger bodies are not split
    exec(&mut s, "CREATE TABLE audit(x); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO audit VALUES (NEW.id); INSERT INTO audit VALUES (-NEW.id); END; INSERT INTO t(v) VALUES ('z')").await;
    assert_eq!(int(&mut s, "SELECT count(*) FROM audit").await, 2);
    // RETURNING produces rows
    let (cols, rows) = query_all(&mut s, "INSERT INTO t(v) VALUES ('r') RETURNING id, v", &[])
        .await
        .unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(rows[0][1], Value::text("r"));
}

#[tokio::test]
async fn parameters_bind_positionally() {
    let mut s = memory().await;
    let r = query_all(
        &mut s,
        "SELECT ?1 + ?1, ?2",
        &[Value::Int(2), Value::text("x")],
    )
    .await
    .unwrap()
    .1;
    assert_eq!(r, vec![vec![Value::Int(4), Value::text("x")]]);
    let r = query_all(
        &mut s,
        "SELECT :a, @b, $c",
        &[Value::Int(1), Value::Int(2), Value::Int(3)],
    )
    .await
    .unwrap()
    .1;
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(2), Value::Int(3)]]);
    let err = query_all(&mut s, "SELECT ?", &[]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Query);
    assert!(err.message.contains("expects 1"), "{err:?}");
    assert!(
        query_all(&mut s, "SELECT ?", &[Value::Int(1), Value::Int(2)])
            .await
            .is_err()
    );
    // statements of a batch consume the parameters in order
    let mut c = s
        .execute(
            "SELECT ?; SELECT ? + ?",
            &[Value::Int(1), Value::Int(2), Value::Int(3)],
            &opts(),
        )
        .await
        .unwrap();
    c.next_result().await.unwrap();
    assert_eq!(c.fetch(5).await.unwrap(), vec![vec![Value::Int(1)]]);
    c.next_result().await.unwrap();
    assert_eq!(c.fetch(5).await.unwrap(), vec![vec![Value::Int(5)]]);
}

#[tokio::test]
async fn errors_have_kinds_codes_and_positions() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE p(id INTEGER PRIMARY KEY, code TEXT UNIQUE, n INT CHECK (n > 0))",
    )
    .await;
    exec(&mut s, "CREATE TABLE c(pid INT REFERENCES p(id))").await;
    exec(&mut s, "INSERT INTO p VALUES (1, 'a', 1)").await;

    let err = s
        .execute("SELECT * FORM p", &[], &opts())
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert_eq!(err.position, Some(10), "{err:?}");
    assert!(err.message.contains("syntax error"));

    let mut c = s.execute("SELECT 1;\nSELEC 2", &[], &opts()).await.unwrap();
    assert!(c.next_result().await.unwrap().is_some());
    let err = c.next_result().await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert_eq!(err.position, Some(11), "{err:?}");
    assert!(
        c.next_result().await.unwrap().is_none(),
        "a failed batch ends"
    );
    drop(c);

    let err = query_all(&mut s, "SELECT * FROM nope", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Query);
    assert!(err.message.contains("no such table"));

    let cases = [
        (
            "INSERT INTO p VALUES (2, 'a', 1)",
            "SQLITE_CONSTRAINT_UNIQUE",
        ),
        (
            "INSERT INTO p VALUES (1, 'b', 1)",
            "SQLITE_CONSTRAINT_PRIMARYKEY",
        ),
        (
            "INSERT INTO p VALUES (3, 'c', 0)",
            "SQLITE_CONSTRAINT_CHECK",
        ),
        ("INSERT INTO c VALUES (42)", "SQLITE_CONSTRAINT_FOREIGNKEY"),
    ];
    for (sql, code) in cases {
        let err = execute_update(&mut s, sql, &[]).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Query, "{sql}");
        assert_eq!(err.code.as_deref(), Some(code), "{sql}: {err:?}");
    }
    // the session is still usable
    assert_eq!(int(&mut s, "SELECT count(*) FROM p").await, 1);
}

#[tokio::test]
async fn streams_endless_results_in_bounded_batches() {
    let mut s = memory().await;
    let mut c = s.execute(ENDLESS, &[], &opts()).await.unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    for batch in 0..5i64 {
        let rows = c.fetch(1000).await.unwrap();
        assert_eq!(rows.len(), 1000);
        assert_eq!(rows[0][0], Value::Int(batch * 1000 + 1));
    }
    // dropping the cursor mid-stream frees the session immediately
    drop(c);
    let started = Instant::now();
    assert_eq!(int(&mut s, "SELECT 42").await, 42);
    assert!(started.elapsed() < Duration::from_secs(1));

    // closing mid-stream as well
    let mut c = s.execute(ENDLESS, &[], &opts()).await.unwrap();
    c.next_result().await.unwrap();
    assert_eq!(c.fetch(3).await.unwrap().len(), 3);
    c.close().await.unwrap();
    assert!(c.fetch(3).await.unwrap().is_empty());
    assert_eq!(int(&mut s, "SELECT 43").await, 43);
}

#[tokio::test]
async fn a_new_statement_closes_a_forgotten_cursor() {
    let mut s = memory().await;
    let mut old = s.execute(ENDLESS, &[], &opts()).await.unwrap();
    old.next_result().await.unwrap();
    assert_eq!(old.fetch(2).await.unwrap().len(), 2);
    // contract violation: execute while `old` is still open
    assert_eq!(int(&mut s, "SELECT 5").await, 5);
    let err = old.fetch(2).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Query);
    assert!(err.message.contains("closed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancels_a_long_query() {
    let mut s = memory().await;
    let cancel = s.cancel_handle().unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let started = Instant::now();
    let err = query_all(&mut s, COUNT_TO_A_BILLION, &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    canceller.await.unwrap();
    assert_eq!(int(&mut s, "SELECT 1").await, 1);

    // cancelling an open cursor between fetches makes the next fetch fail
    let mut c = s.execute(ENDLESS, &[], &opts()).await.unwrap();
    c.next_result().await.unwrap();
    assert_eq!(c.fetch(10).await.unwrap().len(), 10);
    s.cancel_handle().unwrap().cancel().await.unwrap();
    let err = c.fetch(10).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    drop(c);
    assert_eq!(int(&mut s, "SELECT 2").await, 2);
    // a cancel while idle does not affect the next statement
    s.cancel_handle().unwrap().cancel().await.unwrap();
    assert_eq!(int(&mut s, "SELECT 3").await, 3);
}

#[tokio::test]
async fn statement_timeout_interrupts() {
    let mut s = memory().await;
    let o = ExecOptions {
        timeout: Some(Duration::from_millis(200)),
        ..Default::default()
    };
    let started = Instant::now();
    let err = s.execute(COUNT_TO_A_BILLION, &[], &o).await.err().unwrap();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(1500));
    // a quick statement with a timeout is fine
    let mut c = s.execute("SELECT 1", &[], &o).await.unwrap();
    c.next_result().await.unwrap();
    assert_eq!(c.fetch(1).await.unwrap(), vec![vec![Value::Int(1)]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_pending_execute_releases_the_session() {
    let mut s = memory().await;
    let r = tokio::time::timeout(
        Duration::from_millis(200),
        s.execute(COUNT_TO_A_BILLION, &[], &opts()),
    )
    .await;
    assert!(r.is_err(), "the query should still be running");
    let started = Instant::now();
    assert_eq!(int(&mut s, "SELECT 7").await, 7);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn manual_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "tx.db");
    let mut a = connect(&path).await;
    let mut b = connect(&path).await;
    exec(&mut a, "CREATE TABLE t(x)").await;
    assert_eq!(a.transaction_mode(), TransactionMode::Auto);
    a.set_transaction_mode(TransactionMode::Manual)
        .await
        .unwrap();
    assert_eq!(a.transaction_mode(), TransactionMode::Manual);
    assert!(!a.in_transaction());
    // pragmas and failed statements do not open a transaction
    exec(&mut a, "PRAGMA user_version").await;
    assert!(query_all(&mut a, "SELEC 1", &[]).await.is_err());
    assert!(!a.in_transaction());

    exec(&mut a, "INSERT INTO t VALUES (1)").await;
    assert!(a.in_transaction());
    assert_eq!(
        int(&mut b, "SELECT count(*) FROM t").await,
        0,
        "not committed yet"
    );
    a.rollback().await.unwrap();
    assert!(!a.in_transaction());
    assert_eq!(int(&mut a, "SELECT count(*) FROM t").await, 0);
    a.rollback().await.unwrap();

    exec(&mut a, "INSERT INTO t VALUES (2)").await;
    a.commit().await.unwrap();
    assert!(!a.in_transaction());
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 1);

    exec(&mut a, "INSERT INTO t VALUES (3)").await;
    assert!(a.in_transaction());
    // switching to auto-commit commits (JDBC semantics)
    a.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert!(!a.in_transaction());
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 2);

    // explicit transactions in auto-commit mode
    exec(&mut a, "BEGIN").await;
    assert!(a.in_transaction());
    exec(&mut a, "INSERT INTO t VALUES (4); COMMIT").await;
    assert!(!a.in_transaction());
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 3);

    // manual mode from the connection parameters
    let mut p = params(&path, &[]);
    p.auto_commit = false;
    let mut m = SqliteDriver::new().connect_sqlite(&p).await.unwrap();
    assert_eq!(m.transaction_mode(), TransactionMode::Manual);
    exec(&mut m, "DELETE FROM t").await;
    assert!(m.in_transaction());
    m.rollback().await.unwrap();
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 3);
}

#[tokio::test]
async fn attached_databases() {
    let dir = tempfile::tempdir().unwrap();
    let main_path = file_in(dir.path(), "main.db");
    let aux_path = file_in(dir.path(), "aux.db");
    let mut aux = connect(&aux_path).await;
    exec(&mut aux, "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO items(name) VALUES ('x')").await;
    close(aux).await;
    let mut s = connect_with(&main_path, &[("attach.aux", &aux_path)]).await;
    let schemas: Vec<String> = s
        .list_schemas()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(schemas, vec!["main", "temp", "aux"]);
    assert_eq!(
        scalar(&mut s, "SELECT name FROM aux.items").await,
        Value::text("x")
    );
    let other = file_in(dir.path(), "other.db");
    execute_update(&mut s, "ATTACH DATABASE ? AS other", &[Value::text(other)])
        .await
        .unwrap();
    exec(&mut s, "CREATE TABLE other.o(a)").await;
    assert!(
        s.list_schemas()
            .await
            .unwrap()
            .iter()
            .any(|x| x.name == "other")
    );
    exec(&mut s, "DETACH DATABASE other").await;
    assert!(
        !s.list_schemas()
            .await
            .unwrap()
            .iter()
            .any(|x| x.name == "other")
    );
}

#[tokio::test]
async fn read_only_connections_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "ro.db");
    let mut w = connect(&path).await;
    exec(&mut w, "CREATE TABLE t(x); INSERT INTO t VALUES (1)").await;
    close(w).await;
    for props in [&[("read_only", "true")][..], &[][..]] {
        let mut p = params(&path, props);
        if props.is_empty() {
            p.read_only = true;
        }
        let mut s = SqliteDriver::new().connect_sqlite(&p).await.unwrap();
        assert_eq!(int(&mut s, "SELECT count(*) FROM t").await, 1);
        for sql in [
            "INSERT INTO t VALUES (2)",
            "DELETE FROM t",
            "CREATE TABLE u(a)",
            "CREATE TEMP TABLE tt(a)",
            "DROP TABLE t",
        ] {
            let err = execute_update(&mut s, sql, &[]).await.unwrap_err();
            assert_eq!(err.kind, ErrorKind::ReadOnly, "{sql}: {err:?}");
        }
        assert_eq!(int(&mut s, "PRAGMA query_only").await, 1);
        exec(&mut s, "BEGIN; SELECT 1; COMMIT").await;
        // even after switching query_only off the file stays read-only
        exec(&mut s, "PRAGMA query_only = OFF").await;
        let err = execute_update(&mut s, "INSERT INTO t VALUES (3)", &[])
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReadOnly, "{err:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sessions_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "wal.db");
    let mut a = connect_with(&path, &[("journal_mode", "WAL")]).await;
    let mut b = connect_with(&path, &[("journal_mode", "WAL"), ("busy_timeout", "200")]).await;
    exec(&mut a, "CREATE TABLE t(x)").await;
    exec(&mut a, "BEGIN; INSERT INTO t VALUES (1)").await;
    // readers are not blocked by the writer
    let started = Instant::now();
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 0);
    assert!(started.elapsed() < Duration::from_millis(1000));
    // a second writer waits for the busy timeout, then fails with a clear error
    let started = Instant::now();
    let err = execute_update(&mut b, "INSERT INTO t VALUES (2)", &[])
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert_eq!(err.kind, ErrorKind::Query, "{err:?}");
    assert_eq!(err.code.as_deref(), Some("SQLITE_BUSY"));
    assert!(
        err.message.contains("locked") && err.message.contains("200 ms"),
        "{}",
        err.message
    );
    assert!(
        waited >= Duration::from_millis(150) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
    exec(&mut a, "COMMIT").await;
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 1);
    exec(&mut b, "INSERT INTO t VALUES (2)").await;
    assert_eq!(int(&mut a, "SELECT count(*) FROM t").await, 2);

    // cancelling a statement that waits for a lock
    let mut c = connect_with(&path, &[("busy_timeout", "30000")]).await;
    exec(&mut a, "BEGIN IMMEDIATE").await;
    let cancel = c.cancel_handle().unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        cancel.cancel().await.unwrap();
    });
    let started = Instant::now();
    let err = execute_update(&mut c, "INSERT INTO t VALUES (3)", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    canceller.await.unwrap();
    exec(&mut a, "ROLLBACK").await;
    exec(&mut c, "INSERT INTO t VALUES (3)").await;
    assert_eq!(int(&mut a, "SELECT count(*) FROM t").await, 3);
}

#[tokio::test]
async fn busy_in_rollback_journal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "busy.db");
    let mut a = connect(&path).await;
    exec(&mut a, "CREATE TABLE t(x)").await;
    let mut b = connect_with(&path, &[("busy_timeout", "0")]).await;
    exec(&mut a, "BEGIN EXCLUSIVE; INSERT INTO t VALUES (1)").await;
    let started = Instant::now();
    let err = query_all(&mut b, "SELECT * FROM t", &[]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Query, "{err:?}");
    assert!(err.message.contains("locked"), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(1000));
    exec(&mut a, "COMMIT").await;
    assert_eq!(int(&mut b, "SELECT count(*) FROM t").await, 1);
}

#[tokio::test]
async fn online_backup_and_vacuum_into() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "src.db");
    let mut s = connect(&path).await;
    exec(&mut s, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)").await;
    exec(&mut s, "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000) INSERT INTO t(v) SELECT printf('row %d', x) FROM c").await;

    let dest = dir.path().join("copy.db");
    let report = online_backup(&mut s, &dest).await.unwrap();
    assert!(
        report.pages > 0 && report.bytes >= report.pages * 512,
        "{report:?}"
    );
    let mut copy = connect(&dest.to_string_lossy()).await;
    assert_eq!(int(&mut copy, "SELECT count(*) FROM t").await, 5000);
    close(copy).await;

    // from a file, with progress
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let dest2 = dir.path().join("copy2.db");
    let src = Path::new(&path).to_path_buf();
    let report = online_backup_with_progress(
        &src,
        &dest2,
        Some(Arc::new(move |p: BackupProgress| {
            assert!(p.remaining <= p.total);
            seen.fetch_add(1, Ordering::SeqCst);
        })),
    )
    .await
    .unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 1);
    assert!(report.pages > 0);
    let mut copy = connect(&dest2.to_string_lossy()).await;
    assert_eq!(
        scalar(&mut copy, "SELECT v FROM t WHERE id = 5000").await,
        Value::text("row 5000")
    );
    close(copy).await;

    // never onto the source itself
    let err = online_backup(&mut s, Path::new(&path)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);

    // in-memory databases can be backed up too
    let mut m = memory().await;
    exec(&mut m, "CREATE TABLE x(a); INSERT INTO x VALUES (1)").await;
    let dest3 = dir.path().join("mem.db");
    online_backup(&mut m, &dest3).await.unwrap();
    let mut copy = connect(&dest3.to_string_lossy()).await;
    assert_eq!(int(&mut copy, "SELECT a FROM x").await, 1);

    // VACUUM INTO as the SQL alternative
    let dest4 = dir.path().join("vacuum.db");
    exec(&mut s, &vacuum_into_sql(None, &dest4)).await;
    let mut copy = connect(&dest4.to_string_lossy()).await;
    assert_eq!(int(&mut copy, "SELECT count(*) FROM t").await, 5000);
}

#[tokio::test]
async fn explain_query_plan() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INT); CREATE INDEX ix_b ON t(b)",
    )
    .await;
    exec(&mut s, "CREATE TABLE u(id INTEGER PRIMARY KEY, t_a INT)").await;
    let plan = s
        .explain("SELECT * FROM t WHERE b = 'x'", false)
        .await
        .unwrap();
    assert!(!plan.analyzed);
    assert_eq!(plan.roots.len(), 1, "{plan:?}");
    assert_eq!(plan.roots[0].node_type, "SEARCH");
    assert_eq!(plan.roots[0].relation.as_deref(), Some("t"));
    assert_eq!(plan.roots[0].index.as_deref(), Some("ix_b"));
    assert_eq!(plan.roots[0].condition.as_deref(), Some("b=?"));
    assert!(
        plan.text
            .starts_with("QUERY PLAN\n`--SEARCH t USING INDEX ix_b (b=?)"),
        "{}",
        plan.text
    );
    assert!(
        plan.raw
            .as_deref()
            .is_some_and(|r| r.contains("\"detail\""))
    );

    let plan = s
        .explain(
            "SELECT t.b, (SELECT count(*) FROM u WHERE u.t_a = t.a) FROM t ORDER BY t.c;",
            false,
        )
        .await
        .unwrap();
    let mut types = Vec::new();
    for r in &plan.roots {
        r.walk(0, &mut |n, _| types.push(n.node_type.clone()));
    }
    assert!(types.iter().any(|t| t == "SCAN"), "{types:?}");
    assert!(types.iter().any(|t| t.contains("SUBQUERY")), "{types:?}");
    assert!(types.iter().any(|t| t == "USE TEMP B-TREE"), "{types:?}");
    assert!(plan.roots.iter().map(|r| r.count()).sum::<usize>() >= 3);

    // an explicit EXPLAIN QUERY PLAN prefix is accepted
    assert!(
        s.explain("EXPLAIN QUERY PLAN SELECT 1", false)
            .await
            .is_ok()
    );
    assert_eq!(
        s.explain("SELECT 1", true).await.unwrap_err().kind,
        ErrorKind::Unsupported
    );
    let err = s.explain("SELECT * FORM t", false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert_eq!(err.position, Some(10), "{err:?}");
    // explaining a write does not execute it
    s.explain("DELETE FROM t", false).await.unwrap();
}

#[tokio::test]
async fn sessions_are_independent_threads() {
    let mut sessions = Vec::new();
    for _ in 0..8 {
        sessions.push(memory().await);
    }
    for (i, s) in sessions.iter_mut().enumerate() {
        exec(s, &format!("CREATE TABLE t(x); INSERT INTO t VALUES ({i})")).await;
    }
    for (i, s) in sessions.iter_mut().enumerate() {
        assert_eq!(int(s, "SELECT x FROM t").await, i as i64);
    }
    // dropping sessions without closing them ends their threads
    drop(sessions);
}
