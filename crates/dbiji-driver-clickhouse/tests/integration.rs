//! Integration tests against a live ClickHouse server.
//!
//! Set `DBIJI_TEST_CLICKHOUSE=http://user:password@host:port` to run them;
//! without it every test returns immediately. Each test works in its own
//! uniquely named database which is dropped at the end.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::ddl::*;
use dbiji_core::driver::query_all;
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ErrorKind, ExecOptions, ResultHeader, Secret, Session, TransactionMode, Value,
};
use dbiji_driver_clickhouse::ClickHouseDriver;
use std::time::{Duration, Instant};

fn params() -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_CLICKHOUSE").ok()?;
    let (scheme, rest) = url.split_once("://")?;
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport.rsplit_once(':').map(|(h, p)| (h, p.parse().ok())).unwrap_or((hostport, None));
    let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "test");
    cfg.host = host.to_string();
    cfg.port = port.or(Some(8123));
    cfg.ssl.mode = if scheme == "https" { SslMode::Require } else { SslMode::Disable };
    let mut password = None;
    if let Some(a) = auth {
        let (u, p) = a.split_once(':').unwrap_or((a, ""));
        cfg.user = Some(u.to_string());
        password = Some(Secret::new(p));
    }
    cfg.database = Some("default".into());
    Some(ConnectParams::from_config(&cfg, password))
}

