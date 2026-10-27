//! Integration tests against a live PostgreSQL server.
//!
//! * `DBIJI_TEST_PG` (e.g. `postgres://dbiji:dbiji@127.0.0.1:55432/dbiji`, superuser)
//!   enables the main suite. Every test works in its own uniquely named
//!   schema or database and removes it at the end.
//! * `DBIJI_TEST_PGDBG` (a server with pldebugger, PostGIS and SSL) enables the
//!   debugger, PostGIS and TLS tests; `DBIJI_TEST_PG_TLS_DIR` points to a
//!   directory with `ca.crt`, `client.crt` and `client.key` (client
//!   certificate for the role `certuser`).
//!
//! Without the variables every test returns immediately (and passes).

#![allow(clippy::result_large_err)]

use dbiji_core::admin::{BackupFormat, BackupOptions, KillMode, RestoreOptions};
use dbiji_core::config::{SslConfig, SslMode};
use dbiji_core::ddl::{DdlGenerator, SnapshotOptions, TableDef};
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::{MetadataSearch, ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{ConnectParams, DataKind, DbKind, Driver, ErrorKind, ExecOptions, ResultHeader, Secret, Session, TransactionMode, Value};
use dbiji_driver_postgres::{PgDdl, PostgresDriver};
use std::time::{Duration, Instant};

mod common;
use common::*;

// ---------------------------------------------------------------- connection

#[tokio::test]
async fn connect_and_server_info() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let info = s.server_info().clone();
    assert_eq!(info.product, "PostgreSQL");
    assert!(info.version_number.0 >= 11, "{info:?}");
    assert!(info.session_id.as_deref().is_some_and(|p| p.parse::<i32>().is_ok()));
    assert_eq!(s.kind(), DbKind::Postgres);
    s.ping().await.unwrap();
    assert!(!s.is_closed());
    // session settings applied after connect
    assert_eq!(scalar(&mut s, "SHOW DateStyle").await, Value::text("ISO, MDY"));
    assert_eq!(scalar(&mut s, "SHOW IntervalStyle").await, Value::text("postgres"));
    assert_eq!(scalar(&mut s, "SHOW bytea_output").await, Value::text("hex"));
    assert_eq!(scalar(&mut s, "SHOW application_name").await, Value::text("dbiji-driver-postgres tests"));
    assert_eq!(s.current_database().await.unwrap(), params.database.clone());
    s.close().await.unwrap();
    assert!(s.is_closed());
    let e = s.ping().await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Connection);
}

#[tokio::test]
async fn connect_errors_and_properties() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    // wrong password
    let mut bad = params.clone();
    bad.password = Some(Secret::new("definitely-wrong"));
    let e = PostgresDriver::new().connect(&bad).await.err().expect("must fail");
    assert_eq!(e.kind, ErrorKind::Auth, "{}", e.full_text());
    assert!(!e.full_text().contains("definitely-wrong"));
    // unknown database
    let mut nodb = params.clone();
    nodb.database = Some(format!("missing_{}", suffix()));
    let e = PostgresDriver::new().connect(&nodb).await.err().expect("must fail");
    assert_eq!(e.code.as_deref(), Some("3D000"));
    // refused port
    let mut refused = params.clone();
    refused.port = 1;
    let e = PostgresDriver::new().connect(&refused).await.err().expect("must fail");
    assert_eq!(e.kind, ErrorKind::Connection, "{}", e.full_text());
    // properties: search_path, statement_timeout, target_session_attrs, read only
    let mut p = params.clone();
    p.properties.insert("search_path".into(), "pg_catalog, public".into());
    p.properties.insert("statement_timeout".into(), "12345".into());
    p.properties.insert("target_session_attrs".into(), "read-write".into());
    p.properties.insert("unknown_property".into(), "ignored".into());
    let mut s = connect(&p).await;
    assert_eq!(scalar(&mut s, "SHOW search_path").await, Value::text("pg_catalog, public"));
    assert_eq!(scalar(&mut s, "SHOW statement_timeout").await, Value::text("12345ms"));
    s.close().await.ok();
    let mut ro = params.clone();
    ro.read_only = true;
    let mut s = connect(&ro).await;
    let e = execute_update(&mut *s, "CREATE TEMP TABLE ro_test (a int)", &[]).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::ReadOnly, "{}", e.full_text());
    s.close().await.ok();
    let mut standby = params.clone();
    standby.properties.insert("target_session_attrs".into(), "standby".into());
    assert!(PostgresDriver::new().connect(&standby).await.is_err());
}

// ---------------------------------------------------------------- execution

