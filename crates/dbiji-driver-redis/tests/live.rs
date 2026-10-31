//! End-to-end smoke test of the Redis driver against a live server.
//!
//! Skipped unless `DBIJI_TEST_REDIS` names a server (`host:port`, for example
//! `127.0.0.1:6390`). It exercises the paths the UI uses: connect, execute,
//! catalog, data query, DDL, properties, metrics and the session manager.

use dbiji_core::driver::{ConnectParams, ExecOptions};
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbKind, Driver, Session, Value};
use dbiji_driver_redis::RedisDriver;
use std::time::{Duration, Instant};

fn params() -> Option<ConnectParams> {
    let addr = std::env::var("DBIJI_TEST_REDIS").ok()?;
    let (host, port) = addr.rsplit_once(':')?;
    Some(ConnectParams {
        kind: DbKind::Redis,
        host: host.to_string(),
        port: port.parse().ok()?,
        tls_server_name: None,
        database: Some("0".into()),
        user: None,
        password: None,
        file_path: None,
        ssl: Default::default(),
        properties: Default::default(),
        application_name: "DBiji test".into(),
        connect_timeout: Duration::from_secs(5),
        read_only: false,
        auto_commit: true,
    })
}

async fn query(session: &mut dyn Session, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let (cols, rows) = dbiji_core::driver::query_all(session, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    (cols.into_iter().map(|c| c.name).collect(), rows)
}

#[tokio::test]
async fn a_live_server_round_trip() {
    let Some(params) = params() else {
        eprintln!("DBIJI_TEST_REDIS is not set: skipping");
        return;
    };
    let mut session = RedisDriver::new().connect(&params).await.expect("connect");
    session.ping().await.expect("ping");
    assert_eq!(session.server_info().product, "Redis");

    // Writes and reads through the shell language.
    query(session.as_mut(), "SET dbiji:smoke 1").await;
    let (cols, rows) = query(session.as_mut(), "GET dbiji:smoke").await;
    assert_eq!(cols, ["value"]);
    assert_eq!(rows[0][0], Value::Text("1".into()));
    query(session.as_mut(), "HSET dbiji:hash a 1 b 2").await;
    let (cols, rows) = query(session.as_mut(), "HGETALL dbiji:hash").await;
    assert_eq!(cols, ["field", "value"]);
    assert_eq!(rows.len(), 2);

    // The data editor's read statement, whatever the type.
    let key = ObjectRef::table(Some("0".into()), "dbiji:hash");
    let sql = session.dialect().table_data_query(&key, None);
    let (cols, rows) = query(session.as_mut(), &sql).await;
    assert_eq!(cols, ["field", "value"]);
    assert_eq!(rows.len(), 2);

    // Catalog: databases are schemas, keys are tables, fields are columns.
    let schemas = session.list_schemas().await.expect("schemas");
    assert!(schemas.iter().any(|s| s.name == "0" && s.is_current));
    let objects = session.list_objects(&SchemaRef::schema("0"), ObjectKind::Table).await.expect("objects");
    assert!(objects.iter().any(|o| o.obj.name == "dbiji:hash" && o.detail.as_deref() == Some("hash")), "{objects:?}");
    let columns = session.list_children(&key, ObjectKind::Column).await.expect("columns");
    assert_eq!(columns.len(), 2, "{columns:?}");

    // DDL and properties.
    let ddl = session.object_ddl(&key).await.expect("ddl");
    assert!(ddl.contains("HSET \"dbiji:hash\""), "{ddl}");
    let props = session.object_properties(&key).await.expect("properties");
    assert!(props.iter().any(|p| p.name == "type" && p.value == "hash"), "{props:?}");

    // Snapshot (what autocomplete and the ER diagram read).
    let snap = session.schema_snapshot(&SchemaRef::schema("0"), &Default::default()).await.expect("snapshot");
    assert!(snap.tables.iter().any(|t| t.name == "dbiji:hash"));

    // Metrics and the session manager.
    let metrics = session.server_metrics().await.expect("metrics");
    assert!(metrics.iter().any(|m| m.key == "memory.used"));
    let sessions = session.list_sessions().await.expect("sessions");
    assert!(!sessions.data.rows.is_empty());

    // Another database.
    session.set_current_schema("1").await.expect("select 1");
    assert_eq!(session.current_database().await.unwrap().as_deref(), Some("1"));
    session.set_current_schema("0").await.expect("select 0");

    // Read-only sessions refuse writes.
    let mut read_only = RedisDriver::new().connect(&dbiji_core::driver::ConnectParams { read_only: true, ..params.clone() }).await.expect("connect read-only");
    let e = read_only.execute("SET dbiji:smoke 2", &[], &ExecOptions::default()).await.err().expect("refused");
    assert_eq!(e.kind, dbiji_core::ErrorKind::ReadOnly);
    assert!(read_only.execute("GET dbiji:smoke", &[], &ExecOptions::default()).await.is_ok());

    // An unknown command counts as a write on a read-only connection (the
    // classification is conservative) …
    let e = read_only.execute("NOSUCHCOMMAND a", &[], &ExecOptions::default()).await.err().expect("refused");
    assert_eq!(e.kind, dbiji_core::ErrorKind::ReadOnly);
    // … and on a writable one the server's message comes back.
    let e = session.execute("NOSUCHCOMMAND a", &[], &ExecOptions::default()).await.err().expect("failed");
    assert_eq!(e.kind, dbiji_core::ErrorKind::Query, "{e}");
    assert!(e.message.to_lowercase().contains("unknown command"), "{e}");

    query(session.as_mut(), "DEL dbiji:smoke dbiji:hash").await;
    session.close().await.expect("close");
}

/// redis-rs caps every response at 500 ms by default: a read that took longer
/// — a large key, a paused or busy server — failed with a bare "timed out"
/// ("Context: Reading the key" in the data editor). The cap is off now.
#[tokio::test]
async fn a_slow_server_does_not_fail_a_read() {
    let Some(params) = params() else {
        eprintln!("DBIJI_TEST_REDIS is not set: skipping");
        return;
    };
    let mut session = RedisDriver::new().connect(&params).await.expect("connect");
    query(session.as_mut(), "HSET dbiji:pause f v").await;
    // The server holds every command for 1.2 s: longer than the old 500 ms cap.
    query(session.as_mut(), "CLIENT PAUSE 1200 ALL").await;
    let started = Instant::now();
    let (cols, rows) = query(session.as_mut(), "DBIJI.KEYDATA dbiji:pause").await;
    assert_eq!(cols, ["field", "value"]);
    assert_eq!(rows.len(), 1);
    assert!(started.elapsed() >= Duration::from_millis(1000), "the server did not pause: {:?}", started.elapsed());
    query(session.as_mut(), "DEL dbiji:pause").await;
    session.close().await.expect("close");
}
