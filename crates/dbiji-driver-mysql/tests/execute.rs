//! Statement execution against real servers: value conversion (text and
//! binary protocol), parameters, streaming, cancellation, dropped cursors,
//! multiple results, transactions, warnings and error mapping.

mod common;

use chrono::{NaiveDate, NaiveTime};
use common::*;
use dbiji_core::{DataKind, ErrorKind, ExecOptions, ResultHeader, Session, TransactionMode, Value};
use std::time::{Duration, Instant};

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

const TYPES_COLUMNS: &str = "id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    ti TINYINT, tiu TINYINT UNSIGNED, t1 TINYINT(1), si SMALLINT, mi MEDIUMINT, i INT, bi BIGINT, biu BIGINT UNSIGNED,
    dec1 DECIMAL(10,2), f FLOAT, d DOUBLE, bit1 BIT(1), bit10 BIT(10),
    dt DATE, tm TIME, tm3 TIME(3), dtm DATETIME, dtm6 DATETIME(6), ts TIMESTAMP NULL, yr YEAR,
    ch CHAR(5), vc VARCHAR(100), tx TEXT, bin BINARY(4), vb VARBINARY(10), bl BLOB,
    e ENUM('a','b','c'), st SET('x','y','z'), j JSON, g GEOMETRY, big_t TIME, zero_d DATETIME";

const TYPES_INSERT: &str = "INSERT INTO types (ti, tiu, t1, si, mi, i, bi, biu, dec1, f, d, bit1, bit10, dt, tm, tm3, dtm, dtm6, ts, yr,
    ch, vc, tx, bin, vb, bl, e, st, j, g, big_t, zero_d) VALUES
    (-5, 250, 1, -300, 8000000, -2000000000, -9000000000000000000, 18446744073709551615, 12345678.90, 1.5, 2.25, b'1', b'1000000101',
     '2024-02-29', '-01:30:00', '12:34:56.789', '2024-01-02 03:04:05', '2024-01-02 03:04:05.123456', '2024-01-02 03:04:05', 2024,
     'ab', 'héllo wörld ✓', 'long text', 0x01020304, 0xDEADBEEF, 0x00FF, 'b', 'x,z', '{\"a\": [1, 2]}',
     ST_GeomFromText('POINT(1 2)'), '838:59:59', '0000-00-00 00:00:00')";

fn expected_row(mariadb: bool) -> Vec<Value> {
    vec![
        Value::Int(1),
        Value::Int(-5),
        Value::Int(250),
        Value::Int(1),
        Value::Int(-300),
        Value::Int(8_000_000),
        Value::Int(-2_000_000_000),
        Value::Int(-9_000_000_000_000_000_000),
        Value::UInt(u64::MAX),
        Value::decimal("12345678.90"),
        Value::Float(1.5),
        Value::Float(2.25),
        Value::Bool(true),
        Value::text("1000000101"),
        Value::Date(date(2024, 2, 29)),
        Value::text("-01:30:00"),
        Value::Time(NaiveTime::from_hms_milli_opt(12, 34, 56, 789).unwrap()),
        Value::DateTime(date(2024, 1, 2).and_hms_opt(3, 4, 5).unwrap()),
        Value::DateTime(
            date(2024, 1, 2)
                .and_hms_micro_opt(3, 4, 5, 123_456)
                .unwrap(),
        ),
        Value::DateTime(date(2024, 1, 2).and_hms_opt(3, 4, 5).unwrap()),
        Value::Int(2024),
        Value::text("ab"),
        Value::text("héllo wörld ✓"),
        Value::text("long text"),
        Value::bytes(vec![1u8, 2, 3, 4]),
        Value::bytes(vec![0xDEu8, 0xAD, 0xBE, 0xEF]),
        Value::bytes(vec![0u8, 0xFF]),
        Value::text("b"),
        Value::text("x,z"),
        if mariadb {
            Value::text("{\"a\": [1, 2]}")
        } else {
            Value::json("{\"a\": [1, 2]}")
        },
        Value::text("POINT(1 2)"),
        Value::text("838:59:59"),
        Value::text("0000-00-00 00:00:00"),
    ]
}