#[tokio::test]
async fn type_conversions_text_protocol() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        exec(&mut s, &format!("CREATE TYPE {schema}.mood AS ENUM ('sad', 'happy')")).await;
        exec(&mut s, "SET TimeZone = 'Asia/Kolkata'").await;
        let multirange = major(s.as_ref()) >= 14;
        let mr = if multirange { "'{[1,3), [5,7)}'::int4multirange" } else { "NULL::int4range" };
        let sql = format!(
            "SELECT true AS b, 42::int2 AS i2, 42::int4 AS i4, 9007199254740993::int8 AS i8, 4294967295::oid AS o, \
             1.5::float4 AS f4, 2.25::float8 AS f8, 'NaN'::float8 AS fnan, '-Infinity'::float8 AS finf, \
             12.345::numeric(10,3) AS num, 'NaN'::numeric AS numnan, 12.34::money AS m, \
             '2024-02-29'::date AS d, 'infinity'::date AS dinf, '0044-03-15 BC'::date AS dbc, \
             '13:14:15.5'::time AS t, '24:00:00'::time AS t24, '13:14:15+05:30'::timetz AS ttz, \
             '2024-01-02 03:04:05.123456'::timestamp AS ts, '2024-01-02 03:04:05+07'::timestamptz AS tstz, \
             '1 year 2 mons 3 days 04:05:06'::interval AS iv, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u, \
             '{{\"a\": 1}}'::json AS j, '{{\"b\": [1, 2]}}'::jsonb AS jb, '\\x0102ff'::bytea AS by, '<a>x</a>'::xml AS x, \
             '192.168.0.1/24'::inet AS ip, '10.0.0.0/8'::cidr AS net, '08:00:2b:01:02:03'::macaddr AS mac, \
             B'1011'::bit(4) AS bt, B'101'::varbit AS vb, '[1,10)'::int4range AS r, {mr} AS mr, \
             'happy'::{schema}.mood AS en, ARRAY[1,2,3] AS arr, ARRAY['a b', NULL] AS tarr, 'x'::text AS txt, \
             'y'::varchar(10) AS vc, 'z'::char(3) AS ch, 'n'::name AS nm, NULL::int4 AS nul, \
             '(1,2)'::point AS pt, 'a fat cat'::tsvector AS tsv"
        );
        let mut cur = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        let Some(ResultHeader::Rows(cols)) = cur.next_result().await.unwrap() else { panic!("rows expected") };
        let row = cur.fetch(10).await.unwrap().remove(0);
        assert!(cur.fetch(10).await.unwrap().is_empty());
        assert!(cur.next_result().await.unwrap().is_none());
        let get = |name: &str| -> (&dbiji_core::ColumnMeta, &Value) {
            let i = cols.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("column {name}"));
            (&cols[i], &row[i])
        };
        assert_eq!(get("b").1, &Value::Bool(true));
        assert_eq!(get("b").0.kind, DataKind::Bool);
        assert_eq!(get("i2").1, &Value::Int(42));
        assert_eq!(get("i2").0.type_name, "int2");
        assert_eq!(get("i8").1, &Value::Int(9007199254740993));
        assert_eq!(get("o").1, &Value::Int(4294967295));
        assert_eq!(get("f4").1, &Value::Float(1.5));
        assert_eq!(get("f8").1, &Value::Float(2.25));
        assert!(matches!(get("fnan").1, Value::Float(f) if f.is_nan()));
        assert_eq!(get("finf").1, &Value::Float(f64::NEG_INFINITY));
        assert_eq!(get("num").1, &Value::decimal("12.345"));
        assert_eq!(get("num").0.type_name, "numeric(10,3)");
        assert_eq!((get("num").0.precision, get("num").0.scale), (Some(10), Some(3)));
        assert_eq!(get("numnan").1, &Value::text("NaN"));
        assert!(matches!(get("m").1, Value::Text(t) if t.contains("12.34")));
        assert_eq!(get("m").0.kind, DataKind::Decimal);
        assert_eq!(get("d").1.to_text(), "2024-02-29");
        assert_eq!(get("dinf").1, &Value::text("infinity"));
        assert!(matches!(get("dbc").1, Value::Date(d) if chrono::Datelike::year(d) == -43));
        assert_eq!(get("t").1.to_text(), "13:14:15.5");
        assert_eq!(get("t24").1, &Value::text("24:00:00"));
        assert_eq!(get("ttz").1, &Value::text("13:14:15+05:30"));
        assert_eq!(get("ttz").0.kind, DataKind::TimeTz);
        assert_eq!(get("ts").1.to_text(), "2024-01-02 03:04:05.123456");
        // 03:04:05+07 is 01:34:05+05:30 in Asia/Kolkata
        match get("tstz").1 {
            Value::DateTimeTz(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 5 * 3600 + 1800);
                assert_eq!(dt.to_rfc3339(), "2024-01-02T01:34:05+05:30");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(get("iv").1, &Value::text("1 year 2 mons 3 days 04:05:06"));
        assert_eq!(get("iv").0.kind, DataKind::Interval);
        assert!(matches!(get("u").1, Value::Uuid(_)));
        assert_eq!(get("j").1, &Value::json("{\"a\": 1}"));
        assert_eq!(get("jb").1, &Value::json("{\"b\": [1, 2]}"));
        assert_eq!(get("jb").0.kind, DataKind::Json);
        assert_eq!(get("by").1, &Value::bytes(vec![1u8, 2, 255]));
        assert_eq!(get("x").0.kind, DataKind::Xml);
        assert_eq!(get("ip").1, &Value::text("192.168.0.1/24"));
        assert_eq!(get("ip").0.kind, DataKind::Network);
        assert_eq!(get("net").1, &Value::text("10.0.0.0/8"));
        assert_eq!(get("mac").1, &Value::text("08:00:2b:01:02:03"));
        assert_eq!(get("bt").1, &Value::text("1011"));
        assert_eq!(get("bt").0.type_name, "bit(4)");
        assert_eq!(get("bt").0.kind, DataKind::Bit);
        assert_eq!(get("r").1, &Value::text("[1,10)"));
        assert_eq!(get("r").0.kind, DataKind::Range);
        if multirange {
            assert_eq!(get("mr").1, &Value::text("{[1,3),[5,7)}"));
            assert_eq!(get("mr").0.kind, DataKind::Range);
        }
        assert_eq!(get("en").1, &Value::text("happy"));
        assert_eq!(get("en").0.kind, DataKind::Enum);
        assert_eq!(get("en").0.type_name, "mood");
        assert_eq!(get("arr").1, &Value::text("{1,2,3}"));
        assert_eq!(get("arr").0.kind, DataKind::Array);
        assert_eq!(get("arr").0.type_name, "int4[]");
        assert_eq!(get("tarr").1, &Value::text("{\"a b\",NULL}"));
        assert_eq!(get("vc").0.type_name, "varchar(10)");
        assert_eq!(get("vc").0.max_length, Some(10));
        assert_eq!(get("ch").1, &Value::text("z  "));
        assert_eq!(get("nm").1, &Value::text("n"));
        assert_eq!(get("nul").1, &Value::Null);
        assert_eq!(get("pt").0.kind, DataKind::Other);
        assert_eq!(get("tsv").1, &Value::text("'a' 'cat' 'fat'"));
        cur.close().await.unwrap();
        s.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn multiple_statements_and_tags() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        let sql = format!(
            "CREATE TABLE {schema}.m (id int primary key, v text); INSERT INTO {schema}.m VALUES (1,'a'),(2,'b'); \
             SELECT * FROM {schema}.m ORDER BY id; UPDATE {schema}.m SET v = 'c' WHERE id = 2; SELECT count(*) FROM {schema}.m; \
             DELETE FROM {schema}.m"
        );
        let mut cur = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        let mut headers = Vec::new();
        let mut row_sets = Vec::new();
        while let Some(h) = cur.next_result().await.unwrap() {
            if matches!(h, ResultHeader::Rows(_)) {
                let mut all = Vec::new();
                loop {
                    let b = cur.fetch(1).await.unwrap();
                    if b.is_empty() {
                        break;
                    }
                    all.extend(b);
                }
                row_sets.push(all);
            }
            headers.push(h);
        }
        assert_eq!(headers.len(), 6, "{headers:?}");
        assert_eq!(headers[0], ResultHeader::Update { rows_affected: None, last_insert_id: None, tag: Some("CREATE TABLE".into()) });
        assert_eq!(headers[1], ResultHeader::Update { rows_affected: Some(2), last_insert_id: None, tag: Some("INSERT 0 2".into()) });
        // multi statement results have no metadata: text columns
        match &headers[2] {
            ResultHeader::Rows(c) => {
                assert_eq!(c.len(), 2);
                assert_eq!(c[0].kind, DataKind::Text);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(row_sets[0], vec![vec![Value::text("1"), Value::text("a")], vec![Value::text("2"), Value::text("b")]]);
        assert_eq!(headers[3], ResultHeader::Update { rows_affected: Some(1), last_insert_id: None, tag: Some("UPDATE 1".into()) });
        assert_eq!(row_sets[1], vec![vec![Value::text("2")]]);
        assert_eq!(headers[5], ResultHeader::Update { rows_affected: Some(2), last_insert_id: None, tag: Some("DELETE 2".into()) });
        cur.close().await.unwrap();

        // advancing with unread rows discards them
        let mut cur = s.execute("SELECT g FROM generate_series(1, 5000) g; SELECT 'second'", &[], &ExecOptions::default()).await.unwrap();
        assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        assert_eq!(cur.fetch(3).await.unwrap().len(), 3);
        assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        assert_eq!(cur.fetch(10).await.unwrap(), vec![vec![Value::text("second")]]);
        assert!(cur.next_result().await.unwrap().is_none());
        drop(cur);

        // empty statement, comments only
        let mut cur = s.execute("  -- nothing\n", &[], &ExecOptions::default()).await.unwrap();
        assert!(cur.next_result().await.unwrap().is_none());
        drop(cur);

        // single statement with RETURNING has typed metadata
        exec(&mut s, &format!("CREATE TABLE {schema}.r (id serial primary key, v text)")).await;
        let (cols, data) = query_all(&mut *s, &format!("INSERT INTO {schema}.r (v) VALUES ('x') RETURNING id, v"), &[]).await.unwrap();
        assert_eq!(cols[0].kind, DataKind::Int);
        assert_eq!(data, vec![vec![Value::Int(1), Value::text("x")]]);

        // COPY TO STDOUT streams lines, COPY FROM STDIN is refused
        let (cols, data) = query_all(&mut *s, &format!("COPY {schema}.r TO STDOUT"), &[]).await.unwrap();
        assert_eq!(cols[0].name, "COPY");
        assert_eq!(data, vec![vec![Value::text("1\tx")]]);
        let e = s.execute(&format!("COPY {schema}.r FROM STDIN"), &[], &ExecOptions::default()).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        let e = s.execute(&format!("SELECT 1; COPY {schema}.r TO STDOUT"), &[], &ExecOptions::default()).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        // the session still works
        assert_eq!(scalar(&mut s, "SELECT 1").await, Value::Int(1));
        s.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn streaming_with_fetch_sizes() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    for fetch in [1usize, 7, 1000, 65_536] {
        let n = if fetch == 1 { 20_000 } else { 200_000 };
        let sql = format!("SELECT g, md5(g::text) FROM generate_series(1, {n}) g");
        let mut cur = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
        let mut count = 0i64;
        loop {
            let batch = cur.fetch(fetch).await.unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= fetch);
            for r in &batch {
                count += 1;
                assert_eq!(r[0], Value::Int(count));
            }
        }
        assert_eq!(count, n);
        assert!(cur.next_result().await.unwrap().is_none());
        cur.close().await.unwrap();
    }
    s.close().await.ok();
}

#[tokio::test]
async fn million_rows_bounded_memory() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let before = rss_kib();
    let started = Instant::now();
    let mut cur = s
        .execute("SELECT g, 'row ' || g || ' with some padding text to make it larger', now(), g * 1.5 FROM generate_series(1, 1000000) g", &[], &ExecOptions::default())
        .await
        .unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    let mut count = 0usize;
    let mut peak = before.unwrap_or(0);
    loop {
        let batch = cur.fetch(1000).await.unwrap();
        if batch.is_empty() {
            break;
        }
        count += batch.len();
        if count.is_multiple_of(100_000)
            && let Some(r) = rss_kib()
        {
            peak = peak.max(r);
        }
    }
    let elapsed = started.elapsed();
    cur.close().await.unwrap();
    assert_eq!(count, 1_000_000);
    if let Some(b) = before {
        let growth_mib = peak.saturating_sub(b) / 1024;
        eprintln!(
            "1M rows in {:.2}s ({:.0} rows/s), RSS {} MiB -> peak {} MiB (+{growth_mib} MiB)",
            elapsed.as_secs_f64(),
            count as f64 / elapsed.as_secs_f64(),
            b / 1024,
            peak / 1024
        );
        // buffering the whole result would need several hundred MiB
        assert!(growth_mib < 64, "memory grew by {growth_mib} MiB while streaming");
    }
    s.close().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_running_statement() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let handle = s.cancel_handle().expect("cancel handle");
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel().await
    });
    let mut cur = s.execute("SELECT pg_sleep(10)", &[], &ExecOptions::default()).await.unwrap();
    let mut err = None;
    loop {
        match cur.next_result().await {
            Ok(Some(ResultHeader::Rows(_))) => {
                if let Err(e) = cur.fetch(10).await {
                    err = Some(e);
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let err = err.expect("statement must be cancelled");
    assert_eq!(err.kind, ErrorKind::Cancelled, "{}", err.full_text());
    assert_eq!(err.code.as_deref(), Some("57014"));
    assert!(started.elapsed() < Duration::from_secs(3), "cancel took {:?}", started.elapsed());
    canceller.await.unwrap().unwrap();
    drop(cur);
    assert_eq!(scalar(&mut s, "SELECT 1").await, Value::Int(1));
    s.close().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statement_timeout_and_abandoned_cursors() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let started = Instant::now();
    let opts = ExecOptions { timeout: Some(Duration::from_millis(500)), ..Default::default() };
    let e = query_all_opts(&mut s, "SELECT pg_sleep(5)", &opts).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Timeout, "{}", e.full_text());
    assert!(started.elapsed() < Duration::from_secs(3));
    // a fast statement with a timeout is not affected
    assert_eq!(query_all_opts(&mut s, "SELECT 1", &opts).await.unwrap()[0][0], Value::Int(1));

    // closing with unread rows cancels the remaining work quickly (timed from
    // the close: PostgreSQL materialises the whole generate_series result
    // before the first row, which can take seconds on a busy machine)
    let mut cur = s.execute("SELECT g FROM generate_series(1, 50000000) g", &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    assert_eq!(cur.fetch(10).await.unwrap().len(), 10);
    let started = Instant::now();
    cur.close().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "close took {:?}", started.elapsed());
    assert_eq!(scalar(&mut s, "SELECT 2").await, Value::Int(2));

    // dropping without close: the next statement waits for the background cleanup
    let mut cur = s.execute("SELECT g FROM generate_series(1, 50000000) g", &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    assert_eq!(cur.fetch(5).await.unwrap().len(), 5);
    drop(cur);
    assert_eq!(scalar(&mut s, "SELECT 3").await, Value::Int(3));

    // using the session while a cursor is open is refused instead of hanging
    let mut cur = s.execute("SELECT g FROM generate_series(1, 100000) g", &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    let busy = s.execute("SELECT 4", &[], &ExecOptions::default()).await.err().expect("busy");
    assert_eq!(busy.kind, ErrorKind::Internal);
    cur.close().await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT 5").await, Value::Int(5));
    s.close().await.ok();
}

async fn query_all_opts(s: &mut Box<dyn Session>, sql: &str, opts: &ExecOptions) -> dbiji_core::DbResult<Vec<Vec<Value>>> {
    let mut cur = s.execute(sql, &[], opts).await?;
    let mut out = Vec::new();
    let result = async {
        while let Some(h) = cur.next_result().await? {
            if matches!(h, ResultHeader::Rows(_)) {
                loop {
                    let b = cur.fetch(100).await?;
                    if b.is_empty() {
                        break;
                    }
                    out.extend(b);
                }
            }
        }
        Ok::<(), dbiji_core::DbError>(())
    }
    .await;
    cur.close().await?;
    result.map(|_| out)
}

#[tokio::test]
async fn manual_transactions() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        let mut other = connect(&params).await;
        exec(&mut s, &format!("CREATE TABLE {schema}.tx (id int primary key)")).await;
        let count = |n: i64| Value::Int(n);
        let count_sql = format!("SELECT count(*) FROM {schema}.tx");

        s.set_transaction_mode(TransactionMode::Manual).await.unwrap();
        assert_eq!(s.transaction_mode(), TransactionMode::Manual);
        assert!(!s.in_transaction());
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (1)")).await;
        assert!(s.in_transaction());
        assert_eq!(scalar(&mut other, &count_sql).await, count(0));
        s.rollback().await.unwrap();
        assert!(!s.in_transaction());
        assert_eq!(scalar(&mut s, &count_sql).await, count(0));
        // a SELECT also opened a transaction in manual mode
        assert!(s.in_transaction());
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (2)")).await;
        s.commit().await.unwrap();
        assert!(!s.in_transaction());
        assert_eq!(scalar(&mut other, &count_sql).await, count(1));

        // a failed statement only undoes itself: the transaction goes on
        // (per-statement savepoint, like psql's ON_ERROR_ROLLBACK)
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (3)")).await;
        let e = execute_update(&mut *s, &format!("INSERT INTO {schema}.tx VALUES (3)"), &[]).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("23505"));
        assert!(e.detail.as_deref().unwrap_or("").contains("already exists"), "{}", e.full_text());
        assert!(s.in_transaction());
        assert_eq!(scalar(&mut s, "SELECT 1").await, Value::Int(1));
        assert!(s.list_schemas().await.is_ok());
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM {schema}.tx WHERE id = 3")).await, count(1));
        // an explicit user savepoint is not disturbed
        exec(&mut s, "SAVEPOINT mine").await;
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (30)")).await;
        exec(&mut s, "ROLLBACK TO SAVEPOINT mine").await;
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM {schema}.tx WHERE id = 30")).await, count(0));
        s.rollback().await.unwrap();
        assert!(!s.in_transaction());
        // statements that cannot run in a transaction block run without implicit BEGIN
        exec(&mut s, &format!("VACUUM {schema}.tx")).await;
        assert!(!s.in_transaction());
        // switching to auto commits
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (4)")).await;
        assert!(s.in_transaction());
        s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
        assert!(!s.in_transaction());
        assert_eq!(scalar(&mut other, &count_sql).await, count(2));

        // explicit transaction control in auto mode is tracked
        exec(&mut s, "BEGIN").await;
        assert!(s.in_transaction());
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (5)")).await;
        exec(&mut s, "SAVEPOINT a").await;
        exec(&mut s, &format!("INSERT INTO {schema}.tx VALUES (6)")).await;
        exec(&mut s, "ROLLBACK TO SAVEPOINT a").await;
        assert!(s.in_transaction());
        // metadata in an open transaction does not disturb it
        assert!(!s.list_schemas().await.unwrap().is_empty());
        exec(&mut s, "COMMIT").await;
        assert!(!s.in_transaction());
        assert_eq!(scalar(&mut other, &count_sql).await, count(3));
        // the script fails after START TRANSACTION: the transaction stays open in failed state
        execute_update(&mut *s, "START TRANSACTION; INSERT INTO pg_temp.nonexistent VALUES (1)", &[]).await.unwrap_err();
        assert!(s.in_transaction());
        s.rollback().await.unwrap();
        assert!(!s.in_transaction());
        exec(&mut s, &format!("BEGIN; INSERT INTO {schema}.tx VALUES (7); COMMIT")).await;
        assert!(!s.in_transaction());
        exec(&mut s, "BEGIN; SELECT 1").await;
        assert!(s.in_transaction());
        exec(&mut s, "END").await;
        assert!(!s.in_transaction());
        s.close().await.ok();
        other.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn notices_and_error_details() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        let mut cur = s.execute("DO $$ BEGIN RAISE NOTICE 'hello %', 42; RAISE WARNING 'careful' USING HINT = 'look'; END $$", &[], &ExecOptions::default()).await.unwrap();
        while cur.next_result().await.unwrap().is_some() {}
        let msgs = cur.take_messages();
        assert!(msgs.iter().any(|m| m.severity == "NOTICE" && m.message == "hello 42"), "{msgs:?}");
        let w = msgs.iter().find(|m| m.severity == "WARNING").expect("warning");
        assert_eq!(w.message, "careful");
        assert!(w.detail.as_deref().unwrap_or("").contains("look"));
        cur.close().await.unwrap();

        // unknown table: position of the name
        let e = execute_update(&mut *s, "SELECT * FROM missing_table_xyz", &[]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Query);
        assert_eq!(e.code.as_deref(), Some("42P01"));
        assert_eq!(e.position, Some(15));
        // syntax error
        let e = execute_update(&mut *s, "SELEC 1", &[]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Syntax);
        assert_eq!(e.position, Some(1));
        // error inside a function carries the PL/pgSQL context
        exec(&mut s, &format!("CREATE FUNCTION {schema}.boom() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1 / 0; END $$")).await;
        let e = execute_update(&mut *s, &format!("SELECT {schema}.boom()"), &[]).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("22012"));
        assert!(e.context.as_deref().unwrap_or("").contains("PL/pgSQL function"), "{}", e.full_text());
        // constraint violations name the constraint
        exec(&mut s, &format!("CREATE TABLE {schema}.u (id int CONSTRAINT u_pk PRIMARY KEY)")).await;
        exec(&mut s, &format!("INSERT INTO {schema}.u VALUES (1)")).await;
        let e = execute_update(&mut *s, &format!("INSERT INTO {schema}.u VALUES (1)"), &[]).await.unwrap_err();
        assert!(e.context.as_deref().unwrap_or("").contains("constraint u_pk"), "{}", e.full_text());
        s.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn parameters_round_trip() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        exec(&mut s, &format!("CREATE TYPE {schema}.color AS ENUM ('red', 'green')")).await;
        exec(
            &mut s,
            &format!(
                "CREATE TABLE {schema}.p (id int PRIMARY KEY, b bool, i8 bigint, f8 float8, f4 real, n numeric(20,6), \
                 t text, d date, tm time, ts timestamp, tz timestamptz, ttz timetz, iv interval, u uuid, j json, jb jsonb, \
                 by bytea, ip inet, mac macaddr, bits varbit, ia int[], ta text[], r int4range, c {schema}.color, \
                 m money, x xml, ch char(4), mia int[][])"
            ),
        )
        .await;
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_micro_opt(13, 14, 15, 123_456).unwrap();
        let tz = dt.and_local_timezone(chrono::FixedOffset::east_opt(7 * 3600).unwrap()).unwrap();
        let uuid = uuid::Uuid::parse_str("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap();
        let values = vec![
            Value::Int(1),
            Value::Bool(true),
            Value::Int(i64::MIN + 1),
            Value::Float(-1.25e300),
            Value::Float(1.5),
            Value::decimal("-12345678901234.123456"),
            Value::text("it's \"quoted\" \\ text ünïcödé"),
            Value::Date(dt.date()),
            Value::Time(dt.time()),
            Value::DateTime(dt),
            Value::DateTimeTz(tz),
            Value::text("10:11:12+03"),
            Value::text("1 day 02:03:04"),
            Value::Uuid(uuid),
            Value::json("{\"a\": [1, 2]}"),
            Value::json("{\"b\": {\"c\": null}}"),
            Value::bytes(vec![0u8, 1, 2, 254, 255]),
            Value::text("10.1.2.3/16"),
            Value::text("08:00:2b:01:02:03"),
            Value::text("10101"),
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)].into_boxed_slice()),
            Value::Array(vec![Value::text("a,b"), Value::text("")].into_boxed_slice()),
            Value::text("[5,10)"),
            Value::text("green"),
            Value::decimal("12.34"),
            Value::text("<x>1</x>"),
            Value::text("ab"),
            Value::text("{{1,2},{3,4}}"),
        ];
        let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("${i}")).collect();
        let n = execute_update(&mut *s, &format!("INSERT INTO {schema}.p VALUES ({})", placeholders.join(", ")), &values).await.unwrap_or_else(|e| panic!("{}", e.full_text()));
        assert_eq!(n, Some(1));
        // read back through the parameter path (binary results)
        let (cols, data) = query_all(&mut *s, &format!("SELECT * FROM {schema}.p WHERE id = $1"), &[Value::Int(1)]).await.unwrap_or_else(|e| panic!("{}", e.full_text()));
        let row = &data[0];
        let get = |name: &str| &row[cols.iter().position(|c| c.name == name).unwrap()];
        assert_eq!(get("b"), &Value::Bool(true));
        assert_eq!(get("i8"), &Value::Int(i64::MIN + 1));
        assert_eq!(get("f8"), &Value::Float(-1.25e300));
        assert_eq!(get("f4"), &Value::Float(1.5));
        assert_eq!(get("n"), &Value::decimal("-12345678901234.123456"));
        assert_eq!(get("t"), &values[6]);
        assert_eq!(get("d"), &Value::Date(dt.date()));
        assert_eq!(get("tm"), &Value::Time(dt.time()));
        assert_eq!(get("ts"), &Value::DateTime(dt));
        match get("tz") {
            Value::DateTimeTz(v) => assert_eq!(v.timestamp_micros(), tz.timestamp_micros()),
            other => panic!("{other:?}"),
        }
        assert_eq!(get("ttz"), &Value::text("10:11:12+03"));
        assert_eq!(get("iv"), &Value::text("1 day 02:03:04"));
        assert_eq!(get("u"), &Value::Uuid(uuid));
        assert_eq!(get("j"), &Value::json("{\"a\": [1, 2]}"));
        assert_eq!(get("jb"), &Value::json("{\"b\": {\"c\": null}}"));
        assert_eq!(get("by"), &Value::bytes(vec![0u8, 1, 2, 254, 255]));
        assert_eq!(get("ip"), &Value::text("10.1.2.3/16"));
        assert_eq!(get("mac"), &Value::text("08:00:2b:01:02:03"));
        assert_eq!(get("bits"), &Value::text("10101"));
        assert_eq!(get("ia"), &Value::text("{1,NULL,3}"));
        assert_eq!(get("ta"), &Value::text("{\"a,b\",\"\"}"));
        assert_eq!(get("r"), &Value::text("[5,10)"));
        assert_eq!(get("c"), &Value::text("green"));
        assert_eq!(get("m"), &Value::text("12.34"));
        assert_eq!(get("x"), &Value::text("<x>1</x>"));
        assert_eq!(get("ch"), &Value::text("ab  "));
        assert_eq!(get("mia"), &Value::text("{{1,2},{3,4}}"));
        // the same row through the text protocol renders identically for text kinds
        let text_rows = rows(&mut s, &format!("SELECT ia, ta, r, iv, ttz, mia FROM {schema}.p")).await;
        assert_eq!(text_rows[0], vec![get("ia").clone(), get("ta").clone(), get("r").clone(), get("iv").clone(), get("ttz").clone(), get("mia").clone()]);
        // update and delete with parameters, RETURNING through the parameter path
        let (_, ret) = query_all(
            &mut *s,
            &format!("UPDATE {schema}.p SET t = $1, ia = $2 WHERE id = $3 RETURNING t, ia"),
            &[Value::text("new"), Value::Array(vec![Value::Int(9)].into_boxed_slice()), Value::Int(1)],
        )
        .await
        .unwrap();
        assert_eq!(ret, vec![vec![Value::text("new"), Value::text("{9}")]]);
        let n = execute_update(&mut *s, &format!("UPDATE {schema}.p SET b = $1 WHERE id = $2"), &[Value::Null, Value::Int(1)]).await.unwrap();
        assert_eq!(n, Some(1));
        assert_eq!(scalar(&mut s, &format!("SELECT b IS NULL FROM {schema}.p")).await, Value::Bool(true));
        // untyped parameter (SELECT $1) works too
        assert_eq!(query_scalar(&mut *s, "SELECT $1", &[Value::text("x")]).await.unwrap(), Value::text("x"));
        // a placeholder without parameters is a server error; too many parameters a client error
        let e = execute_update(&mut *s, &format!("DELETE FROM {schema}.p WHERE id = $1"), &[]).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("42P02"), "{}", e.full_text());
        let e = execute_update(&mut *s, &format!("DELETE FROM {schema}.p WHERE id = $1"), &[Value::Int(1), Value::Int(2)]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Query);
        // invalid input is a server error, not a client crash
        let e = execute_update(&mut *s, &format!("DELETE FROM {schema}.p WHERE id = $1"), &[Value::text("abc")]).await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("22P02"));
        let n = execute_update(&mut *s, &format!("DELETE FROM {schema}.p WHERE id = $1"), &[Value::Int(1)]).await.unwrap();
        assert_eq!(n, Some(1));
        s.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn resolve_sources_of_result_columns() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        exec(&mut s, &format!("CREATE TABLE {schema}.src (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text NOT NULL, note text)")).await;
        exec(&mut s, &format!("CREATE VIEW {schema}.src_v AS SELECT id, name FROM {schema}.src")).await;
        let opts = ExecOptions { resolve_sources: true, ..Default::default() };
        for sql in [format!("SELECT id, name, note, 1 AS x FROM {schema}.src"), format!("SELECT id, name, note, 1 AS x FROM {schema}.src WHERE id = $1")] {
            let p = if sql.contains("$1") { vec![Value::Int(1)] } else { Vec::new() };
            let mut cur = s.execute(&sql, &p, &opts).await.unwrap();
            let Some(ResultHeader::Rows(cols)) = cur.next_result().await.unwrap() else { panic!() };
            let src = cols[0].source.as_ref().expect("source of id");
            assert_eq!(src.schema.as_deref(), Some(schema.as_str()));
            assert_eq!(src.table, "src");
            assert_eq!(src.column, "id");
            assert_eq!(cols[0].nullable, Some(false));
            assert_eq!(cols[0].auto_increment, Some(true));
            assert_eq!(cols[1].source.as_ref().unwrap().column, "name");
            assert_eq!(cols[2].nullable, Some(true));
            assert!(cols[3].source.is_none());
            cur.close().await.unwrap();
        }
        // columns of a view resolve to the base table
        let mut cur = s.execute(&format!("SELECT * FROM {schema}.src_v"), &[], &opts).await.unwrap();
        let Some(ResultHeader::Rows(cols)) = cur.next_result().await.unwrap() else { panic!() };
        // PostgreSQL reports the view itself as origin (simple views are updatable)
        assert_eq!(cols[1].source.as_ref().map(|s| s.table.as_str()), Some("src_v"));
        cur.close().await.unwrap();
        // the cache is invalidated by DDL
        exec(&mut s, &format!("ALTER TABLE {schema}.src RENAME COLUMN name TO full_name")).await;
        let mut cur = s.execute(&format!("SELECT full_name FROM {schema}.src"), &[], &opts).await.unwrap();
        let Some(ResultHeader::Rows(cols)) = cur.next_result().await.unwrap() else { panic!() };
        assert_eq!(cols[0].source.as_ref().unwrap().column, "full_name");
        cur.close().await.unwrap();
        s.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn context_and_databases() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        s.set_current_schema(&schema).await.unwrap();
        assert_eq!(s.current_schema().await.unwrap().as_deref(), Some(schema.as_str()));
        s.set_current_schema("public").await.unwrap();
        assert_eq!(s.current_schema().await.unwrap().as_deref(), Some("public"));
        assert_eq!(s.set_current_database("postgres").await.unwrap_err().kind, ErrorKind::Unsupported);
        let dbs = s.list_databases().await.unwrap();
        let cur = dbs.iter().find(|d| d.is_current).expect("current database listed");
        assert_eq!(Some(cur.name.clone()), params.database);
        assert!(cur.size_bytes.is_some());
        assert!(!dbs.iter().any(|d| d.name == "template0" || d.name == "template1"));
        let schemas = s.list_schemas().await.unwrap();
        assert!(schemas.iter().any(|x| x.name == schema && !x.is_system));
        assert!(schemas.iter().any(|x| x.name == "pg_catalog" && x.is_system));
        s.close().await.ok();
    })
    .await;
    with_database(&params, |p, db| async move {
        let mut s = connect(&p).await;
        assert_eq!(s.current_database().await.unwrap().as_deref(), Some(db.as_str()));
        let dbs = s.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d.name == db && d.is_current));
        s.close().await.ok();
    })
    .await;
}

