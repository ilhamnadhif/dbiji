//! Integration tests against a live Cassandra or ScyllaDB node.
//!
//! Set `DBIJI_TEST_CASSANDRA=cassandra://[user:password@]host:port` (for
//! example `cassandra://127.0.0.1:59042` with `scylladb/scylla
//! --developer-mode 1`) to run them; without it every test returns
//! immediately. Each test works in a keyspace of its own.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ExecOptions, ResultHeader, Secret, Session, Value,
};
use dbiji_driver_cassandra::CassandraDriver;

fn params() -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_CASSANDRA")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let (host, port) = hostport
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse().ok()))
        .unwrap_or((hostport.to_string(), None));
    let mut cfg = ConnectionConfig::new(DbKind::Cassandra, "test");
    cfg.host = host;
    cfg.port = port.or(Some(9042));
    cfg.ssl.mode = SslMode::Disable;
    cfg.init.connect_timeout_secs = 30;
    let mut password = None;
    if let Some((u, p)) = auth.and_then(|a| a.split_once(':')) {
        cfg.user = Some(u.to_string());
        password = Some(Secret::new(p));
    }
    Some(ConnectParams::from_config(&cfg, password))
}

async fn connect() -> Option<Box<dyn Session>> {
    Some(
        CassandraDriver::new()
            .connect(&params()?)
            .await
            .expect("connect"),
    )
}

