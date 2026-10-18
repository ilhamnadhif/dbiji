//! Statement execution against real DuckDB databases.

mod common;

use common::*;
use dbiji_core::value::DataKind;
use dbiji_core::{ErrorKind, ExecOptions, ResultHeader, Session, TransactionMode, Value};
use dbiji_driver_duckdb::DuckDbDriver;
use std::time::{Duration, Instant};

#[tokio::test]
async fn connect_ping_and_close() {
    let mut s = memory().await;
    assert_eq!(s.server_info().product, "DuckDB");
    assert!(s.server_info().version_number.0 >= 1);
    s.ping().await.unwrap();
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("memory")
    );
    assert_eq!(s.current_schema().await.unwrap().as_deref(), Some("main"));
    s.close().await.unwrap();
    assert!(s.is_closed());
    assert_eq!(s.ping().await.unwrap_err().kind, ErrorKind::Connection);
}

#[tokio::test]
async fn open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let d = DuckDbDriver::new();
    let missing = file_in(dir.path(), "missing.duckdb");
    let e = d
        .connect_duckdb(&params(&missing, &[("read_only", "true")]))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Io, "{e:?}");
    let bad_dir = file_in(dir.path(), "no/such/dir/x.duckdb");
    assert_eq!(
        d.connect_duckdb(&params(&bad_dir, &[]))
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Io
    );
    let garbage = file_in(dir.path(), "garbage.duckdb");
    std::fs::write(&garbage, vec![b'x'; 8192]).unwrap();
    assert_eq!(
        d.connect_duckdb(&params(&garbage, &[]))
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Io
    );
    let e = d
        .connect_duckdb(&params(":memory:", &[("threads", "-1")]))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Config);
    let e = d
        .connect_duckdb(&params(":memory:", &[("setting.no_such_option", "1")]))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Config, "{e:?}");
}

#[tokio::test]
async fn value_conversions() {
    let mut s = memory().await;
    exec(&mut s, "CREATE TYPE mood AS ENUM ('sad', 'happy')").await;
    let sql = "SELECT 42::TINYINT, 170141183460469231731687303715884105727::HUGEINT, 18446744073709551615::UBIGINT, \
               1.5::DOUBLE, 0.1::FLOAT, 12.50::DECIMAL(10,2), 'héllo', '\\x00\\xFF'::BLOB, true, \
               DATE '2024-02-29', TIME '13:14:15.5', TIMESTAMP '2024-01-02 03:04:05.123456', \
               TIMESTAMPTZ '2024-01-02 03:04:05+00', INTERVAL '1 year 2 days 3 seconds', \
               '67e55044-10b1-426f-9247-bb680e5fe0c8'::UUID, '{\"a\": 1}'::JSON, [1, 2, NULL], \
               {'x': 1, 'y': 'z'}, MAP {'k': 1}, 'happy'::mood, '101'::BIT, NULL::INTEGER, 'infinity'::TIMESTAMP";
    let (cols, data) = dbiji_core::driver::query_all(&mut s, sql, &[])
        .await
        .unwrap();
    let r = &data[0];
    assert_eq!(r[0], Value::Int(42));
    assert_eq!(
        r[1],
        Value::decimal("170141183460469231731687303715884105727")
    );
    assert_eq!(r[2], Value::UInt(u64::MAX));
    assert_eq!(r[3], Value::Float(1.5));
    assert_eq!(r[4], Value::Float(0.1));
    assert_eq!(r[5], Value::decimal("12.50"));
    assert_eq!(cols[5].type_name, "DECIMAL(10,2)");
    assert_eq!((cols[5].precision, cols[5].scale), (Some(10), Some(2)));
    assert_eq!(r[6], Value::text("héllo"));
    assert_eq!(r[7], Value::bytes(vec![0u8, 255]));
    assert_eq!(r[8], Value::Bool(true));
    assert_eq!(r[9].to_text(), "2024-02-29");
    assert_eq!(r[10].to_text(), "13:14:15.5");
    assert_eq!(r[11].to_text(), "2024-01-02 03:04:05.123456");
    assert!(matches!(r[12], Value::DateTimeTz(_)), "{:?}", r[12]);
    assert_eq!(cols[12].kind, DataKind::DateTimeTz);
    assert_eq!(r[13], Value::text("1 year 2 days 00:00:03"));
    assert!(matches!(r[14], Value::Uuid(_)));
    assert_eq!(r[15], Value::json("{\"a\": 1}"));
    assert_eq!(cols[15].kind, DataKind::Json);
    assert_eq!(r[16], Value::json("[1,2,null]"));
    assert_eq!(cols[16].kind, DataKind::Array);
    assert_eq!(cols[16].type_name, "INTEGER[]");
    assert_eq!(r[17], Value::json("{\"x\":1,\"y\":\"z\"}"));
    assert_eq!(r[18], Value::json("{\"k\":1}"));
    assert_eq!(r[19], Value::text("happy"));
    assert_eq!(cols[19].kind, DataKind::Enum);
    assert_eq!(r[20], Value::text("101"));
    assert_eq!(cols[20].kind, DataKind::Bit);
    assert_eq!(r[21], Value::Null);
    assert_eq!(r[22], Value::text("infinity"));
}