#[tokio::test]
async fn type_conversions_text_and_binary() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        exec(&mut s, "SET SESSION sql_mode = ''").await;
        exec(&mut s, &format!("CREATE TABLE types ({TYPES_COLUMNS})")).await;
        exec(&mut s, TYPES_INSERT).await;
        let expected = expected_row(srv.is_mariadb());

        // text protocol
        let mut c = s
            .execute("SELECT * FROM types", &[], &ExecOptions::default())
            .await
            .unwrap();
        let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
            panic!("rows expected")
        };
        let rows = c.fetch(10).await.unwrap();
        assert!(c.fetch(10).await.unwrap().is_empty());
        assert!(c.next_result().await.unwrap().is_none());
        c.close().await.unwrap();
        drop(c);
        assert_eq!(rows.len(), 1);
        for (i, (got, want)) in rows[0].iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                got, want,
                "[{}] text protocol column {} ({})",
                srv.name, cols[i].name, cols[i].type_name
            );
        }

        // column metadata
        let by = |n: &str| {
            cols.iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("column {n}"))
        };
        let id = by("id");
        assert_eq!((id.nullable, id.auto_increment), (Some(false), Some(true)));
        let src = id.source.clone().expect("source");
        assert_eq!(
            (
                src.schema.as_deref(),
                src.table.as_str(),
                src.column.as_str()
            ),
            (Some(db.name.as_str()), "types", "id")
        );
        let names: Vec<(&str, &str, DataKind)> = cols
            .iter()
            .map(|c| (c.name.as_str(), c.type_name.as_str(), c.kind))
            .collect();
        let expect_types = [
            ("ti", "tinyint", DataKind::Int),
            ("tiu", "tinyint unsigned", DataKind::Int),
            ("t1", "tinyint(1)", DataKind::Int),
            ("si", "smallint", DataKind::Int),
            ("mi", "mediumint", DataKind::Int),
            ("i", "int", DataKind::Int),
            ("bi", "bigint", DataKind::Int),
            ("biu", "bigint unsigned", DataKind::Int),
            ("dec1", "decimal(10,2)", DataKind::Decimal),
            ("f", "float", DataKind::Float),
            ("d", "double", DataKind::Float),
            ("bit1", "bit(1)", DataKind::Bool),
            ("bit10", "bit(10)", DataKind::Bit),
            ("dt", "date", DataKind::Date),
            ("tm", "time", DataKind::Time),
            ("tm3", "time(3)", DataKind::Time),
            ("dtm", "datetime", DataKind::DateTime),
            ("dtm6", "datetime(6)", DataKind::DateTime),
            ("ts", "timestamp", DataKind::DateTime),
            ("yr", "year", DataKind::Int),
            ("ch", "char(5)", DataKind::Text),
            ("vc", "varchar(100)", DataKind::Text),
            ("tx", "text", DataKind::Text),
            ("bin", "binary(4)", DataKind::Binary),
            ("vb", "varbinary(10)", DataKind::Binary),
            ("bl", "blob", DataKind::Binary),
            ("e", "enum", DataKind::Enum),
            ("st", "set", DataKind::Enum),
            ("g", "geometry", DataKind::Geometry),
        ];
        for (n, t, k) in expect_types {
            let got = names.iter().find(|x| x.0 == n).unwrap();
            assert_eq!((got.1, got.2), (t, k), "[{}] column {n}", srv.name);
        }
        let j = by("j");
        if srv.is_mariadb() {
            assert_eq!(j.kind, DataKind::Text);
        } else {
            assert_eq!((j.type_name.as_str(), j.kind), ("json", DataKind::Json));
        }
        let d = by("dec1");
        assert_eq!((d.precision, d.scale), (Some(10), Some(2)));
        assert_eq!(by("vc").max_length, Some(100));

        // binary protocol (parameter bound)
        let r = run(&mut s, "SELECT * FROM types WHERE id = ?", &[Value::Int(1)])
            .await
            .unwrap();
        let ResultHeader::Rows(bcols) = &r[0].0 else {
            panic!("rows")
        };
        assert_eq!(
            bcols.iter().map(|c| &c.type_name).collect::<Vec<_>>(),
            cols.iter().map(|c| &c.type_name).collect::<Vec<_>>()
        );
        for (i, (got, want)) in r[0].1[0].iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                got, want,
                "[{}] binary protocol column {}",
                srv.name, cols[i].name
            );
        }
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn geometry_types() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let srid = if srv.is_mariadb() {
            "ST_GeomFromText('POINT(1 2)', 4326)"
        } else {
            "ST_GeomFromText('POINT(1 2)', 4326, 'axis-order=long-lat')"
        };
        let sql = format!(
            "SELECT {srid}, ST_GeomFromText('LINESTRING(0 0,1 1,2 0.5)'), ST_GeomFromText('POLYGON((0 0,4 0,4 4,0 0),(1 1,2 1,2 2,1 1))'), \
             ST_GeomFromText('MULTIPOINT(1 1,2 2)'), ST_GeomFromText('GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1 1))'), \
             ST_GeomFromText('MULTIPOLYGON(((0 0,1 0,1 1,0 0)))'), ST_GeomFromText('MULTILINESTRING((0 0,1 1),(2 2,3 3))')"
        );
        let rows = query(&mut s, &sql).await;
        let got: Vec<String> = rows[0].iter().map(|v| v.to_text().into_owned()).collect();
        assert_eq!(
            got,
            vec![
                "SRID=4326;POINT(1 2)",
                "LINESTRING(0 0,1 1,2 0.5)",
                "POLYGON((0 0,4 0,4 4,0 0),(1 1,2 1,2 2,1 1))",
                "MULTIPOINT((1 1),(2 2))",
                "GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1 1))",
                "MULTIPOLYGON(((0 0,1 0,1 1,0 0)))",
                "MULTILINESTRING((0 0,1 1),(2 2,3 3))",
            ],
            "[{}]",
            srv.name
        );
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn parameter_round_trip() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        exec(
            &mut s,
            "CREATE TABLE p (id INT PRIMARY KEY, b TINYINT(1), i BIGINT, u BIGINT UNSIGNED, f DOUBLE, d DECIMAL(20,6), t VARCHAR(200),
             bin VARBINARY(20), dt DATE, tm TIME(6), dtm DATETIME(6), u2 CHAR(36), j TEXT, n INT)",
        )
        .await;
        let uuid = uuid::Uuid::parse_str("6f1c2b4e-4b7d-4a1f-9c3e-2d5f6a7b8c9d").unwrap();
        let dtm = date(2023, 12, 31)
            .and_hms_micro_opt(23, 59, 58, 999_999)
            .unwrap();
        let params = vec![
            Value::Int(1),
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::UInt(u64::MAX),
            Value::Float(-0.125),
            Value::decimal("-12345678901234.123456"),
            Value::text("it's a \\ \"test\" ✓ 日本"),
            Value::bytes(vec![0u8, 1, 2, 255, b'\'', b'\\']),
            Value::Date(date(2000, 1, 1)),
            Value::Time(NaiveTime::from_hms_micro_opt(13, 14, 15, 16).unwrap()),
            Value::DateTime(dtm),
            Value::Uuid(uuid),
            Value::Array(vec![Value::Int(1), Value::text("a")].into_boxed_slice()),
            Value::Null,
        ];
        let r = run(
            &mut s,
            "INSERT INTO p VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &params,
        )
        .await
        .unwrap();
        match &r[0].0 {
            ResultHeader::Update { rows_affected, .. } => assert_eq!(*rows_affected, Some(1)),
            other => panic!("unexpected {other:?}"),
        }
        let expected = vec![
            Value::Int(1),
            Value::Int(1),
            Value::Int(i64::MIN),
            Value::UInt(u64::MAX),
            Value::Float(-0.125),
            Value::decimal("-12345678901234.123456"),
            Value::text("it's a \\ \"test\" ✓ 日本"),
            Value::bytes(vec![0u8, 1, 2, 255, b'\'', b'\\']),
            Value::Date(date(2000, 1, 1)),
            Value::Time(NaiveTime::from_hms_micro_opt(13, 14, 15, 16).unwrap()),
            Value::DateTime(dtm),
            Value::text("6f1c2b4e-4b7d-4a1f-9c3e-2d5f6a7b8c9d"),
            Value::text("[1,\"a\"]"),
            Value::Null,
        ];
        // text protocol read back
        let rows = query(&mut s, "SELECT * FROM p").await;
        assert_eq!(rows[0], expected, "[{}] text", srv.name);
        // binary protocol read back with a parameter in WHERE
        let r = run(
            &mut s,
            "SELECT * FROM p WHERE t = ? AND bin = ?",
            &[params[6].clone(), params[7].clone()],
        )
        .await
        .unwrap();
        assert_eq!(r[0].1.len(), 1, "[{}] binary lookup", srv.name);
        assert_eq!(r[0].1[0], expected, "[{}] binary", srv.name);
        // parameter count mismatch: client side error, connection stays usable
        let e = run(&mut s, "SELECT ? + ?", &[Value::Int(1)])
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Query, "{}", e.full_text());
        // named-parameter-looking text is rejected before execution
        let e = s
            .execute(
                "SELECT ':abc' AS x, ? FROM dual WHERE 1 = :x",
                &[Value::Int(1)],
                &ExecOptions::default(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Query);
        s.ping().await.unwrap();
        assert_eq!(
            query(&mut s, "SELECT COUNT(*) FROM p").await[0][0],
            Value::Int(1)
        );
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn streaming_fetch_sizes() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let sql = numbers_sql(4, 20); // 10 000 rows
        for fetch in [1usize, 7, 1000, 100_000] {
            let mut c = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
            let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
                panic!()
            };
            assert_eq!(cols.len(), 2);
            let mut seen = vec![false; 10_000];
            let mut total = 0usize;
            loop {
                let batch = c.fetch(fetch).await.unwrap();
                if batch.is_empty() {
                    break;
                }
                assert!(batch.len() <= fetch);
                for r in &batch {
                    let n = r[0].as_i64().unwrap() as usize;
                    assert!(!seen[n]);
                    seen[n] = true;
                    assert_eq!(r[1].as_str().map(str::len), Some(20));
                }
                total += batch.len();
            }
            assert_eq!(total, 10_000, "[{}] fetch size {fetch}", srv.name);
            assert!(c.next_result().await.unwrap().is_none());
            c.close().await.unwrap();
        }
        s.ping().await.unwrap();
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_running_statement() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let cancel = s.cancel_handle().unwrap();
        let started = Instant::now();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel().await
        });
        // SLEEP alone returns 1 when interrupted; as part of a larger
        // expression the statement fails with ER_QUERY_INTERRUPTED.
        let r = run(&mut s, "SELECT SLEEP(10)", &[]).await;
        let elapsed = started.elapsed();
        canceller.await.unwrap().unwrap();
        println!(
            "[{}] SLEEP(10) cancelled after {elapsed:?} (cancel sent at 300 ms)",
            srv.name
        );
        // cancel request at 300 ms + side connection and KILL QUERY
        assert!(
            elapsed < Duration::from_millis(1300),
            "[{}] cancel took {elapsed:?}",
            srv.name
        );
        match r {
            Ok(res) => assert_eq!(
                res[0].1[0][0],
                Value::Int(1),
                "[{}] interrupted SLEEP returns 1",
                srv.name
            ),
            Err(e) => assert_eq!(e.kind, ErrorKind::Cancelled, "{}", e.full_text()),
        }
        let cancel = s.cancel_handle().unwrap();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel().await
        });
        let started = Instant::now();
        let e = run(
            &mut s,
            "SELECT SLEEP(10) + 1 FROM (SELECT 1 UNION ALL SELECT 2) x",
            &[],
        )
        .await;
        canceller.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        if let Err(e) = e {
            assert_eq!(
                e.kind,
                ErrorKind::Cancelled,
                "[{}] {}",
                srv.name,
                e.full_text()
            );
            assert_eq!(e.code.as_deref(), Some("1317"));
        }
        // the session is still usable
        assert_eq!(query(&mut s, "SELECT 7").await[0][0], Value::Int(7));
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statement_timeout() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        };
        let started = Instant::now();
        let mut c = s
            .execute(
                "SELECT SLEEP(5) + 1 FROM (SELECT 1 UNION ALL SELECT 2) x",
                &[],
                &opts,
            )
            .await
            .unwrap();
        let r = collect_all(&mut c, 100).await;
        drop(c);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "[{}] timeout took {:?}",
            srv.name,
            started.elapsed()
        );
        match r {
            Err(e) => assert_eq!(
                e.kind,
                ErrorKind::Timeout,
                "[{}] {}",
                srv.name,
                e.full_text()
            ),
            Ok(res) => panic!("[{}] expected a timeout, got {res:?}", srv.name),
        }
        // statements faster than the timeout are not affected
        let mut c = s.execute("SELECT 1", &[], &opts).await.unwrap();
        assert_eq!(
            collect_all(&mut c, 10).await.unwrap()[0].1[0][0],
            Value::Int(1)
        );
        drop(c);
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_cursor_does_not_hang() {
    for srv in servers() {
        let mut s = srv.connect().await;
        // 1. huge result, a few rows read, cursor dropped
        let sql = numbers_sql(7, 100); // 10 million rows
        let mut c = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        assert!(matches!(
            c.next_result().await.unwrap(),
            Some(ResultHeader::Rows(_))
        ));
        assert_eq!(c.fetch(10).await.unwrap().len(), 10);
        let started = Instant::now();
        drop(c);
        assert_eq!(query(&mut s, "SELECT 5").await[0][0], Value::Int(5));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "[{}] took {:?}: {}",
            srv.name,
            started.elapsed(),
            dbiji_driver_mysql::cursor::last_abandon_report()
        );

        // 2. cursor dropped while the statement still executes
        let c = s
            .execute("SELECT SLEEP(20)", &[], &ExecOptions::default())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        drop(c);
        assert_eq!(query(&mut s, "SELECT 6").await[0][0], Value::Int(6));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "[{}] took {:?}",
            srv.name,
            started.elapsed()
        );

        // 3. small remainder: drained without KILL
        let mut c = s
            .execute(&numbers_sql(2, 1), &[], &ExecOptions::default())
            .await
            .unwrap();
        c.next_result().await.unwrap();
        c.fetch(3).await.unwrap();
        c.close().await.unwrap();
        drop(c);
        assert_eq!(query(&mut s, "SELECT 8").await[0][0], Value::Int(8));

        // 4. close() with unread rows of a huge result
        let mut c = s.execute(&sql, &[], &ExecOptions::default()).await.unwrap();
        c.next_result().await.unwrap();
        c.fetch(1).await.unwrap();
        let started = Instant::now();
        c.close().await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(c);
        // 5. session.close() while a cursor is still alive
        let c = s
            .execute("SELECT SLEEP(20)", &[], &ExecOptions::default())
            .await
            .unwrap();
        let started = Instant::now();
        s.close().await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "[{}] close took {:?}",
            srv.name,
            started.elapsed()
        );
        drop(c);
    }
}