// ----------------------------------------------------------------- metadata

const RICH_SCHEMA: &str = r#"
CREATE TYPE {s}.status AS ENUM ('new', 'paid', 'shipped');
CREATE TYPE {s}.money_pair AS (amount numeric(12,2), currency char(3));
CREATE TYPE {s}.floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
CREATE DOMAIN {s}.positive_int AS integer NOT NULL DEFAULT 1 CONSTRAINT positive_check CHECK (VALUE > 0);
CREATE SEQUENCE {s}.ticket_seq AS integer INCREMENT BY 5 START WITH 100 CACHE 10;
CREATE TABLE {s}.customers (
    id integer GENERATED ALWAYS AS IDENTITY,
    code varchar(20) COLLATE "C" NOT NULL,
    name text NOT NULL,
    email text,
    credit numeric(12,2) DEFAULT 0 NOT NULL,
    created_at timestamptz DEFAULT now() NOT NULL,
    CONSTRAINT customers_pkey PRIMARY KEY (id),
    CONSTRAINT customers_code_key UNIQUE (code),
    CONSTRAINT customers_credit_check CHECK (credit >= 0)
) WITH (fillfactor = 80);
COMMENT ON TABLE {s}.customers IS 'Customers, it''s important';
COMMENT ON COLUMN {s}.customers.email IS 'Contact address';
CREATE TABLE {s}.orders (
    id bigserial PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES {s}.customers (id) ON DELETE CASCADE ON UPDATE RESTRICT,
    status {s}.status DEFAULT 'new' NOT NULL,
    qty {s}.positive_int,
    price numeric(10,2) NOT NULL,
    total numeric GENERATED ALWAYS AS (qty * price) STORED,
    tags text[],
    note text,
    ticket integer DEFAULT nextval('{s}.ticket_seq'),
    CONSTRAINT orders_price_check CHECK (price > 0::numeric)
);
CREATE INDEX orders_customer_idx ON {s}.orders (customer_id, status DESC);
CREATE INDEX orders_note_lower_idx ON {s}.orders (lower(note)) WHERE note IS NOT NULL;
CREATE UNIQUE INDEX orders_ticket_uidx ON {s}.orders (ticket) INCLUDE (status);
CREATE INDEX orders_tags_gin ON {s}.orders USING gin (tags);
COMMENT ON INDEX {s}.orders_customer_idx IS 'lookup by customer';
CREATE FUNCTION {s}.touch_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.note := coalesce(NEW.note, 'n/a');
  RETURN NEW;