async fn exec(s: &mut dyn Session, sql: &str) {
    execute_update(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
}

async fn keyspace(s: &mut dyn Session, name: &str) {
    exec(s, &format!("DROP KEYSPACE IF EXISTS {name}")).await;
    exec(s, &format!("CREATE KEYSPACE {name} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}")).await;
}

#[tokio::test]
async fn reads_writes_and_binds_cql_values() {
    let Some(mut s) = connect().await else { return };
    assert!(matches!(
        s.server_info().product.as_str(),
        "ScyllaDB" | "Cassandra"
    ));
    keyspace(s.as_mut(), "dbiji_values").await;
    exec(
        s.as_mut(),
        "CREATE TABLE dbiji_values.items (id int PRIMARY KEY, name text, price decimal, tags list<text>, attrs map<text, int>, born date, at timestamp, uid uuid, ok boolean, data blob)",
    )
    .await;
    exec(
        s.as_mut(),
        "INSERT INTO dbiji_values.items (id, name, price, tags, attrs, born, at, uid, ok, data) VALUES (1, 'apple', 1.25, ['red', 'fruit'], {'kcal': 52}, '2024-01-02', '2024-01-02 03:04:05+0000', 5f1d3f0e-5d4e-4a35-9b0a-6d3f8e3b6a11, true, 0xcafe)",
    )
    .await;
    // Parameters are converted to the column types: text for an int, an int for a bigint ...
    execute_update(
        s.as_mut(),
        "INSERT INTO dbiji_values.items (id, name, born, at, ok) VALUES (?, ?, ?, ?, ?)",
        &[
            Value::Text("2".into()),
            Value::Text("pear".into()),
            Value::Text("2024-05-06".into()),
            Value::Text("2024-05-06 07:08:09".into()),
            Value::Bool(false),
        ],
    )
    .await
    .unwrap();
    let (cols, rows) = query_all(s.as_mut(), "SELECT id, name, price, tags, attrs, born, at, uid, ok, data FROM dbiji_values.items WHERE id = 1", &[]).await.unwrap();
    assert_eq!(cols[0].kind, DataKind::Int);
    assert_eq!(cols[3].type_name, "list<text>");
    assert_eq!(
        cols[0].source.as_ref().map(|s| s.table.as_str()),
        Some("items")
    );
    let r = &rows[0];
    assert_eq!(r[0], Value::Int(1));
    assert_eq!(r[1], Value::Text("apple".into()));
    assert_eq!(r[2], Value::Decimal("1.25".into()));
    assert_eq!(r[3], Value::Json("[\"red\",\"fruit\"]".into()));
    assert_eq!(r[4], Value::Json("{\"kcal\":52}".into()));
    assert_eq!(r[5].to_text(), "2024-01-02");
    assert_eq!(r[6].to_text(), "2024-01-02 03:04:05+00");
    assert_eq!(r[7].to_text(), "5f1d3f0e-5d4e-4a35-9b0a-6d3f8e3b6a11");
    assert_eq!(r[8], Value::Bool(true));
    assert_eq!(r[9], Value::Bytes(vec![0xca, 0xfe].into()));
    let (_, rows) = query_all(
        s.as_mut(),
        "SELECT name, born FROM dbiji_values.items WHERE id = ?",
        &[Value::Int(2)],
    )
    .await
    .unwrap();
    assert_eq!(rows[0][0], Value::Text("pear".into()));
    assert_eq!(rows[0][1].to_text(), "2024-05-06");
    // Grid edits: UPDATE by primary key with bound values.
    execute_update(
        s.as_mut(),
        "UPDATE dbiji_values.items SET name = ? WHERE id = ?",
        &[Value::Text("green apple".into()), Value::Int(1)],
    )
    .await
    .unwrap();
    assert_eq!(
        query_scalar(
            s.as_mut(),
            "SELECT name FROM dbiji_values.items WHERE id = 1",
            &[]
        )
        .await
        .unwrap(),
        Value::Text("green apple".into())
    );
    // Counting and limiting without subqueries, as the data view does.
    let d = s.dialect();
    let select = d.select_table(
        &ObjectRef::table(Some("dbiji_values".into()), "items"),
        None,
        None,
        None,
        Some(10),
    );
    assert_eq!(
        query_all(s.as_mut(), &select, &[]).await.unwrap().1.len(),
        2
    );
    assert_eq!(
        query_scalar(s.as_mut(), &d.count_query(&select), &[])
            .await
            .unwrap(),
        Value::Int(2)
    );
    let e = query_all(s.as_mut(), "SELEC * FROM dbiji_values.items", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, dbiji_core::ErrorKind::Syntax, "{e:?}");
    exec(s.as_mut(), "DROP KEYSPACE dbiji_values").await;
}

#[tokio::test]
async fn catalog_of_keyspaces_and_tables() {
    let Some(mut s) = connect().await else { return };
    keyspace(s.as_mut(), "dbiji_catalog").await;
    exec(s.as_mut(), "CREATE TABLE dbiji_catalog.events (tenant text, day date, at timestamp, kind text, payload text, PRIMARY KEY ((tenant, day), at)) WITH CLUSTERING ORDER BY (at DESC) AND comment = 'Events'").await;
    exec(
        s.as_mut(),
        "CREATE INDEX events_kind ON dbiji_catalog.events (kind)",
    )
    .await;
    let ks = s.list_schemas().await.unwrap();
    assert!(ks.iter().any(|k| k.name == "dbiji_catalog" && !k.is_system));
    assert!(ks.iter().any(|k| k.name == "system" && k.is_system));
    let tables = s
        .list_objects(&SchemaRef::schema("dbiji_catalog"), ObjectKind::Table)
        .await
        .unwrap();
    let ev = tables
        .iter()
        .find(|t| t.obj.name == "events")
        .expect("table listed");
    assert_eq!(ev.comment.as_deref(), Some("Events"));
    let t = s.table_definition(&ev.obj).await.unwrap();
    assert_eq!(
        t.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["tenant", "day", "at", "kind", "payload"]
    );
    assert_eq!(
        t.primary_key.as_ref().unwrap().columns,
        ["tenant", "day", "at"]
    );
    assert_eq!(
        t.options.get("partition_key").map(String::as_str),
        Some("tenant, day")
    );
    assert_eq!(
        t.options.get("clustering_order").map(String::as_str),
        Some("at DESC")
    );
    assert_eq!(t.indexes.len(), 1);
    let cols = s.list_children(&ev.obj, ObjectKind::Column).await.unwrap();
    assert_eq!(cols[0].detail.as_deref(), Some("text partition key"));
    let ddl = s.object_ddl(&ev.obj).await.unwrap();
    assert!(
        ddl.contains("CREATE TABLE") && ddl.contains("events"),
        "{ddl}"
    );
    // The generated DDL recreates the table.
    let generated = s.dialect().ddl().create_table(&t, false);
    exec(s.as_mut(), "DROP TABLE dbiji_catalog.events").await;
    for stmt in &generated {
        exec(s.as_mut(), stmt).await;
    }
    assert_eq!(
        s.table_definition(&ev.obj).await.unwrap().primary_key,
        t.primary_key
    );
    let snap = s
        .schema_snapshot(
            &SchemaRef::schema("dbiji_catalog"),
            &dbiji_core::ddl::SnapshotOptions::light(),
        )
        .await
        .unwrap();
    assert!(snap.table("events").is_some());
    exec(s.as_mut(), "DROP KEYSPACE dbiji_catalog").await;
}

#[tokio::test]
async fn results_are_paged() {
    let Some(mut s) = connect().await else { return };
    keyspace(s.as_mut(), "dbiji_paging").await;
    exec(
        s.as_mut(),
        "CREATE TABLE dbiji_paging.n (id int PRIMARY KEY)",
    )
    .await;
    for i in 0..2500 {
        execute_update(
            s.as_mut(),
            "INSERT INTO dbiji_paging.n (id) VALUES (?)",
            &[Value::Int(i)],
        )
        .await
        .unwrap();
    }
    let mut c = s
        .execute(
            "SELECT id FROM dbiji_paging.n",
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
    loop {
        let rows = c.fetch(700).await.unwrap();
        if rows.is_empty() {
            break;
        }
        total += rows.len();
    }
    assert_eq!(total, 2500);
    exec(s.as_mut(), "DROP KEYSPACE dbiji_paging").await;
}