#[tokio::test]
async fn parameters() {
    let mut s = memory().await;
    let date = chrono::NaiveDate::from_ymd_opt(2024, 5, 6).unwrap();
    let ts = date.and_hms_micro_opt(7, 8, 9, 10).unwrap();
    let uuid = uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
    let params = vec![
        Value::Int(-5),
        Value::Float(2.25),
        Value::decimal("-12.345"),
        Value::text("it's"),
        Value::bytes(vec![1u8, 2]),
        Value::Bool(false),
        Value::Date(date),
        Value::DateTime(ts),
        Value::Uuid(uuid),
        Value::Null,
    ];
    let sql = "SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?::UUID, ?";
    let (_, data) = dbiji_core::driver::query_all(&mut s, sql, &params)
        .await
        .unwrap();
    let r = &data[0];
    assert_eq!(r[0], Value::Int(-5));
    assert_eq!(r[1], Value::Float(2.25));
    assert_eq!(r[2], Value::decimal("-12.345"));
    assert_eq!(r[3], Value::text("it's"));
    assert_eq!(r[4], Value::bytes(vec![1u8, 2]));
    assert_eq!(r[5], Value::Bool(false));
    assert_eq!(r[6], Value::Date(date));
    assert_eq!(r[7], Value::DateTime(ts));
    assert_eq!(r[8], Value::Uuid(uuid));
    assert_eq!(r[9], Value::Null);
    // $n parameters and parameters spread over a script
    assert_eq!(
        dbiji_core::driver::query_scalar(
            &mut s,
            "SELECT $2 - $1",
            &[Value::Int(1), Value::Int(10)]
        )
        .await
        .unwrap(),
        Value::Int(9)
    );
    let e = dbiji_core::driver::query_scalar(&mut s, "SELECT ?", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Query);
    exec(&mut s, "CREATE TABLE p(a INT, b VARCHAR)").await;
    let n = dbiji_core::driver::execute_update(
        &mut s,
        "INSERT INTO p VALUES (?, ?); INSERT INTO p VALUES (?, 'x')",
        &[Value::Int(1), Value::text("a"), Value::Int(2)],
    )
    .await
    .unwrap();
    assert_eq!(n, Some(2));
}

#[tokio::test]
async fn multi_statement_protocol() {
    let mut s = memory().await;
    let mut c = s
        .execute(
            "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2); SELECT a FROM t ORDER BY a; \
             UPDATE t SET a = a + 10; BEGIN; COMMIT; SET threads = 2",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    match c.next_result().await.unwrap() {
        Some(ResultHeader::Update {
            rows_affected: None,
            tag: Some(t),
            ..
        }) => assert_eq!(t, "CREATE TABLE"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update {
            rows_affected: Some(2),
            ..
        })
    ));
    match c.next_result().await.unwrap() {
        Some(ResultHeader::Rows(cols)) => assert_eq!(cols[0].name, "a"),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.fetch(1).await.unwrap(), vec![vec![Value::Int(1)]]);
    assert_eq!(c.fetch(10).await.unwrap(), vec![vec![Value::Int(2)]]);
    assert!(c.fetch(10).await.unwrap().is_empty());
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update {
            rows_affected: Some(2),
            ..
        })
    ));
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update { .. })
    ));
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update { .. })
    ));
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Update { .. })
    ));
    assert!(c.next_result().await.unwrap().is_none());
    c.close().await.unwrap();
    drop(c);
    assert!(!s.in_transaction());
    assert_eq!(int(&mut s, "SELECT sum(a) FROM t").await, 23);
    // a row result left unread is discarded by the next statement
    let mut c = s
        .execute("SELECT * FROM range(100000)", &[], &ExecOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    assert_eq!(c.fetch(5).await.unwrap().len(), 5);
    drop(c);
    assert_eq!(int(&mut s, "SELECT 7").await, 7);
}