END $$;
COMMENT ON FUNCTION {s}.touch_note() IS 'fills the note';
CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE OF note ON {s}.orders FOR EACH ROW EXECUTE FUNCTION {s}.touch_note();
CREATE TRIGGER orders_stmt AFTER DELETE ON {s}.orders FOR EACH STATEMENT EXECUTE FUNCTION {s}.touch_note();
ALTER TABLE {s}.orders DISABLE TRIGGER orders_stmt;
COMMENT ON TRIGGER orders_touch ON {s}.orders IS 'touch';
CREATE TABLE {s}.events (
    id bigint NOT NULL,
    happened date NOT NULL,
    payload jsonb,
    CONSTRAINT events_pkey PRIMARY KEY (id, happened)
) PARTITION BY RANGE (happened);
CREATE TABLE {s}.events_2024 PARTITION OF {s}.events FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE {s}.events_2025 PARTITION OF {s}.events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE {s}.events_default PARTITION OF {s}.events DEFAULT;
CREATE INDEX events_payload_idx ON {s}.events USING gin (payload);
CREATE VIEW {s}.big_customers WITH (security_barrier = true) AS
    SELECT c.id, c.name, sum(o.price) AS spent FROM {s}.customers c JOIN {s}.orders o ON o.customer_id = c.id GROUP BY c.id, c.name;