#[tokio::test]
async fn multiple_results() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        let r = run(
            &mut s,
            "CREATE TABLE m (id INT PRIMARY KEY AUTO_INCREMENT, v INT)",
            &[],
        )
        .await
        .unwrap();
        assert!(
            matches!(&r[0].0, ResultHeader::Update { tag: Some(t), .. } if t == "CREATE TABLE"),
            "[{}] {:?}",
            srv.name,
            r[0].0
        );
        exec(
            &mut s,
            "CREATE PROCEDURE two_sets(IN n INT)
             BEGIN
               SELECT n AS first_v;
               INSERT INTO m (v) VALUES (n);
               SELECT v, v * 2 AS twice FROM m ORDER BY id;
             END",
        )
        .await;
        let r = run(&mut s, "CALL two_sets(21)", &[]).await.unwrap();
        assert!(r.len() >= 3, "[{}] {r:?}", srv.name);
        assert!(matches!(&r[0].0, ResultHeader::Rows(c) if c[0].name == "first_v"));
        assert_eq!(r[0].1, vec![vec![Value::Int(21)]]);
        assert!(matches!(&r[1].0, ResultHeader::Rows(c) if c.len() == 2 && c[1].name == "twice"));
        assert_eq!(r[1].1, vec![vec![Value::Int(21), Value::Int(42)]]);
        assert!(matches!(r.last().unwrap().0, ResultHeader::Update { .. }));
        // CALL with a bound parameter (binary protocol)
        let r = run(&mut s, "CALL two_sets(?)", &[Value::Int(5)])
            .await
            .unwrap();
        assert_eq!(r[0].1, vec![vec![Value::Int(5)]]);
        assert_eq!(r[1].1.len(), 2);

        // multi statement text
        let r = run(&mut s, "SELECT 1 AS a; UPDATE m SET v = v + 1; SELECT 'x' AS b, 'y' AS c; INSERT INTO m (v) VALUES (100)", &[])
            .await
            .unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].1, vec![vec![Value::Int(1)]]);
        assert!(
            matches!(
                r[1].0,
                ResultHeader::Update {
                    rows_affected: Some(2),
                    ..
                }
            ),
            "{:?}",
            r[1].0
        );
        if let ResultHeader::Update { tag, .. } = &r[1].0 {
            assert!(
                tag.as_deref().unwrap_or("").contains("Rows matched: 2"),
                "{tag:?}"
            );
        }
        assert_eq!(r[2].1, vec![vec![Value::text("x"), Value::text("y")]]);
        match &r[3].0 {
            ResultHeader::Update {
                rows_affected,
                last_insert_id,
                ..
            } => {
                assert_eq!(*rows_affected, Some(1));
                assert_eq!(*last_insert_id, Some(3));
            }
            other => panic!("{other:?}"),
        }
        // next_result discards unread rows of the current result
        let mut c = s
            .execute(
                "SELECT * FROM m; SELECT 'second'",
                &[],
                &ExecOptions::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            c.next_result().await.unwrap(),
            Some(ResultHeader::Rows(_))
        ));
        assert_eq!(c.fetch(1).await.unwrap().len(), 1);
        assert!(matches!(
            c.next_result().await.unwrap(),
            Some(ResultHeader::Rows(_))
        ));
        assert_eq!(
            c.fetch(10).await.unwrap(),
            vec![vec![Value::text("second")]]
        );
        assert!(c.next_result().await.unwrap().is_none());
        drop(c);
        // an error in a later statement is reported at its next_result
        let mut c = s
            .execute(
                "SELECT 1; SELECT * FROM no_such_table; SELECT 3",
                &[],
                &ExecOptions::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            c.next_result().await.unwrap(),
            Some(ResultHeader::Rows(_))
        ));
        assert_eq!(c.fetch(5).await.unwrap(), vec![vec![Value::Int(1)]]);
        let e = c.next_result().await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("1146"));
        assert!(c.next_result().await.unwrap().is_none());
        drop(c);
        s.ping().await.unwrap();
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn transactions() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        exec(&mut s, "CREATE TABLE tx (id INT PRIMARY KEY) ENGINE=InnoDB").await;
        assert_eq!(s.transaction_mode(), TransactionMode::Auto);
        s.set_transaction_mode(TransactionMode::Manual)
            .await
            .unwrap();
        assert!(!s.in_transaction());
        // browsing metadata does not look like a pending transaction
        let t = dbiji_core::meta::ObjectRef::table(Some(db.name.clone()), "tx");
        s.table_definition(&t).await.unwrap();
        s.object_properties(&t).await.unwrap();
        assert!(
            !s.in_transaction(),
            "[{}] metadata calls flipped the indicator",
            srv.name
        );
        exec(&mut s, "INSERT INTO tx VALUES (1)").await;
        assert!(
            s.in_transaction(),
            "[{}] in transaction after insert",
            srv.name
        );
        s.rollback().await.unwrap();
        assert!(!s.in_transaction());
        assert_eq!(
            query(&mut s, "SELECT COUNT(*) FROM tx").await[0][0],
            Value::Int(0)
        );
        exec(&mut s, "INSERT INTO tx VALUES (2)").await;
        exec(&mut s, "SAVEPOINT sp1").await;
        exec(&mut s, "INSERT INTO tx VALUES (3)").await;
        exec(&mut s, "ROLLBACK TO SAVEPOINT sp1").await;
        s.commit().await.unwrap();
        assert!(!s.in_transaction());
        let mut other = db.session().await;
        assert_eq!(
            query(&mut other, "SELECT GROUP_CONCAT(id) FROM tx").await[0][0],
            Value::text("2")
        );
        // switching to Auto commits an open transaction
        exec(&mut s, "INSERT INTO tx VALUES (4)").await;
        assert!(s.in_transaction());
        s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
        assert!(!s.in_transaction());
        assert_eq!(
            query(&mut other, "SELECT COUNT(*) FROM tx").await[0][0],
            Value::Int(2)
        );
        // explicit transaction in auto-commit mode
        exec(&mut s, "START TRANSACTION").await;
        assert!(s.in_transaction());
        exec(&mut s, "DELETE FROM tx").await;
        exec(&mut s, "ROLLBACK").await;
        assert!(!s.in_transaction());
        assert_eq!(
            query(&mut s, "SELECT COUNT(*) FROM tx").await[0][0],
            Value::Int(2)
        );
        // a manual-mode session from the connection settings
        let mut p = srv.params_for(&db.name);
        p.auto_commit = false;
        let mut m = srv.connect_with(&p).await;
        assert_eq!(m.transaction_mode(), TransactionMode::Manual);
        exec(&mut m, "INSERT INTO tx VALUES (9)").await;
        assert!(m.in_transaction());
        m.close().await.unwrap();
        assert_eq!(
            query(&mut other, "SELECT COUNT(*) FROM tx WHERE id = 9").await[0][0],
            Value::Int(0)
        );
        other.close().await.unwrap();
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn warnings_are_reported() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let mut c = s
            .execute(
                "SELECT CAST('12abc' AS SIGNED) AS v",
                &[],
                &ExecOptions::default(),
            )
            .await
            .unwrap();
        let res = collect_all(&mut c, 10).await.unwrap();
        assert_eq!(res[0].1[0][0], Value::Int(12));
        let msgs = c.take_messages();
        assert!(
            msgs.iter()
                .any(|m| m.code.as_deref() == Some("1292") && m.severity == "WARNING"),
            "[{}] {msgs:?}",
            srv.name
        );
        assert!(c.take_messages().is_empty());
        drop(c);
        // no warnings, no SHOW WARNINGS messages
        let mut c = s
            .execute("SELECT 1", &[], &ExecOptions::default())
            .await
            .unwrap();
        collect_all(&mut c, 10).await.unwrap();
        assert!(c.take_messages().is_empty());
        drop(c);
        // update results with warnings
        let mut c = s
            .execute(
                "SET @x = CAST('1.5x' AS DECIMAL(5,2))",
                &[],
                &ExecOptions::default(),
            )
            .await
            .unwrap();
        collect_all(&mut c, 10).await.unwrap();
        assert!(
            !c.take_messages().is_empty(),
            "[{}] warning of SET",
            srv.name
        );
        drop(c);
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn error_mapping() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let e = run(&mut s, "SELECT * FORM dual", &[]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Syntax);
        assert_eq!(e.code.as_deref(), Some("1064"));
        assert_eq!(e.detail.as_deref(), Some("SQLSTATE 42000"));
        assert_eq!(e.position, Some(10), "[{}] {}", srv.name, e.message);
        let e = run(&mut s, "SELECT * FROM mysql.no_such_table_x", &[])
            .await
            .unwrap_err();
        assert_eq!(
            (e.kind, e.code.as_deref()),
            (ErrorKind::Query, Some("1146")),
            "{}",
            e.full_text()
        );
        let e = run(&mut s, "SELECT no_such_function_x(1)", &[])
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Query);
        // errors with parameters (binary protocol prepare)
        let e = run(&mut s, "SELEC ?", &[Value::Int(1)]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Syntax);
        s.ping().await.unwrap();
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn result_column_sources() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        exec(
            &mut s,
            "CREATE TABLE a (id INT PRIMARY KEY, name VARCHAR(20))",
        )
        .await;
        exec(&mut s, "CREATE TABLE b (id INT PRIMARY KEY, a_id INT)").await;
        exec(
            &mut s,
            "INSERT INTO a VALUES (1, 'x'); INSERT INTO b VALUES (10, 1)",
        )
        .await;
        let mut c = s.execute("SELECT a.id AS aid, a.name, b.id, 1 + 1 AS expr, UPPER(a.name) AS up FROM a JOIN b ON b.a_id = a.id", &[], &ExecOptions { resolve_sources: true, ..Default::default() })
            .await
            .unwrap();
        let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
            panic!()
        };
        drop(c);
        let src = |i: usize| {
            cols[i]
                .source
                .clone()
                .map(|s| (s.schema.unwrap_or_default(), s.table, s.column))
        };
        assert_eq!(
            src(0),
            Some((db.name.clone(), "a".to_string(), "id".to_string()))
        );
        assert_eq!(cols[0].name, "aid");
        assert_eq!(
            src(1),
            Some((db.name.clone(), "a".to_string(), "name".to_string()))
        );
        assert_eq!(
            src(2),
            Some((db.name.clone(), "b".to_string(), "id".to_string()))
        );
        assert_eq!(src(3), None);
        assert_eq!(src(4), None);
        assert_eq!(cols[0].nullable, Some(false));
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn context_switching() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = srv.connect().await;
        s.set_current_schema(&db.name).await.unwrap();
        assert_eq!(
            s.current_schema().await.unwrap().as_deref(),
            Some(db.name.as_str())
        );
        assert_eq!(
            s.current_database().await.unwrap().as_deref(),
            Some(db.name.as_str())
        );
        s.set_current_database("information_schema").await.unwrap();
        assert_eq!(
            s.current_database().await.unwrap().as_deref(),
            Some("information_schema")
        );
        assert!(s.list_databases().await.unwrap().is_empty());
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_while_streaming_rows() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let mut c = s
            .execute(&numbers_sql(7, 50), &[], &ExecOptions::default())
            .await
            .unwrap();
        assert!(matches!(
            c.next_result().await.unwrap(),
            Some(ResultHeader::Rows(_))
        ));
        assert_eq!(c.fetch(100).await.unwrap().len(), 100);
        s.cancel_handle().unwrap().cancel().await.unwrap();
        let started = Instant::now();
        let mut outcome = None;
        while started.elapsed() < Duration::from_secs(10) {
            match c.fetch(10_000).await {
                Ok(rows) if rows.is_empty() => {
                    outcome = Some(Ok(()));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    outcome = Some(Err(e));
                    break;
                }
            }
        }
        match outcome {
            Some(Err(e)) => assert_eq!(
                e.kind,
                ErrorKind::Cancelled,
                "[{}] {}",
                srv.name,
                e.full_text()
            ),
            other => panic!(
                "[{}] expected the stream to be interrupted, got {other:?}",
                srv.name
            ),
        }
        drop(c);
        assert_eq!(query(&mut s, "SELECT 3").await[0][0], Value::Int(3));
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_cursor_call_is_reported() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let mut c = s
            .execute("SELECT SLEEP(1)", &[], &ExecOptions::default())
            .await
            .unwrap();
        // the caller gives up waiting for the header (the future is dropped)
        let r = tokio::time::timeout(Duration::from_millis(100), c.next_result()).await;
        assert!(r.is_err());
        let e = c.next_result().await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Internal);
        let started = Instant::now();
        c.close().await.unwrap();
        drop(c);
        assert_eq!(query(&mut s, "SELECT 4").await[0][0], Value::Int(4));
        assert!(started.elapsed() < Duration::from_secs(5));
        s.close().await.unwrap();
    }
}