#[tokio::test]
async fn errors() {
    let mut s = memory().await;
    let e = dbiji_core::driver::query_all(&mut s, "SELECT * FORM t", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Syntax);
    assert_eq!(e.code.as_deref(), Some("PARSER"));
    assert!(e.position.is_some(), "{e:?}");
    let e = dbiji_core::driver::query_all(&mut s, "SELECT * FROM no_such_table", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Query);
    assert_eq!(e.code.as_deref(), Some("CATALOG"));
    exec(&mut s, "CREATE TABLE u(id INTEGER PRIMARY KEY)").await;
    exec(&mut s, "INSERT INTO u VALUES (1)").await;
    let e = dbiji_core::driver::execute_update(&mut s, "INSERT INTO u VALUES (1)", &[])
        .await
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("CONSTRAINT"), "{e:?}");
    // the error of the second statement surfaces at next_result
    let mut c = s
        .execute(
            "SELECT 1; SELECT x FROM nowhere; SELECT 3",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    let e = c.next_result().await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("CATALOG"));
    assert!(c.next_result().await.unwrap().is_none());
    drop(c);
    // runtime errors while fetching
    let e = dbiji_core::driver::query_all(
        &mut s,
        "SELECT CAST('x' || i AS INTEGER) FROM range(10) t(i)",
        &[],
    )
    .await
    .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("CONVERSION"), "{e:?}");
    assert_eq!(int(&mut s, "SELECT 1").await, 1);
}