COMMENT ON VIEW {s}.big_customers IS 'top';
CREATE MATERIALIZED VIEW {s}.order_stats AS SELECT status, count(*) AS n FROM {s}.orders GROUP BY status;
CREATE UNIQUE INDEX order_stats_status ON {s}.order_stats (status);
CREATE FUNCTION {s}.add(a integer, b integer DEFAULT 1) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT a + b';
CREATE FUNCTION {s}.add(a text, b text) RETURNS text LANGUAGE sql AS 'SELECT a || b';
CREATE PROCEDURE {s}.reset_credit(IN cid integer) LANGUAGE plpgsql AS $$ BEGIN UPDATE {s}.customers SET credit = 0 WHERE id = cid; END $$;
CREATE AGGREGATE {s}.mysum(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0');
"#;

async fn create_rich_schema(s: &mut Box<dyn Session>, schema: &str) {
    let mut sql = RICH_SCHEMA.replace("{s}", schema);
    if major(s.as_ref()) < 12 {
        sql = sql.replace(" GENERATED ALWAYS AS (qty * price) STORED", "");
    }
    exec(s, &sql).await;
}

#[tokio::test]
async fn metadata_of_every_object_kind() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        create_rich_schema(&mut s, &schema).await;
        exec(&mut s, &format!("ANALYZE {schema}.customers")).await;
        let sr = SchemaRef::schema(schema.clone());
        let names = |v: &[dbiji_core::meta::ObjectInfo]| v.iter().map(|o| o.obj.name.clone()).collect::<Vec<_>>();

        let tables = s.list_objects(&sr, ObjectKind::Table).await.unwrap();
        assert_eq!(names(&tables), vec!["customers", "events", "orders"], "partitions are not listed");
        assert_eq!(tables.iter().find(|t| t.obj.name == "events").unwrap().detail.as_deref(), Some("partitioned"));
        // Sizes for the navigator: a table with its indexes, a partitioned one its partitions'.
        assert!(tables.iter().all(|t| t.size_bytes.is_some_and(|b| b > 0)), "{tables:?}");
        assert_eq!(tables[0].comment.as_deref(), Some("Customers, it's important"));
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::View).await.unwrap()), vec!["big_customers"]);
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::MaterializedView).await.unwrap()), vec!["order_stats"]);
        let seqs = s.list_objects(&sr, ObjectKind::Sequence).await.unwrap();
        assert!(names(&seqs).contains(&"ticket_seq".to_string()));
        assert!(names(&seqs).contains(&"orders_id_seq".to_string()));
        let funcs = s.list_objects(&sr, ObjectKind::Function).await.unwrap();
        let adds: Vec<_> = funcs.iter().filter(|f| f.obj.name == "add").collect();
        assert_eq!(adds.len(), 2);
        assert!(adds.iter().any(|f| f.obj.signature.as_deref() == Some("(integer, integer)")));
        assert!(adds.iter().any(|f| f.detail.as_deref() == Some("(a text, b text) → text")));
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::Procedure).await.unwrap()), vec!["reset_credit"]);
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::Aggregate).await.unwrap()), vec!["mysum"]);
        let types = s.list_objects(&sr, ObjectKind::Type).await.unwrap();
        let tn = names(&types);
        assert!(tn.contains(&"status".to_string()) && tn.contains(&"money_pair".to_string()) && tn.contains(&"floatrange".to_string()), "{tn:?}");
        assert!(!tn.iter().any(|t| t == "customers" || t.starts_with('_')), "no row or array types: {tn:?}");
        let domains = s.list_objects(&sr, ObjectKind::Domain).await.unwrap();
        assert_eq!(names(&domains), vec!["positive_int"]);
        assert_eq!(domains[0].detail.as_deref(), Some("integer NOT NULL"));
        let exts = s.list_objects(&SchemaRef::default(), ObjectKind::Extension).await.unwrap();
        assert!(exts.iter().any(|e| e.obj.name == "plpgsql"));

        // children
        let orders = ObjectRef::table(Some(schema.clone()), "orders");
        let cols = s.list_children(&orders, ObjectKind::Column).await.unwrap();
        assert_eq!(cols.len(), 9);
        assert_eq!(cols[0].detail, Some(format!("bigint NOT NULL DEFAULT nextval('{schema}.orders_id_seq'::regclass) (PK)")));
        assert_eq!(cols[0].obj.table.as_deref(), Some("orders"));
        let idx = s.list_children(&orders, ObjectKind::Index).await.unwrap();
        assert_eq!(idx.len(), 5, "{:?}", names(&idx));
        assert!(idx.iter().any(|i| i.detail.as_deref().unwrap_or("").starts_with("gin (tags)")));
        assert_eq!(names(&s.list_children(&orders, ObjectKind::PrimaryKey).await.unwrap()), vec!["orders_pkey"]);
        assert_eq!(names(&s.list_children(&orders, ObjectKind::CheckConstraint).await.unwrap()), vec!["orders_price_check"]);
        let fks = s.list_children(&orders, ObjectKind::ForeignKey).await.unwrap();
        assert_eq!(fks.len(), 1);
        assert!(fks[0].detail.as_deref().unwrap().contains("ON DELETE CASCADE"));
        let trg = s.list_children(&orders, ObjectKind::Trigger).await.unwrap();
        assert_eq!(names(&trg), vec!["orders_stmt", "orders_touch"]);
        assert!(trg[0].detail.as_deref().unwrap().ends_with("(disabled)"));
        assert!(trg[1].detail.as_deref().unwrap().starts_with("BEFORE INSERT OR UPDATE OF note ON"), "{:?}", trg[1].detail);
        let credit = s.list_children(&ObjectRef::table(Some(schema.clone()), "customers"), ObjectKind::Column).await.unwrap();
        assert_eq!(credit[4].detail.as_deref(), Some("numeric(12,2) NOT NULL DEFAULT 0"));
        let generated = major(s.as_ref()) >= 12;
        assert!(!generated || cols[5].detail.as_deref().unwrap().contains("GENERATED ALWAYS AS"), "{:?}", cols[5].detail);
        let customers = ObjectRef::table(Some(schema.clone()), "customers");
        assert_eq!(names(&s.list_children(&customers, ObjectKind::UniqueKey).await.unwrap()), vec!["customers_code_key"]);
        let events = ObjectRef::table(Some(schema.clone()), "events");
        let parts = s.list_children(&events, ObjectKind::Partition).await.unwrap();
        assert_eq!(names(&parts), vec!["events_2024", "events_2025", "events_default"]);
        assert_eq!(parts[2].detail.as_deref(), Some("DEFAULT"));
        assert!(parts.iter().all(|p| p.size_bytes.is_some_and(|b| b >= 0)), "partitions have their size: {:?}", parts.iter().map(|p| p.size_bytes).collect::<Vec<_>>());

        // table definition
        let def = s.table_definition(&orders).await.unwrap();
        assert_eq!(def.columns.len(), 9);
        let id = def.column("id").unwrap();
        assert!(id.auto_increment && !id.nullable);
        assert!(id.default.as_deref().unwrap().starts_with("nextval("));
        assert_eq!(def.column("total").unwrap().generated.as_deref(), generated.then_some("((qty)::numeric * price)"));
        let ticket = def.column("ticket").unwrap();
        assert!(!ticket.auto_increment, "a default from a standalone sequence is not serial");
        assert_eq!(ticket.default.as_deref(), Some(format!("nextval('{schema}.ticket_seq'::regclass)").as_str()));
        assert_eq!(def.column("status").unwrap().data_type, format!("{schema}.status"));
        assert_eq!(def.primary_key.as_ref().unwrap().columns, vec!["id"]);
        assert_eq!(def.foreign_keys[0].ref_table, "customers");
        assert_eq!(def.foreign_keys[0].on_delete, dbiji_core::ddl::FkAction::Cascade);
        assert_eq!(def.foreign_keys[0].on_update, dbiji_core::ddl::FkAction::Restrict);
        assert_eq!(def.indexes.len(), 5);
        let expr = def.indexes.iter().find(|i| i.name == "orders_note_lower_idx").unwrap();
        assert!(expr.columns[0].is_expression);
        assert_eq!(expr.predicate.as_deref(), Some("(note IS NOT NULL)"));
        let cov = def.indexes.iter().find(|i| i.name == "orders_ticket_uidx").unwrap();
        assert_eq!(cov.include, vec!["status"]);
        let desc = def.indexes.iter().find(|i| i.name == "orders_customer_idx").unwrap();
        assert!(desc.columns[1].descending);
        assert_eq!(desc.comment.as_deref(), Some("lookup by customer"));
        assert_eq!(def.triggers.len(), 2);
        assert!(!def.triggers.iter().find(|t| t.name == "orders_stmt").unwrap().enabled);
        let cdef = s.table_definition(&customers).await.unwrap();
        assert_eq!(cdef.referenced_by.len(), 1);
        assert_eq!(cdef.referenced_by[0].table, "orders");
        assert_eq!(cdef.options.get("with").map(String::as_str), Some("fillfactor=80"));
        assert_eq!(cdef.column("code").unwrap().collation.as_deref(), Some("C"));
        assert_eq!(cdef.column("id").unwrap().default.as_deref(), Some("GENERATED ALWAYS AS IDENTITY"));
        assert!(cdef.size_bytes.unwrap_or(0) > 0);
        let edef = s.table_definition(&events).await.unwrap();
        assert_eq!(edef.partitioning.as_ref().unwrap().strategy, "RANGE");
        assert_eq!(edef.partitioning.as_ref().unwrap().key, "happened");
        assert_eq!(edef.partitions.len(), 3);

        // views, routines, sequences
        let v = s.view_definition(&ObjectRef::new(ObjectKind::View, Some(schema.clone()), "big_customers")).await.unwrap();
        assert!(!v.materialized && v.definition.to_lowercase().contains("sum(o.price)"));
        assert_eq!(v.options.get("security_barrier").map(String::as_str), Some("true"));
        assert_eq!(v.columns.len(), 3);
        let mv = s.view_definition(&ObjectRef::new(ObjectKind::MaterializedView, Some(schema.clone()), "order_stats")).await.unwrap();
        assert!(mv.materialized);
        let add_int = ObjectRef::new(ObjectKind::Function, Some(schema.clone()), "add").with_signature("(integer, integer)");
        let r = s.routine_definition(&add_int).await.unwrap();
        assert_eq!(r.arguments, "a integer, b integer");
        assert!(r.definition.starts_with("CREATE OR REPLACE FUNCTION"));
        let add_txt = ObjectRef::new(ObjectKind::Function, Some(schema.clone()), "add").with_signature("(text, text)");
        assert_eq!(s.routine_definition(&add_txt).await.unwrap().returns.as_deref(), Some("text"));
        let agg = s.routine_definition(&ObjectRef::new(ObjectKind::Aggregate, Some(schema.clone()), "mysum")).await.unwrap();
        assert!(agg.definition.contains("SFUNC = int4pl") && agg.definition.contains("INITCOND = '0'"), "{}", agg.definition);
        let seq = s.sequence_definition(&ObjectRef::new(ObjectKind::Sequence, Some(schema.clone()), "ticket_seq")).await.unwrap();
        assert_eq!((seq.start, seq.increment, seq.cache), (Some(100), Some(5), Some(10)));
        assert_eq!(seq.data_type.as_deref(), Some("integer"));
        let oseq = s.sequence_definition(&ObjectRef::new(ObjectKind::Sequence, Some(schema.clone()), "orders_id_seq")).await.unwrap();
        assert_eq!(oseq.owned_by, Some(format!("{schema}.orders.id")));
        assert_eq!((seq.min_value, seq.max_value), (None, None), "defaults are omitted");
        let deps = s.object_dependencies(&orders).await.unwrap();
        assert!(deps.iter().any(|d| d.target.kind == ObjectKind::Trigger && d.target.name == "orders_touch" && d.target.table.as_deref() == Some("orders")), "{deps:?}");
        let ddl = s.object_ddl(&ObjectRef::child(ObjectKind::Index, &events, "events_payload_idx")).await.unwrap();
        assert!(ddl.starts_with(&format!("CREATE INDEX events_payload_idx ON {schema}.events USING gin")), "{ddl}");
        let ddl = s.object_ddl(&events).await.unwrap();
        assert!(!ddl.contains("ON ONLY"), "{ddl}");

        // DDL of every kind
        let ddl_of = |kind: ObjectKind, name: &str| ObjectRef::new(kind, Some(schema.clone()), name);
        let mut objs = vec![
            ddl_of(ObjectKind::Table, "orders"),
            ddl_of(ObjectKind::Table, "events"),
            ddl_of(ObjectKind::Partition, "events_2024"),
            ddl_of(ObjectKind::View, "big_customers"),
            ddl_of(ObjectKind::MaterializedView, "order_stats"),
            add_int.clone(),
            ddl_of(ObjectKind::Procedure, "reset_credit"),
            ddl_of(ObjectKind::Aggregate, "mysum"),
            ddl_of(ObjectKind::Sequence, "ticket_seq"),
            ddl_of(ObjectKind::Type, "status"),
            ddl_of(ObjectKind::Type, "money_pair"),
            ddl_of(ObjectKind::Type, "floatrange"),
            ddl_of(ObjectKind::Domain, "positive_int"),
            ObjectRef::new(ObjectKind::Extension, None, "plpgsql"),
            ObjectRef::new(ObjectKind::Schema, None, schema.clone()),
            ObjectRef::new(ObjectKind::Database, None, params.database.clone().unwrap()),
        ];
        objs.push(ObjectRef::child(ObjectKind::Index, &orders, "orders_note_lower_idx"));
        objs.push(ObjectRef::child(ObjectKind::Trigger, &orders, "orders_touch"));
        objs.push(ObjectRef::child(ObjectKind::ForeignKey, &orders, "orders_customer_id_fkey"));
        objs.push(ObjectRef::child(ObjectKind::Column, &orders, "note"));
        for o in &objs {
            let ddl = s.object_ddl(o).await.unwrap_or_else(|e| panic!("DDL of {o}: {}", e.full_text()));
            assert!(ddl.trim_end().ends_with(';'), "{o}: {ddl}");
            assert!(ddl.contains("CREATE") || ddl.contains("ALTER"), "{o}: {ddl}");
        }
        let ddl = s.object_ddl(&ddl_of(ObjectKind::Type, "status")).await.unwrap();
        assert_eq!(ddl, format!("CREATE TYPE {schema}.status AS ENUM ('new', 'paid', 'shipped');\n"));
        let ddl = s.object_ddl(&ddl_of(ObjectKind::Domain, "positive_int")).await.unwrap();
        assert!(ddl.contains("DEFAULT 1 NOT NULL") && ddl.contains("CONSTRAINT positive_check CHECK"), "{ddl}");
        let ddl = s.object_ddl(&ddl_of(ObjectKind::Partition, "events_2024")).await.unwrap();
        assert!(ddl.starts_with(&format!("CREATE TABLE {schema}.events_2024 PARTITION OF {schema}.events FOR VALUES")), "{ddl}");
        assert!(!ddl.contains("events_2024_payload_idx"), "inherited index must not be repeated: {ddl}");
        let ddl = s.object_ddl(&ddl_of(ObjectKind::MaterializedView, "order_stats")).await.unwrap();
        assert!(ddl.contains("CREATE UNIQUE INDEX order_stats_status"), "{ddl}");

        // properties, dependencies, statistics
        let props = s.object_properties(&customers).await.unwrap();
        assert!(props.iter().any(|p| p.name == "Owner"));
        assert!(props.iter().any(|p| p.name == "Comment" && p.value.contains("important")));
        assert!(props.iter().any(|p| p.name == "Options" && p.value.contains("fillfactor")));
        for o in [
            &add_int,
            &ddl_of(ObjectKind::Type, "status"),
            &ddl_of(ObjectKind::Sequence, "ticket_seq"),
            &ObjectRef::new(ObjectKind::Schema, None, schema.clone()),
            &ObjectRef::child(ObjectKind::Column, &orders, "status"),
            &ObjectRef::child(ObjectKind::Index, &orders, "orders_tags_gin"),
            &ObjectRef::child(ObjectKind::Trigger, &orders, "orders_touch"),
            &ObjectRef::child(ObjectKind::CheckConstraint, &orders, "orders_price_check"),
            &ObjectRef::new(ObjectKind::Database, None, params.database.clone().unwrap()),
            &ObjectRef::new(ObjectKind::Extension, None, "plpgsql"),
        ] {
            let p = s.object_properties(o).await.unwrap_or_else(|e| panic!("{o}: {}", e.full_text()));
            assert!(!p.is_empty(), "no properties for {o}");
        }
        let deps = s.object_dependencies(&ObjectRef::new(ObjectKind::View, Some(schema.clone()), "big_customers")).await.unwrap();
        assert!(deps.iter().any(|d| d.direction == dbiji_core::meta::DependencyDirection::DependsOn && d.target.name == "orders"), "{deps:?}");
        let deps = s.object_dependencies(&customers).await.unwrap();
        assert!(deps.iter().any(|d| d.direction == dbiji_core::meta::DependencyDirection::UsedBy && d.target.name == "big_customers"), "{deps:?}");
        assert!(deps.iter().any(|d| d.kind.as_deref().unwrap_or("").starts_with("foreign key") && d.target.name == "orders"), "{deps:?}");
        let deps = s.object_dependencies(&orders).await.unwrap();
        assert!(deps.iter().any(|d| d.target.name == "status" && d.target.kind == ObjectKind::Type), "{deps:?}");
        let stats = s.object_statistics(&customers).await.unwrap();
        assert!(stats.iter().any(|p| p.name == "seq_scan"), "{stats:?}");
        assert!(stats.iter().any(|p| p.name == "total_size"));
        assert!(!s.object_statistics(&ObjectRef::child(ObjectKind::Index, &orders, "orders_pkey")).await.unwrap().is_empty());

        // search
        let found = s.search_metadata(&MetadataSearch { pattern: "custom*".into(), schemas: vec![schema.clone()], ..Default::default() }).await.unwrap();
        assert!(found.iter().any(|o| o.obj.kind == ObjectKind::Table && o.obj.name == "customers"));
        assert!(found.iter().any(|o| o.obj.kind == ObjectKind::Column && o.obj.name == "customer_id" && o.obj.table.as_deref() == Some("orders")));
        let found = s
            .search_metadata(&MetadataSearch {
                pattern: "important".into(),
                schemas: vec![schema.clone()],
                search_comments: true,
                kinds: vec![ObjectKind::Table],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(names(&found), vec!["customers"]);
        let only_columns =
            s.search_metadata(&MetadataSearch { pattern: "credit".into(), schemas: vec![schema.clone()], kinds: vec![ObjectKind::Column], ..Default::default() }).await.unwrap();
        assert_eq!(only_columns.len(), 1);
        assert_eq!(only_columns[0].obj.table.as_deref(), Some("customers"));
        let routines = s.search_metadata(&MetadataSearch { pattern: "ADD".into(), kinds: vec![ObjectKind::Function], ..Default::default() }).await.unwrap();
        assert!(routines.iter().any(|o| o.obj.schema.as_deref() == Some(schema.as_str()) && o.obj.signature.is_some()));
        let limited = s.search_metadata(&MetadataSearch { pattern: "*".into(), schemas: vec![schema.clone()], limit: 3, ..Default::default() }).await.unwrap();
        assert_eq!(limited.len(), 3);

        // snapshot
        let snap = s.schema_snapshot(&sr, &SnapshotOptions::full()).await.unwrap();
        assert_eq!(snap.tables.len(), 3);
        assert_eq!(snap.views.len(), 2);
        assert!(snap.views.iter().all(|v| !v.definition.is_empty()));
        assert!(snap.sequences.len() >= 3);
        assert!(snap.routines.len() >= 5);
        assert!(snap.types.iter().any(|t| t.category == "enum" && t.elements == vec!["new", "paid", "shipped"]));
        assert!(snap.types.iter().any(|t| t.category == "domain"));
        let light = s.schema_snapshot(&sr, &SnapshotOptions::light()).await.unwrap();
        assert!(light.tables.iter().all(|t| t.indexes.is_empty() && t.triggers.is_empty()));
        assert!(light.tables.iter().any(|t| !t.foreign_keys.is_empty()));
        assert!(light.views.iter().all(|v| v.definition.is_empty()));
        s.close().await.ok();
    })
    .await;
}