async fn connect() -> Option<Box<dyn Session>> {
    let p = params()?;
    Some(ClickHouseDriver::new().connect(&p).await.expect("connect"))
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

async fn exec(s: &mut dyn Session, sql: &str) {
    dbiji_core::driver::execute_update(s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

struct TestDb {
    name: String,
}

impl TestDb {
    async fn create(s: &mut dyn Session) -> TestDb {
        let name = unique("dbiji_it");
        exec(s, &format!("CREATE DATABASE {name}")).await;
        TestDb { name }
    }

    async fn drop(self, s: &mut dyn Session) {
        exec(s, &format!("DROP DATABASE IF EXISTS {}", self.name)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_queries_and_types() {
    let Some(mut s) = connect().await else { return };
    let info = s.server_info().clone();
    assert_eq!(info.product, "ClickHouse");
    assert!(info.version_number.0 >= 20, "{info:?}");
    s.ping().await.unwrap();

    let (cols, rows) = query_all(
        s.as_mut(),
        "SELECT toUInt64(18446744073709551615) AS big, toInt128('-170141183460469231731687303715884105728') AS i128, \
         toDecimal64('12.345', 3) AS d, 'héllo\\n' AS s, toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0') AS u, \
         toDate('2024-02-29') AS dt, toDateTime64('2024-01-02 03:04:05.123456', 6) AS ts, [1, 2, NULL] AS arr, \
         map('a', 1) AS m, NULL AS n, true AS b, toIPv4('10.0.0.1') AS ip, nan AS f",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(cols.len(), 13);
    assert_eq!(cols[2].kind, DataKind::Decimal);
    assert_eq!((cols[2].precision, cols[2].scale), (Some(18), Some(3)));
    let r = &rows[0];
    assert_eq!(r[0], Value::UInt(u64::MAX));
    assert_eq!(r[1], Value::decimal("-170141183460469231731687303715884105728"));
    assert_eq!(r[2].as_str(), Some("12.345"));
    assert_eq!(r[3], Value::text("héllo\n"));
    assert!(matches!(r[4], Value::Uuid(_)));
    assert!(matches!(r[5], Value::Date(_)));
    assert_eq!(r[6].to_text(), "2024-01-02 03:04:05.123456");
    assert_eq!(r[7], Value::Array(vec![Value::Int(1), Value::Int(2), Value::Null].into_boxed_slice()));
    assert_eq!(r[8], Value::json("{\"a\":1}"));
    assert_eq!(r[9], Value::Null);
    assert_eq!(r[10], Value::Bool(true));
    assert_eq!(r[11], Value::text("10.0.0.1"));
    assert!(r[12].as_f64().unwrap().is_nan());

    // parameters, INSERT counts, session state (SET, temporary tables)
    exec(s.as_mut(), "CREATE TEMPORARY TABLE tmp_t (a UInt32, b String)").await;
    // NULL into a non-Nullable column becomes the type default in ClickHouse
    let n = dbiji_core::driver::execute_update(
        s.as_mut(),
        "INSERT INTO tmp_t VALUES (?, ?)",
        &[Value::Int(3), Value::Null],
    )
    .await
    .unwrap();
    assert_eq!(n, Some(1));
    let v = dbiji_core::driver::query_scalar(s.as_mut(), "SELECT b FROM tmp_t WHERE a = 3", &[]).await.unwrap();
    assert_eq!(v, Value::text(""));
    let mut cur = s
        .execute("INSERT INTO tmp_t VALUES (?, ?), (?, ?)", &[Value::Int(1), Value::text("x'y"), Value::Int(2), Value::text("z")], &ExecOptions::default())
        .await
        .unwrap();
    match cur.next_result().await.unwrap() {
        Some(ResultHeader::Update { rows_affected, tag, .. }) => {
            assert_eq!(rows_affected, Some(2));
            assert_eq!(tag.as_deref(), Some("INSERT"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(cur.next_result().await.unwrap().is_none());
    cur.close().await.unwrap();
    let v = dbiji_core::driver::query_scalar(s.as_mut(), "SELECT b FROM tmp_t WHERE a = ?", &[Value::Int(1)]).await.unwrap();
    assert_eq!(v, Value::text("x'y"));
    exec(s.as_mut(), "SET max_threads = 3").await;
    let v = dbiji_core::driver::query_scalar(s.as_mut(), "SELECT getSetting('max_threads')", &[]).await.unwrap();
    assert_eq!(v.as_i64(), Some(3));

    // errors: syntax with position, unknown table, mid-stream exception
    let err = query_all(s.as_mut(), "SELEC 1", &[]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert!(err.code.as_deref().unwrap_or("").starts_with("62"), "{err:?}");
    assert_eq!(err.position, Some(1));
    let err = query_all(s.as_mut(), "SELECT * FROM no_such_table_xyz", &[]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Query);
    let mut cur = s
        .execute(
            "SELECT number, throwIf(number = 300000, 'boom mid stream') FROM numbers(400000)",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    // Depending on buffering the exception arrives before the header (HTTP
    // 500) or in the middle of the stream: both must surface as errors.
    let mut n = 0usize;
    let err = match cur.next_result().await {
        Err(e) => e,
        Ok(Some(ResultHeader::Rows(_))) => loop {
            match cur.fetch(10_000).await {
                Ok(b) if b.is_empty() => panic!("expected an error after {n} rows"),
                Ok(b) => n += b.len(),
                Err(e) => break e,
            }
        },
        Ok(other) => panic!("unexpected {other:?}"),
    };
    assert!(err.code.as_deref().unwrap_or("").starts_with("395"), "{err:?}");
    assert!(err.message.contains("boom mid stream"), "{err:?}");
    assert!(n < 400_000);
    cur.close().await.unwrap();

    // user supplied FORMAT: raw lines
    let (cols, rows) = query_all(s.as_mut(), "SELECT 1 AS a, 'x' AS b FORMAT CSV", &[]).await.unwrap();
    assert_eq!(cols[0].name, "CSV");
    assert_eq!(rows, vec![vec![Value::text("1,\"x\"")]]);

    // no transactions
    assert_eq!(s.transaction_mode(), TransactionMode::Auto);
    assert_eq!(s.set_transaction_mode(TransactionMode::Manual).await.unwrap_err().kind, ErrorKind::Unsupported);
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_a_million_rows() {
    let Some(mut s) = connect().await else { return };
    let start = Instant::now();
    let mut cur = s
        .execute("SELECT number, toString(number) FROM numbers(1000000)", &[], &ExecOptions::default())
        .await
        .unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    let mut total = 0usize;
    let mut last = -1i64;
    loop {
        let batch = cur.fetch(10_000).await.unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= 10_000);
        for r in &batch {
            let n = r[0].as_i64().unwrap();
            assert_eq!(n, last + 1);
            last = n;
        }
        total += batch.len();
    }
    assert_eq!(total, 1_000_000);
    assert!(cur.next_result().await.unwrap().is_none());
    cur.close().await.unwrap();
    eprintln!("streamed 1,000,000 rows in {:?}", start.elapsed());

    // Backpressure: read a little of a huge result, wait, and verify the
    // server is still producing (the client does not buffer everything);
    // closing the cursor kills the query.
    let marker = unique("bp");
    let mut cur = s
        .execute(
            &format!("SELECT number, '{marker}' FROM numbers(10000000000)"),
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    cur.next_result().await.unwrap();
    assert_eq!(cur.fetch(100).await.unwrap().len(), 100);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut other = connect().await.unwrap();
    let running = dbiji_core::driver::query_scalar(
        other.as_mut(),
        &format!("SELECT count() FROM system.processes WHERE query LIKE '%{marker}%' AND query NOT LIKE '%system.processes%'"),
        &[],
    )
    .await
    .unwrap();
    assert_eq!(running.as_i64(), Some(1), "the streaming query should still be running");
    cur.close().await.unwrap();
    drop(cur);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let running = dbiji_core::driver::query_scalar(
            other.as_mut(),
            &format!("SELECT count() FROM system.processes WHERE query LIKE '%{marker}%' AND query NOT LIKE '%system.processes%'"),
            &[],
        )
        .await
        .unwrap();
        if running.as_i64() == Some(0) {
            break;
        }
        assert!(Instant::now() < deadline, "query not killed after cursor close");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // the session is usable again
    s.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_and_timeout() {
    let Some(mut s) = connect().await else { return };
    let handle = s.cancel_handle().unwrap();
    let mut cur = s.execute("SELECT count() FROM numbers(100000000000)", &[], &ExecOptions::default()).await.unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        handle.cancel().await
    });
    let start = Instant::now();
    let err = loop {
        match cur.next_result().await {
            Ok(Some(ResultHeader::Rows(_))) => match cur.fetch(10).await {
                Err(e) => break e,
                Ok(_) => continue,
            },
            Ok(other) => panic!("unexpected {other:?}"),
            Err(e) => break e,
        }
    };
    canceller.await.unwrap().unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(20));
    cur.close().await.unwrap();
    drop(cur);

    let opts = ExecOptions { timeout: Some(Duration::from_secs(1)), ..Default::default() };
    let mut cur = s.execute("SELECT count() FROM numbers(100000000000)", &[], &opts).await.unwrap();
    let err = loop {
        match cur.next_result().await {
            Ok(Some(ResultHeader::Rows(_))) => match cur.fetch(10).await {
                Err(e) => break e,
                Ok(_) => continue,
            },
            Ok(other) => panic!("unexpected {other:?}"),
            Err(e) => break e,
        }
    };
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    cur.close().await.unwrap();
    drop(cur);
    s.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_and_ddl_round_trip() {
    let Some(mut s) = connect().await else { return };
    let db = TestDb::create(s.as_mut()).await;
    let d = db.name.clone();
    exec(
        s.as_mut(),
        &format!(
            "CREATE TABLE {d}.events (id UInt64, name String COMMENT 'event name', v Nullable(Float64) DEFAULT 1.5, \
             ts DateTime64(3) DEFAULT now64(3), up String MATERIALIZED upper(name), \
             INDEX idx_name name TYPE bloom_filter GRANULARITY 2, CONSTRAINT c_id CHECK id > 0) \
             ENGINE = MergeTree PARTITION BY toYYYYMM(ts) ORDER BY (id, ts) COMMENT 'all events'"
        ),
    )
    .await;
    exec(s.as_mut(), &format!("INSERT INTO {d}.events (id, name) SELECT number + 1, toString(number) FROM numbers(100)")).await;
    exec(s.as_mut(), &format!("CREATE VIEW {d}.v_events AS SELECT id, name FROM {d}.events WHERE id < 10")).await;
    exec(
        s.as_mut(),
        &format!("CREATE MATERIALIZED VIEW {d}.mv_events ENGINE = MergeTree ORDER BY id AS SELECT id, name FROM {d}.events"),
    )
    .await;

    let schemas = s.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|x| x.name == d));
    assert!(schemas.iter().any(|x| x.name == "system" && x.is_system));
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.iter().any(|x| x.name == d));
    let sref = SchemaRef::schema(d.clone());
    let tables = s.list_objects(&sref, ObjectKind::Table).await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.obj.name.as_str()).collect();
    assert_eq!(names, vec!["events"], "{names:?}");
    assert_eq!(tables[0].row_estimate, Some(100));
    assert_eq!(tables[0].comment.as_deref(), Some("all events"));
    let views = s.list_objects(&sref, ObjectKind::View).await.unwrap();
    assert_eq!(views.len(), 1);
    let mvs = s.list_objects(&sref, ObjectKind::MaterializedView).await.unwrap();
    assert_eq!(mvs.len(), 1);

    let tref = ObjectRef::table(Some(d.clone()), "events");
    let cols = s.list_children(&tref, ObjectKind::Column).await.unwrap();
    assert_eq!(cols.len(), 5);
    assert_eq!(cols[1].comment.as_deref(), Some("event name"));
    let idx = s.list_children(&tref, ObjectKind::Index).await.unwrap();
    assert_eq!(idx.len(), 1);
    assert!(idx[0].detail.as_deref().unwrap().contains("bloom_filter"));
    let pk = s.list_children(&tref, ObjectKind::PrimaryKey).await.unwrap();
    assert_eq!(pk.len(), 1);
    let checks = s.list_children(&tref, ObjectKind::CheckConstraint).await.unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].obj.name, "c_id");

    let t = s.table_definition(&tref).await.unwrap();
    assert_eq!(t.columns.len(), 5);
    assert_eq!(t.columns[2].data_type, "Float64");
    assert!(t.columns[2].nullable);
    assert_eq!(t.columns[2].default.as_deref(), Some("1.5"));
    assert_eq!(t.columns[4].generated.as_deref(), Some("upper(name)"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id", "ts"]);
    assert_eq!(t.options.get("engine").map(String::as_str), Some("MergeTree"));
    assert_eq!(t.options.get("partition_by").map(String::as_str), Some("toYYYYMM(ts)"));
    assert_eq!(t.indexes.len(), 1);
    assert_eq!(t.checks.len(), 1);
    assert_eq!(t.comment.as_deref(), Some("all events"));

    let ddl = s.object_ddl(&tref).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
    let vdef = s.view_definition(&ObjectRef::new(ObjectKind::View, Some(d.clone()), "v_events")).await.unwrap();
    assert!(vdef.definition.to_ascii_uppercase().contains("SELECT"), "{vdef:?}");
    assert!(!vdef.materialized);
    let mv = s.view_definition(&ObjectRef::new(ObjectKind::MaterializedView, Some(d.clone()), "mv_events")).await.unwrap();
    assert!(mv.materialized);
    let deps = s.object_dependencies(&tref).await.unwrap();
    assert!(deps.iter().any(|x| x.target.name == "mv_events"), "{deps:?}");
    let props = s.object_properties(&tref).await.unwrap();
    assert!(props.iter().any(|p| p.name == "Engine" && p.value == "MergeTree"));
    let stats = s.object_statistics(&tref).await.unwrap();
    assert!(stats.iter().any(|p| p.name == "Rows" && p.value == "100"), "{stats:?}");

    // search
    let found = s
        .search_metadata(&MetadataSearch {
            pattern: "event".into(),
            kinds: vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Column],
            schemas: vec![d.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(found.iter().any(|o| o.obj.kind == ObjectKind::Table && o.obj.name == "events"));
    assert!(found.iter().any(|o| o.obj.kind == ObjectKind::View && o.obj.name == "v_events"));
    let found = s
        .search_metadata(&MetadataSearch {
            pattern: "na*".into(),
            kinds: vec![ObjectKind::Column],
            schemas: vec![d.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(found.iter().any(|o| o.obj.name == "name" && o.obj.table.as_deref() == Some("events")));

    // snapshot
    let snap = s.schema_snapshot(&sref, &SnapshotOptions::full()).await.unwrap();
    assert_eq!(snap.tables.len(), 1);
    assert_eq!(snap.tables[0].columns.len(), 5);
    assert_eq!(snap.tables[0].indexes.len(), 1);
    assert_eq!(snap.views.len(), 2);

    // DDL round trip: generate a table from a definition, create, read back
    let dialect = s.dialect();
    let mut def = TableDef::new(Some(d.clone()), "round_trip");
    def.columns.push(ColumnDef::new("id", "UInt32").not_null());
    let mut c = ColumnDef::new("label", "String");
    c.comment = Some("a label".into());
    def.columns.push(c);
    def.columns.push(ColumnDef::new("amount", "Decimal(12, 2)").not_null().default_value("0"));
    def.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
    def.indexes.push(IndexDef {
        name: "idx_label".into(),
        columns: vec![IndexColumn::column("label")],
        method: Some("set(100)".into()),
        ..Default::default()
    });
    def.checks.push(CheckDef { name: Some("positive".into()), expression: "amount >= 0".into() });
    def.comment = Some("round trip".into());
    for stmt in dialect.ddl().create_table(&def, true) {
        exec(s.as_mut(), &stmt).await;
    }
    let rt_ref = ObjectRef::table(Some(d.clone()), "round_trip");
    let back = s.table_definition(&rt_ref).await.unwrap();
    assert_eq!(back.columns.len(), 3);
    assert_eq!(back.columns[1].data_type, "String");
    assert!(back.columns[1].nullable);
    assert_eq!(back.columns[1].comment.as_deref(), Some("a label"));
    assert!(!back.columns[2].nullable);
    assert_eq!(back.columns[2].default.as_deref(), Some("0"));
    assert_eq!(back.primary_key, def.primary_key);
    assert_eq!(back.indexes[0].method.as_deref(), Some("set(100)"));
    assert_eq!(back.checks, def.checks);
    assert_eq!(back.comment.as_deref(), Some("round trip"));

    // alter: add / modify / rename / drop
    let mut to = back.clone();
    to.columns[1].comment = Some("renamed label".into());
    to.columns[1].name = "title".into();
    to.columns.push(ColumnDef::new("flag", "UInt8").not_null().default_value("1"));
    to.columns[2].data_type = "Decimal(18, 4)".into();
    to.indexes.clear();
    to.comment = Some("changed".into());
    let stmts = dialect.ddl().alter_table(&back, &to, &[("label".into(), "title".into())]);
    for stmt in &stmts {
        exec(s.as_mut(), stmt).await;
    }
    let after = s.table_definition(&rt_ref).await.unwrap();
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "title", "amount", "flag"], "{stmts:?}");
    assert_eq!(after.columns[1].comment.as_deref(), Some("renamed label"));
    assert_eq!(after.columns[2].data_type, "Decimal(18, 4)");
    assert!(after.indexes.is_empty());
    assert_eq!(after.comment.as_deref(), Some("changed"));

    // context switching
    s.set_current_schema(&d).await.unwrap();
    assert_eq!(s.current_schema().await.unwrap().as_deref(), Some(d.as_str()));
    let v = dbiji_core::driver::query_scalar(s.as_mut(), "SELECT count() FROM events", &[]).await.unwrap();
    assert_eq!(v.as_i64(), Some(100));
    exec(s.as_mut(), "USE default").await;
    assert_eq!(s.current_database().await.unwrap().as_deref(), Some("default"));

    db.drop(s.as_mut()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_sessions_metrics() {
    let Some(mut s) = connect().await else { return };
    let plan = s.explain("SELECT number % 10 AS k, count() FROM numbers(1000) GROUP BY k ORDER BY k", false).await.unwrap();
    assert!(!plan.roots.is_empty());
    assert!(plan.roots[0].count() >= 2, "{plan:?}");
    assert!(!plan.text.is_empty());
    assert!(plan.raw.is_some());
    assert_eq!(s.explain("SELECT 1", true).await.unwrap_err().kind, ErrorKind::Unsupported);

    // a long query in another session shows up in the session list and can be killed
    let mut other = connect().await.unwrap();
    let marker = unique("sess");
    let mut cur = other
        .execute(&format!("SELECT count(), '{marker}' FROM numbers(100000000000)"), &[], &ExecOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let list = s.list_sessions().await.unwrap();
    let qcol = list.query_column.unwrap();
    let row = list
        .data
        .rows
        .iter()
        .find(|r| r[qcol].as_str().is_some_and(|q| q.contains(&marker)))
        .expect("long query listed");
    let id = row[list.id_column].to_text().into_owned();
    s.kill_session(&id, dbiji_core::admin::KillMode::CancelQuery).await.unwrap();
    let err = loop {
        match cur.next_result().await {
            Ok(Some(_)) => match cur.fetch(10).await {
                Err(e) => break e,
                Ok(_) => continue,
            },
            Ok(None) => panic!("query finished"),
            Err(e) => break e,
        }
    };
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    cur.close().await.unwrap();
    drop(cur);

    let metrics = s.server_metrics().await.unwrap();
    assert!(metrics.iter().any(|m| m.key == "queries.total" && m.value > 0.0), "{metrics:?}");
    assert!(metrics.iter().any(|m| m.key == "server.uptime"));
    assert!(s.list_locks().await.is_err());
}