#[tokio::test]
async fn streaming_and_dropped_cursors() {
    let mut s = memory().await;
    let mut c = s
        .execute(
            "SELECT i, repeat('x', 100) FROM range(100000000) t(i)",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    let mut total = 0;
    for _ in 0..20 {
        total += c.fetch(1000).await.unwrap().len();
    }
    assert_eq!(total, 20_000);
    c.close().await.unwrap();
    drop(c);
    // a forgotten cursor does not block the session
    let mut c = s
        .execute(
            "SELECT * FROM range(100000000)",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    c.next_result().await.unwrap();
    c.fetch(10).await.unwrap();
    std::mem::forget(c);
    assert_eq!(int(&mut s, "SELECT 5").await, 5);
}

#[tokio::test]
async fn cancel_and_timeout() {
    let mut s = memory().await;
    let cancel = s.cancel_handle().unwrap();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let e =
        dbiji_core::driver::query_all(&mut s, "SELECT count(*) FROM range(100000000000) a", &[])
            .await
            .unwrap_err();
    canceller.await.unwrap();
    assert_eq!(e.kind, ErrorKind::Cancelled, "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(int(&mut s, "SELECT 1").await, 1);

    // cancel between fetches
    let mut c = s
        .execute(
            "SELECT * FROM range(100000000)",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    c.next_result().await.unwrap();
    assert!(!c.fetch(100).await.unwrap().is_empty());
    s.cancel_handle().unwrap().cancel().await.unwrap();
    let mut err = None;
    for _ in 0..1000 {
        match c.fetch(4096).await {
            Ok(r) if r.is_empty() => break,
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert_eq!(err.map(|e| e.kind), Some(ErrorKind::Cancelled));
    drop(c);

    // statement timeout
    let opts = ExecOptions {
        timeout: Some(Duration::from_millis(200)),
        ..Default::default()
    };
    let started = Instant::now();
    // execute returns the first result, here the error
    let e = s
        .execute("SELECT count(*) FROM range(100000000000) a", &[], &opts)
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Timeout, "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(int(&mut s, "SELECT 2").await, 2);
    // the timeout applies to every statement of a script
    let mut c = s
        .execute(
            "SELECT 1; SELECT count(*) FROM range(100000000000) a",
            &[],
            &opts,
        )
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    assert_eq!(c.next_result().await.unwrap_err().kind, ErrorKind::Timeout);
    drop(c);

    // dropping a pending execute stops the statement
    {
        let defaults = ExecOptions::default();
        let fut = s.execute("SELECT count(*) FROM range(100000000000) a", &[], &defaults);
        let r = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(r.is_err());
    }
    let started = Instant::now();
    assert_eq!(int(&mut s, "SELECT 3").await, 3);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn manual_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "tx.duckdb");
    let mut cfg = params(&path, &[]);
    cfg.auto_commit = false;
    let mut s = DuckDbDriver::new().connect_duckdb(&cfg).await.unwrap();
    assert_eq!(s.transaction_mode(), TransactionMode::Manual);
    exec(&mut s, "CREATE TABLE t(a INTEGER)").await;
    assert!(s.in_transaction());
    s.commit().await.unwrap();
    assert!(!s.in_transaction());
    exec(&mut s, "INSERT INTO t VALUES (1)").await;
    assert!(s.in_transaction());
    // a second session on the same file does not see uncommitted rows
    let mut other = connect(&path).await;
    assert_eq!(int(&mut other, "SELECT count(*) FROM t").await, 0);
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    assert_eq!(int(&mut s, "SELECT count(*) FROM t").await, 0);
    exec(&mut s, "INSERT INTO t VALUES (2)").await;
    s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert!(!s.in_transaction());
    assert_eq!(int(&mut other, "SELECT count(*) FROM t").await, 1);
    // explicit transactions in auto-commit mode
    exec(&mut s, "BEGIN TRANSACTION; INSERT INTO t VALUES (3)").await;
    assert!(s.in_transaction());
    exec(&mut s, "ROLLBACK").await;
    assert!(!s.in_transaction());
    assert_eq!(int(&mut s, "SELECT count(*) FROM t").await, 1);
    close(s).await;
    close(other).await;
}

#[tokio::test]
async fn read_only_and_shared_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "ro.duckdb");
    let mut w = connect(&path).await;
    exec(
        &mut w,
        "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    // the instance is shared: a read-only session needs all others closed
    let e = DuckDbDriver::new()
        .connect_duckdb(&params(&path, &[("read_only", "true")]))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Config);
    let mut w2 = connect(&path).await;
    assert_eq!(int(&mut w2, "SELECT count(*) FROM t").await, 1);
    close(w).await;
    close(w2).await;
    let mut r = connect_with(&path, &[("read_only", "true")]).await;
    assert_eq!(int(&mut r, "SELECT count(*) FROM t").await, 1);
    let e = dbiji_core::driver::execute_update(&mut r, "INSERT INTO t VALUES (2)", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::ReadOnly, "{e:?}");
    close(r).await;
    // closed: the file can be opened read-write again
    let mut w = connect(&path).await;
    exec(&mut w, "INSERT INTO t VALUES (2)").await;
    close(w).await;
}

#[tokio::test]
async fn column_sources() {
    let mut s = memory().await;
    exec(&mut s, "CREATE SEQUENCE seq; CREATE TABLE src(id INTEGER DEFAULT nextval('seq') PRIMARY KEY, name VARCHAR NOT NULL, n INTEGER)")
        .await;
    let opts = ExecOptions {
        resolve_sources: true,
        ..Default::default()
    };
    let mut c = s
        .execute("SELECT name AS label, n, id, n + 1 FROM src", &[], &opts)
        .await
        .unwrap();
    let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
        panic!("rows expected")
    };
    drop(c);
    let src = cols[0].source.as_ref().unwrap();
    assert_eq!((src.table.as_str(), src.column.as_str()), ("src", "name"));
    assert_eq!(src.schema.as_deref(), Some("main"));
    assert_eq!(cols[0].nullable, Some(false));
    assert_eq!(cols[2].auto_increment, Some(true));
    assert!(cols[3].source.is_none());
    let mut c = s.execute("SELECT * FROM src", &[], &opts).await.unwrap();
    let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
        panic!("rows expected")
    };
    assert!(cols.iter().all(|c| c.source.is_some()));
}

#[tokio::test]
async fn databases_and_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let main = file_in(dir.path(), "app.duckdb");
    let other = file_in(dir.path(), "other.duckdb");
    let mut s = connect(&main).await;
    exec(
        &mut s,
        &format!(
            "ATTACH '{other}' AS other; CREATE TABLE other.main.x(i INTEGER); CREATE SCHEMA s2"
        ),
    )
    .await;
    let dbs = s.list_databases().await.unwrap();
    let names: Vec<&str> = dbs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"app") && names.contains(&"other"),
        "{names:?}"
    );
    assert!(dbs.iter().find(|d| d.name == "app").unwrap().is_current);
    s.set_current_schema("s2").await.unwrap();
    assert_eq!(s.current_schema().await.unwrap().as_deref(), Some("s2"));
    s.set_current_database("other").await.unwrap();
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("other")
    );
    assert_eq!(int(&mut s, "SELECT count(*) FROM x").await, 0);
    let schemas = s.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|x| x.name == "main" && x.is_current));
    close(s).await;
}