/// Replace schema references so definitions from two schemas can be compared.
fn normalize(def: &TableDef, from: &str, to: &str) -> TableDef {
    let mut d = def.clone();
    let fix = |s: &str| s.replace(&format!("{from}."), &format!("{to}."));
    d.schema = Some(to.to_string());
    for c in &mut d.columns {
        c.data_type = fix(&c.data_type);
        c.default = c.default.as_deref().map(fix);
    }
    for fk in d.foreign_keys.iter_mut().chain(d.referenced_by.iter_mut()) {
        if fk.ref_schema.as_deref() == Some(from) {
            fk.ref_schema = Some(to.to_string());
        }
        if fk.schema.as_deref() == Some(from) {
            fk.schema = Some(to.to_string());
        }
    }
    for i in &mut d.indexes {
        i.definition = i.definition.as_deref().map(fix);
    }
    for t in &mut d.triggers {
        t.definition = fix(&t.definition);
        t.body = t.body.as_deref().map(fix);
    }
    for v in d.options.values_mut() {
        *v = fix(v);
    }
    d.size_bytes = None;
    d.row_estimate = None;
    for p in &mut d.partitions {
        p.row_estimate = None;
    }
    d
}

#[tokio::test]
async fn ddl_round_trip_into_second_schema() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, s1| async move {
        let s2 = format!("{s1}_copy");
        let mut s = connect(&params).await;
        create_rich_schema(&mut s, &s1).await;
        exec(&mut s, &format!("CREATE SCHEMA {s2}")).await;
        // types, sequences and functions used by the tables are created from their DDL first
        let snap = s.schema_snapshot(&SchemaRef::schema(s1.clone()), &SnapshotOptions::full()).await.unwrap();
        let ddl = PgDdl::new();
        let retarget = |sql: &str| sql.replace(&format!("{s1}."), &format!("{s2}."));
        for t in &snap.types {
            let mut t = t.clone();
            t.schema = Some(s2.clone());
            t.definition = retarget(&t.definition);
            for stmt in ddl.create_type(&t) {
                exec(&mut s, &stmt).await;
            }
        }
        for q in snap.sequences.iter().filter(|q| q.owned_by.is_none()) {
            let mut q = q.clone();
            q.schema = Some(s2.clone());
            exec(&mut s, &ddl.create_sequence(&q)).await;
        }
        let trig_fn = snap.routines.iter().find(|r| r.name == "touch_note").unwrap();
        exec(&mut s, &retarget(&trig_fn.definition)).await;
        // tables in dependency order without FKs, FKs afterwards
        let order = ["customers", "orders", "events"];
        let mut fks = Vec::new();
        for name in order {
            let def = s.table_definition(&ObjectRef::table(Some(s1.clone()), name)).await.unwrap();
            let target = normalize(&def, &s1, &s2);
            for stmt in ddl.create_table(&target, false) {
                exec(&mut s, &stmt).await;
            }
            for fk in &target.foreign_keys {
                fks.push(ddl.add_foreign_key(&target, fk));
            }
        }
        for f in fks {
            exec(&mut s, &f).await;
        }
        for name in order {
            let a = s.table_definition(&ObjectRef::table(Some(s1.clone()), name)).await.unwrap();
            let b = s.table_definition(&ObjectRef::table(Some(s2.clone()), name)).await.unwrap();
            let a = normalize(&a, &s1, &s2);
            let mut b = normalize(&b, &s2, &s2);
            // sequence of serial columns is recreated with the same name in the new schema
            assert_eq!(a.columns, b.columns, "columns of {name}");
            assert_eq!(a.primary_key, b.primary_key, "pk of {name}");
            assert_eq!(a.unique_keys, b.unique_keys, "unique keys of {name}");
            assert_eq!(a.checks, b.checks, "checks of {name}");
            assert_eq!(a.foreign_keys, b.foreign_keys, "fks of {name}");
            assert_eq!(a.indexes, b.indexes, "indexes of {name}");
            assert_eq!(a.triggers, b.triggers, "triggers of {name}");
            assert_eq!(a.partitioning, b.partitioning, "partitioning of {name}");
            assert_eq!(a.partitions, b.partitions, "partitions of {name}");
            assert_eq!(a.comment, b.comment, "comment of {name}");
            assert_eq!(a.options, b.options, "options of {name}");
            b.referenced_by.clone_from(&a.referenced_by);
            assert_eq!(a, b, "definition of {name}");
        }
        // the partitioned index reaches every partition and is valid
        let r = rows(
            &mut s,
            &format!(
                "SELECT count(*) FILTER (WHERE NOT x.indisvalid), count(*) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
                 JOIN pg_namespace n ON n.oid = i.relnamespace WHERE n.nspname = '{s2}' AND i.relname LIKE 'events%payload%'"
            ),
        )
        .await;
        assert_eq!(r[0], vec![Value::Int(0), Value::Int(4)], "parent + 3 partitions, all valid");
        // alter_table produces executable statements
        let from = s.table_definition(&ObjectRef::table(Some(s2.clone()), "customers")).await.unwrap();
        let mut to = from.clone();
        to.columns.retain(|c| c.name != "email");
        to.columns.iter_mut().find(|c| c.name == "name").unwrap().data_type = "character varying(200)".into();
        to.columns.iter_mut().find(|c| c.name == "credit").unwrap().default = Some("10".into());
        let mut extra = dbiji_core::ddl::ColumnDef::new("phone", "text");
        extra.comment = Some("phone number".into());
        to.columns.push(extra);
        to.checks.clear();
        to.comment = Some("changed".into());
        for stmt in ddl.alter_table(&from, &to, &[]) {
            exec(&mut s, &stmt).await;
        }
        let after = s.table_definition(&ObjectRef::table(Some(s2.clone()), "customers")).await.unwrap();
        assert!(after.column("email").is_none());
        assert_eq!(after.column("name").unwrap().data_type, "character varying(200)");
        assert_eq!(after.column("credit").unwrap().default.as_deref(), Some("10"));
        assert_eq!(after.column("phone").unwrap().comment.as_deref(), Some("phone number"));
        assert!(after.checks.is_empty());
        assert_eq!(after.comment.as_deref(), Some("changed"));
        exec(&mut s, &format!("DROP SCHEMA {s2} CASCADE")).await;
        s.close().await.ok();
    })
    .await;
}

// -------------------------------------------------------------------- plans

#[tokio::test]
async fn explain_and_analyze() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        exec(&mut s, &format!("CREATE TABLE {schema}.e (id int PRIMARY KEY, v int); INSERT INTO {schema}.e SELECT g, g FROM generate_series(1, 1000) g; ANALYZE {schema}.e")).await;
        let plan = s.explain(&format!("SELECT * FROM {schema}.e a JOIN {schema}.e b ON a.id = b.v WHERE a.v > 10"), false).await.unwrap();
        assert!(!plan.analyzed);
        assert_eq!(plan.roots.len(), 1);
        assert!(plan.roots[0].total_cost.unwrap() > 0.0);
        assert!(plan.text.contains("Join") || plan.text.contains("Nested Loop"), "{}", plan.text);
        assert!(plan.raw.as_deref().unwrap().starts_with('['));
        let mut relations = Vec::new();
        plan.roots[0].walk(0, &mut |n, _| {
            if let Some(r) = &n.relation {
                relations.push(r.clone());
            }
        });
        assert!(relations.iter().all(|r| r == "e"), "{relations:?}");
        assert!(plan.roots[0].count() >= 3);
        // ANALYZE of a write runs in a rolled back transaction
        let plan = s.explain(&format!("UPDATE {schema}.e SET v = v + 1 WHERE id <= 10"), true).await.unwrap();
        assert!(plan.analyzed);
        assert!(plan.execution_ms.is_some() && plan.planning_ms.is_some());
        assert!(plan.roots[0].actual_total_ms.is_some());
        assert!(plan.text.contains("Execution Time"));
        assert_eq!(scalar(&mut s, &format!("SELECT sum(v) FROM {schema}.e WHERE id <= 10")).await, Value::Int(55));
        assert!(!s.in_transaction());
        // inside a user transaction a savepoint protects it
        exec(&mut s, "BEGIN").await;
        exec(&mut s, &format!("INSERT INTO {schema}.e VALUES (5000, 1)")).await;
        s.explain(&format!("DELETE FROM {schema}.e"), true).await.unwrap();
        assert!(s.in_transaction());
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM {schema}.e")).await, Value::Int(1001));
        exec(&mut s, "ROLLBACK").await;
        // statements with placeholders use a generic plan (PostgreSQL 16+)
        if s.server_info().at_least(16, 0) {
            let p = s.explain(&format!("SELECT * FROM {schema}.e WHERE id = $1"), false).await.unwrap();
            assert!(p.roots[0].node_type.contains("Index") || p.text.contains("Index"), "{}", p.text);
        }
        // errors keep the position within the user's statement
        let e = s.explain("SELECT * FROM missing_tbl_for_explain", false).await.unwrap_err();
        assert_eq!(e.position, Some(15));
        s.close().await.ok();
    })
    .await;
}

// -------------------------------------------------------------------- admin

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn sessions_locks_and_metrics() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut admin = connect(&params).await;
        let mut holder = connect(&params).await;
        let waiter_params = params.clone();
        exec(&mut admin, &format!("CREATE TABLE {schema}.lk (id int)")).await;
        let my_pid: i64 = admin.server_info().session_id.as_deref().unwrap().parse().unwrap();
        let sessions = admin.list_sessions().await.unwrap();
        let pid_col = sessions.id_column;
        assert!(sessions.data.rows.iter().any(|r| r[pid_col] == Value::Int(my_pid)));
        assert!(sessions.query_column.is_some() && sessions.blocked_by_column.is_some());

        exec(&mut holder, "BEGIN").await;
        exec(&mut holder, &format!("LOCK TABLE {schema}.lk IN ACCESS EXCLUSIVE MODE")).await;
        let holder_pid: i64 = holder.server_info().session_id.as_deref().unwrap().parse().unwrap();
        let lk = format!("{schema}.lk");
        let waiter = tokio::spawn(async move {
            let mut w = connect(&waiter_params).await;
            let pid: i64 = w.server_info().session_id.as_deref().unwrap().parse().unwrap();
            let r = execute_update(&mut *w, &format!("SELECT * FROM {lk}"), &[]).await;
            w.close().await.ok();
            (pid, r)
        });
        // wait until the waiter is blocked
        let mut blocked = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let locks = admin.list_locks().await.unwrap();
            let gcol = locks.granted_column.unwrap();
            let bcol = locks.blocked_by_column.unwrap();
            if let Some(r) = locks.data.rows.iter().find(|r| r[gcol] == Value::Bool(false)) {
                blocked = Some((r[locks.session_column].clone(), r[bcol].clone()));
                break;
            }
        }
        let (waiting_pid, blocked_by) = blocked.expect("a waiting lock");
        assert_eq!(blocked_by, Value::text(holder_pid.to_string()));
        let sessions = admin.list_sessions().await.unwrap();
        let bcol = sessions.blocked_by_column.unwrap();
        assert!(sessions.data.rows.iter().any(|r| r[pid_col] == waiting_pid && r[bcol] == Value::text(holder_pid.to_string())));
        // cancel the waiting query
        let pid_text = waiting_pid.to_text().into_owned();
        admin.kill_session(&pid_text, KillMode::CancelQuery).await.unwrap();
        let (pid, r) = waiter.await.unwrap();
        assert_eq!(Value::Int(pid), waiting_pid);
        assert_eq!(r.unwrap_err().kind, ErrorKind::Cancelled);
        exec(&mut holder, "ROLLBACK").await;
        // terminate a session
        admin.kill_session(&holder_pid.to_string(), KillMode::Terminate).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let e = holder.ping().await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Connection);
        assert!(holder.is_closed());
        assert!(admin.kill_session("not-a-pid", KillMode::Terminate).await.is_err());
        // metrics
        let metrics = admin.server_metrics().await.unwrap();
        for key in ["connections.total", "xact.commit", "io.cache_hit_ratio", "db.size", "locks.total", "tuples.returned"] {
            assert!(metrics.iter().any(|m| m.key == key), "missing metric {key}");
        }
        assert!(metrics.iter().find(|m| m.key == "db.size").unwrap().value > 0.0);
        admin.close().await.ok();
    })
    .await;
}

#[tokio::test]
async fn backup_and_restore_with_local_tools() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let driver = PostgresDriver::new();
    let probe = driver.backup_command(&params, &BackupOptions { output: "/tmp/x".into(), ..Default::default() }).unwrap();
    if !std::path::Path::new(&probe.program).is_file() {
        eprintln!("pg_dump not installed; skipping");
        return;
    }
    let tool_major: u32 = std::process::Command::new(&probe.program)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .map(|v| dbiji_core::driver::parse_version_number(&v).0)
        .unwrap_or(0);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        let skew = tool_major > s.server_info().version_number.0;
        exec(&mut s, &format!("CREATE TABLE {schema}.bk (id int PRIMARY KEY, v text); INSERT INTO {schema}.bk VALUES (1, 'one'), (2, 'it''s two')")).await;
        s.close().await.ok();
        let dir = std::env::temp_dir().join(format!("dbiji_pgdump_{}", suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, file) in [(BackupFormat::Plain, "dump.sql"), (BackupFormat::Custom, "dump.custom")] {
            let out = dir.join(file);
            let cmd = driver.backup_command(&params, &BackupOptions { output: out.clone(), format, schemas: vec![schema.clone()], no_owner: true, ..Default::default() }).unwrap();
            let status = run(&cmd);
            assert!(status, "pg_dump failed: {}", cmd.display());
            assert!(out.is_file());
            with_database(&params, |p, _db| {
                let schema = schema.clone();
                let cmd = driver
                    .restore_command(
                        &p,
                        &RestoreOptions {
                            input: out.clone(),
                            format,
                            // newer client tools write settings (transaction_timeout) older servers reject
                            exit_on_error: !skew,
                            single_transaction: !skew,
                            no_owner: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                async move {
                    // with version skew the tools report the ignored SET error in their exit code
                    let ok = run(&cmd);
                    assert!(ok || skew, "restore failed: {}", cmd.display());
                    let mut s = connect(&p).await;
                    let r = rows(&mut s, &format!("SELECT v FROM {schema}.bk ORDER BY id")).await;
                    assert_eq!(r, vec![vec![Value::text("one")], vec![Value::text("it's two")]]);
                    s.close().await.ok();
                }
            })
            .await;
        }
        // roles and tablespaces of the whole server (pg_dumpall)
        let globals = dir.join("globals.sql");
        let cmd = driver.backup_command(&params, &BackupOptions { output: globals.clone(), all_databases: true, globals_only: true, ..Default::default() }).unwrap();
        assert!(cmd.program.ends_with("pg_dumpall"), "{}", cmd.program);
        assert!(run(&cmd), "pg_dumpall failed: {}", cmd.display());
        let text = std::fs::read_to_string(&globals).unwrap();
        assert!(text.contains("CREATE ROLE") && !text.contains(&format!("{schema}.bk")), "{text}");
        // execute a script with the local client
        let script = dir.join("script.sql");
        std::fs::write(&script, format!("INSERT INTO {schema}.bk VALUES (3, 'three');\n")).unwrap();
        let cmd = driver.client_command(&params, &script).unwrap();
        assert!(run(&cmd), "psql failed");
        let mut s = connect(&params).await;
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM {schema}.bk")).await, Value::Int(3));
        s.close().await.ok();
        std::fs::remove_dir_all(&dir).ok();
    })
    .await;
}

fn run(cmd: &dbiji_core::admin::CommandSpec) -> bool {
    let mut c = std::process::Command::new(&cmd.program);
    c.args(&cmd.args);
    for (k, v) in &cmd.env {
        c.env(k, v.expose());
    }
    match c.output() {
        Ok(o) => {
            if !o.status.success() {
                eprintln!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            }
            o.status.success()
        }
        Err(e) => {
            eprintln!("cannot run {}: {e}", cmd.program);
            false
        }
    }
}

// ------------------------------------- extensions in a throwaway database

/// Foreign tables (file_fdw) and hstore values, in a temporary database so
/// the extensions never touch the shared one.
#[tokio::test]
async fn foreign_tables_and_hstore_in_temp_database() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_database(&params, |p, _db| async move {
        let mut s = connect(&p).await;
        let available = scalar(&mut s, "SELECT count(*) FROM pg_available_extensions WHERE name IN ('file_fdw', 'hstore')").await;
        if available != Value::Int(2) {
            eprintln!("file_fdw / hstore not available on this server; skipping");
            s.close().await.ok();
            return;
        }
        for sql in [
            "CREATE EXTENSION file_fdw",
            "CREATE EXTENSION hstore",
            "CREATE SERVER dbiji_files FOREIGN DATA WRAPPER file_fdw",
            "CREATE SCHEMA ft",
            "CREATE FOREIGN TABLE ft.lines (a integer NOT NULL, b text) SERVER dbiji_files \
             OPTIONS (filename '/dev/null', format 'csv')",
            "COMMENT ON FOREIGN TABLE ft.lines IS 'empty file'",
            "CREATE TABLE ft.kv (id integer PRIMARY KEY, h hstore)",
        ] {
            exec(&mut s, sql).await;
        }
        let names = |v: &[dbiji_core::meta::ObjectInfo]| v.iter().map(|o| o.obj.name.clone()).collect::<Vec<_>>();
        let sr = SchemaRef::schema("ft");
        let exts = s.list_objects(&SchemaRef::default(), ObjectKind::Extension).await.unwrap();
        assert!(names(&exts).contains(&"file_fdw".to_string()), "{exts:?}");
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::ForeignTable).await.unwrap()), vec!["lines"]);
        assert_eq!(names(&s.list_objects(&sr, ObjectKind::Table).await.unwrap()), vec!["kv"]);
        let ft = ObjectRef::new(ObjectKind::ForeignTable, Some("ft".into()), "lines");
        let cols = s.list_children(&ft, ObjectKind::Column).await.unwrap();
        assert_eq!(names(&cols), vec!["a", "b"]);
        // definition, DDL and a replay into a second schema
        let def = s.table_definition(&ft).await.unwrap();
        assert_eq!(def.table_kind, dbiji_core::ddl::TableKind::ForeignTable);
        assert_eq!(def.options.get("server").map(String::as_str), Some("dbiji_files"));
        assert_eq!(def.comment.as_deref(), Some("empty file"));
        let ddl = s.object_ddl(&ft).await.unwrap();
        assert!(ddl.contains("CREATE FOREIGN TABLE ft.lines"), "{ddl}");
        assert!(ddl.contains("SERVER dbiji_files OPTIONS (filename '/dev/null', format 'csv')"), "{ddl}");
        assert!(ddl.contains("COMMENT ON FOREIGN TABLE ft.lines IS 'empty file'"), "{ddl}");
        exec(&mut s, "CREATE SCHEMA ft2").await;
        let mut copy = def.clone();
        copy.schema = Some("ft2".into());
        for stmt in PgDdl::new().create_table(&copy, true) {
            exec(&mut s, &stmt).await;
        }
        let back = s.table_definition(&ObjectRef::new(ObjectKind::ForeignTable, Some("ft2".into()), "lines")).await.unwrap();
        assert_eq!(back.columns, def.columns);
        assert_eq!(back.options, def.options);
        assert_eq!(back.comment, def.comment);
        let props = s.object_properties(&ft).await.unwrap();
        let prop = |n: &str| props.iter().find(|p| p.name == n).map(|p| p.value.clone());
        assert_eq!(prop("Type").as_deref(), Some("Foreign table"));
        assert_eq!(prop("Server").as_deref(), Some("dbiji_files"));
        assert_eq!(prop("Foreign data wrapper").as_deref(), Some("file_fdw"));
        s.object_statistics(&ft).await.unwrap();
        s.object_dependencies(&ft).await.unwrap();
        let (_, data) = query_all(&mut *s, "SELECT * FROM ft.lines", &[]).await.unwrap();
        assert!(data.is_empty());
        // hstore: text protocol, binary protocol (with parameters) and as a parameter
        exec(&mut s, r#"INSERT INTO ft.kv VALUES (1, 'a=>1, "b c"=>NULL, q=>"x\"y"')"#).await;
        let expected = Value::text(r#""a"=>"1", "q"=>"x\"y", "b c"=>NULL"#);
        assert_eq!(scalar(&mut s, "SELECT h FROM ft.kv").await, expected);
        assert_eq!(query_scalar(&mut *s, "SELECT h FROM ft.kv WHERE id = $1", &[Value::Int(1)]).await.unwrap(), expected);
        execute_update(&mut *s, "INSERT INTO ft.kv VALUES ($1, $2)", &[Value::Int(2), Value::text("k=>v")]).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT h -> 'k' FROM ft.kv WHERE id = 2").await, Value::text("v"));
        s.close().await.ok();
    })
    .await;
}

// ------------------------------------------------- debugger, PostGIS and TLS

#[tokio::test]
async fn debugger_unsupported_without_extension() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let has_ext = scalar(&mut s, "SELECT count(*) FROM pg_extension WHERE extname = 'pldbgapi'").await;
    if has_ext == Value::Int(0) {
        let f = ObjectRef::new(ObjectKind::Function, Some("pg_catalog".into()), "now");
        let e = s.debug_routine(&f, &[]).await.err().expect("unsupported");
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert!(e.message.contains("pldbgapi") || e.message.contains("pldebugger"));
    }
    s.close().await.ok();
}

#[tokio::test]
async fn postgis_geometry_values() {
    let Some(url) = dbg_url() else { return };
    let params = params_from_url(&url);
    let mut s = connect(&params).await;
    let has_postgis = scalar(&mut s, "SELECT count(*) FROM pg_extension WHERE extname = 'postgis'").await;
    if has_postgis == Value::Int(0) {
        eprintln!("PostGIS not installed; skipping");
        return;
    }
    let cases = [
        ("'SRID=4326;POINT(1 2)'::geometry", "SRID=4326;POINT(1 2)"),
        ("ST_GeomFromText('LINESTRING(0 0, 1 1.5)')", "LINESTRING(0 0,1 1.5)"),
        ("'POLYGON((0 0, 1 0, 1 1, 0 0), (0.2 0.2, 0.5 0.2, 0.5 0.5, 0.2 0.2))'::geometry", "POLYGON((0 0,1 0,1 1,0 0),(0.2 0.2,0.5 0.2,0.5 0.5,0.2 0.2))"),
        ("'MULTIPOINT((1 2), (3 4))'::geometry", "MULTIPOINT(1 2,3 4)"),
        ("'MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)))'::geometry", "MULTIPOLYGON(((0 0,1 0,1 1,0 0)))"),
        ("'POINT Z (1 2 3)'::geometry", "POINT(1 2 3)"),
        ("'POINT M (1 2 3)'::geometry", "POINTM(1 2 3)"),
        ("'SRID=3857;POINT ZM (1 2 3 4)'::geometry", "SRID=3857;POINT(1 2 3 4)"),
        ("'GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))'::geometry", "GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))"),
        ("'SRID=4326;POINT(10 20)'::geography", "SRID=4326;POINT(10 20)"),
        ("'POINT EMPTY'::geometry", "POINT EMPTY"),
        ("ST_SetSRID(ST_MakeLine(ST_MakePoint(0.5, -1.25), ST_MakePoint(1e-7, 123456.789)), 4326)", "SRID=4326;LINESTRING(0.5 -1.25,0.0000001 123456.789)"),
    ];
    for (literal, expected) in cases {
        // text protocol (hex EWKB from the server)
        let (cols, data) = query_all(&mut *s, &format!("SELECT {literal} AS g"), &[]).await.unwrap();
        assert_eq!(data[0][0], Value::text(expected), "{literal}");
        assert_eq!(cols[0].kind, DataKind::Geometry);
        // our EWKT is accepted by PostGIS and denotes the same geometry
        let same = query_scalar(&mut *s, &format!("SELECT ST_AsEWKT($1::geometry) = ST_AsEWKT(({literal})::geometry)"), &[Value::text(expected)])
            .await
            .unwrap_or_else(|e| panic!("{literal}: {}", e.full_text()));
        assert_eq!(same, Value::Bool(true), "{literal}");
        // binary protocol (EWKB) gives the same text
        let bin = query_scalar(&mut *s, &format!("SELECT {literal} WHERE $1::int = 1"), &[Value::Int(1)]).await.unwrap();
        assert_eq!(bin, Value::text(expected), "binary {literal}");
    }
    // typmod rendering
    exec(&mut s, "CREATE TEMP TABLE geo (g geometry(Point, 4326))").await;
    let (cols, _) = query_all(&mut *s, "SELECT g FROM geo", &[]).await.unwrap();
    assert_eq!(cols[0].type_name, "geometry(Point,4326)");
    s.close().await.ok();
}

#[tokio::test]
async fn tls_modes() {
    let (Some(url), Some(dir)) = (dbg_url(), tls_dir()) else { return };
    let base = params_from_url(&url);
    let ca = dir.join("ca.crt").to_string_lossy().into_owned();
    async fn uses_ssl(p: &ConnectParams) -> dbiji_core::DbResult<bool> {
        let mut s = PostgresDriver::new().connect(p).await?;
        let v = query_scalar(&mut *s, "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", &[]).await?;
        s.close().await.ok();
        Ok(v == Value::Bool(true))
    }
    // disable / prefer / require
    let mut p = base.clone();
    p.ssl.mode = SslMode::Disable;
    assert!(!uses_ssl(&p).await.unwrap());
    p.ssl.mode = SslMode::Prefer;
    assert!(uses_ssl(&p).await.unwrap());
    p.ssl.mode = SslMode::Require;
    assert!(uses_ssl(&p).await.unwrap());
    // verify-ca with the test CA, no host name check
    p.ssl = SslConfig { mode: SslMode::VerifyCa, ca_cert: Some(ca.clone()), only_custom_ca: true, ..Default::default() };
    p.tls_server_name = Some("some-other-name".into());
    assert!(uses_ssl(&p).await.unwrap());
    // verify-ca without the CA fails
    p.ssl = SslConfig { mode: SslMode::VerifyCa, ..Default::default() };
    assert_eq!(uses_ssl(&p).await.unwrap_err().kind, ErrorKind::Tls);
    // verify-full checks the tls_host (tunnel case: connect to 127.0.0.1, verify "localhost")
    p.ssl = SslConfig { mode: SslMode::VerifyFull, ca_cert: Some(ca.clone()), ..Default::default() };
    p.tls_server_name = Some("localhost".into());
    assert!(uses_ssl(&p).await.unwrap());
    p.tls_server_name = None; // 127.0.0.1 is in the certificate's SAN
    assert!(uses_ssl(&p).await.unwrap());
    p.tls_server_name = Some("wrong.example.com".into());
    let e = uses_ssl(&p).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Tls, "{}", e.full_text());
    // client certificate authentication
    let mut c = base.clone();
    c.user = Some("certuser".into());
    c.password = None;
    c.ssl = SslConfig {
        mode: SslMode::VerifyFull,
        ca_cert: Some(ca.clone()),
        client_cert: Some(dir.join("client.crt").to_string_lossy().into_owned()),
        client_key: Some(dir.join("client.key").to_string_lossy().into_owned()),
        only_custom_ca: true,
    };
    c.tls_server_name = Some("localhost".into());
    let mut s = PostgresDriver::new().connect(&c).await.unwrap_or_else(|e| panic!("{}", e.full_text()));
    assert_eq!(scalar(&mut s, "SELECT current_user").await, Value::text("certuser"));
    // cancel requests use TLS too
    let handle = s.cancel_handle().unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel().await
    });
    let e = execute_update(&mut *s, "SELECT pg_sleep(10)", &[]).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    canceller.await.unwrap().unwrap();
    s.close().await.ok();
    // inline PEM works as well
    let pem = std::fs::read_to_string(dir.join("ca.crt")).unwrap();
    p.ssl = SslConfig { mode: SslMode::VerifyFull, ca_cert: Some(pem), only_custom_ca: true, ..Default::default() };
    p.tls_server_name = Some("localhost".into());
    assert!(uses_ssl(&p).await.unwrap());
}

#[tokio::test]
async fn large_schema_metadata_is_fast() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        // 1000 tables (no TOAST tables, so few locks), every 10th with a foreign key;
        // committed in batches to keep the lock table small on a shared server
        exec(
            &mut s,
            &format!(
                "DO $$ BEGIN \
                   FOR i IN 1..1000 LOOP \
                     EXECUTE format('CREATE TABLE %I.t%s (id int PRIMARY KEY, code int NOT NULL, created timestamptz DEFAULT now(), parent int %s)', \
                                    '{schema}', i, CASE WHEN i % 10 = 0 THEN format('REFERENCES %I.t%s (id)', '{schema}', i - 1) ELSE '' END); \
                     EXECUTE format('COMMENT ON TABLE %I.t%s IS %L', '{schema}', i, 'table ' || i); \
                     IF i % 100 = 0 THEN COMMIT; END IF; \
                   END LOOP; \
                 END $$"
            ),
        )
        .await;
        let sr = SchemaRef::schema(schema.clone());
        let started = Instant::now();
        let tables = s.list_objects(&sr, ObjectKind::Table).await.unwrap();
        let list_time = started.elapsed();
        assert_eq!(tables.len(), 1000);
        let started = Instant::now();
        let snap = s.schema_snapshot(&sr, &SnapshotOptions::full()).await.unwrap();
        let snap_time = started.elapsed();
        assert_eq!(snap.tables.len(), 1000);
        assert!(snap.tables.iter().all(|t| t.columns.len() == 4 && t.primary_key.is_some()));
        assert_eq!(snap.tables.iter().filter(|t| !t.foreign_keys.is_empty()).count(), 100);
        assert_eq!(snap.tables.iter().filter(|t| !t.referenced_by.is_empty()).count(), 100);
        let started = Instant::now();
        let light = s.schema_snapshot(&sr, &SnapshotOptions::light()).await.unwrap();
        let light_time = started.elapsed();
        assert_eq!(light.tables.len(), 1000);
        let started = Instant::now();
        let found = s.search_metadata(&MetadataSearch { pattern: "t99*".into(), schemas: vec![schema.clone()], ..Default::default() }).await.unwrap();
        let search_time = started.elapsed();
        assert!(found.iter().any(|o| o.obj.name == "t999"));
        eprintln!("1000 tables: list {list_time:?}, full snapshot {snap_time:?}, light snapshot {light_time:?}, search {search_time:?}");
        assert!(list_time < Duration::from_secs(2), "list took {list_time:?}");
        assert!(snap_time < Duration::from_secs(10), "snapshot took {snap_time:?}");
        // drop in batches as well
        exec(
            &mut s,
            &format!(
                "DO $$ BEGIN FOR i IN REVERSE 1000..1 LOOP EXECUTE format('DROP TABLE %I.t%s', '{schema}', i); \
                 IF i % 100 = 0 THEN COMMIT; END IF; END LOOP; END $$"
            ),
        )
        .await;
        s.close().await.ok();
    })
    .await;
}

/// Manual commit: re-querying while a big result is still streaming (the
/// data editor changing its ORDER BY) must not leave the transaction
/// aborted (25P02), and work done earlier in the transaction is kept.
#[tokio::test]
async fn manual_mode_survives_an_abandoned_cursor() {
    let Some(url) = common::url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        exec(&mut s, &format!("CREATE TABLE {schema}.big AS SELECT g AS id, repeat('x', 100) AS pad FROM generate_series(1, 300000) g")).await;
        exec(&mut s, &format!("CREATE TABLE {schema}.kept (id int)")).await;
        s.set_transaction_mode(TransactionMode::Manual).await.unwrap();
        exec(&mut s, &format!("INSERT INTO {schema}.kept VALUES (1)")).await;
        for _ in 0..2 {
            let mut c = s.execute(&format!("SELECT * FROM {schema}.big"), &[], &ExecOptions::default()).await.unwrap();
            assert!(matches!(c.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
            assert_eq!(c.fetch(200).await.unwrap().len(), 200);
            drop(c);
            let r = rows(&mut s, &format!("SELECT id FROM {schema}.big ORDER BY id DESC LIMIT 1")).await;
            assert_eq!(r[0][0], Value::Int(300000));
        }
        // close() instead of drop
        let mut c = s.execute(&format!("SELECT * FROM {schema}.big"), &[], &ExecOptions::default()).await.unwrap();
        c.next_result().await.unwrap();
        c.fetch(10).await.unwrap();
        c.close().await.unwrap();
        drop(c);
        assert!(s.in_transaction());
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM {schema}.kept")).await, Value::Int(1), "earlier work kept");
        s.commit().await.unwrap();
    })
    .await;
}

/// Every object of the schema answers properties, statistics, dependencies and
/// DDL without a server error (a catalog column of a newer version, say).
#[tokio::test]
async fn every_object_answers_its_tabs() {
    let Some(url) = url() else { return };
    let params = params_from_url(&url);
    with_schema(&params, |params, schema| async move {
        let mut s = connect(&params).await;
        create_rich_schema(&mut s, &schema).await;
        exec(&mut s, &format!("CREATE POLICY p1 ON {schema}.customers USING (true)")).await;
        exec(&mut s, &format!("CREATE STATISTICS {schema}.st1 (dependencies) ON qty, price FROM {schema}.orders")).await;
        let sr = SchemaRef::schema(schema.clone());
        let mut objs = vec![ObjectRef::new(ObjectKind::Schema, None, schema.clone()), ObjectRef::new(ObjectKind::Database, None, params.database.clone().unwrap())];
        let kinds = [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Sequence,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Aggregate,
            ObjectKind::Type,
            ObjectKind::Domain,
        ];
        for k in kinds {
            objs.extend(s.list_objects(&sr, k).await.unwrap_or_else(|e| panic!("{k:?}: {}", e.full_text())).into_iter().map(|o| o.obj));
        }
        objs.extend(s.list_objects(&SchemaRef::default(), ObjectKind::Extension).await.unwrap().into_iter().map(|o| o.obj));
        let tables: Vec<ObjectRef> = objs.iter().filter(|o| o.kind == ObjectKind::Table).cloned().collect();
        let children = [
            ObjectKind::Column,
            ObjectKind::Index,
            ObjectKind::PrimaryKey,
            ObjectKind::UniqueKey,
            ObjectKind::CheckConstraint,
            ObjectKind::ForeignKey,
            ObjectKind::Trigger,
            ObjectKind::Partition,
        ];
        for t in &tables {
            for k in children {
                objs.extend(s.list_children(t, k).await.unwrap_or_else(|e| panic!("{t} {k:?}: {}", e.full_text())).into_iter().map(|o| o.obj));
            }
        }
        assert!(objs.len() > 40, "{}", objs.len());
        let mut failed = Vec::new();
        for o in &objs {
            let results =
                [s.object_properties(o).await.map(|_| ()), s.object_statistics(o).await.map(|_| ()), s.object_dependencies(o).await.map(|_| ()), s.object_ddl(o).await.map(|_| ())];
            failed.extend(results.into_iter().filter_map(|r| r.err()).filter(|e| e.code.is_some()).map(|e| format!("{:?} {o}: {}", o.kind, e.full_text())));
        }
        assert!(failed.is_empty(), "{failed:#?}");
    })
    .await;
}
