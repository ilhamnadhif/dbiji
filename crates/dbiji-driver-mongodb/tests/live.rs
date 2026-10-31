//! End-to-end smoke test of the MongoDB driver against a live server.
//!
//! Skipped unless `DBIJI_TEST_MONGO` names a server (`host:port`, for example
//! `127.0.0.1:27018`). It exercises the paths the UI uses: the shell language,
//! catalog, data query, DDL, explain, read-only enforcement and errors.

use dbiji_core::driver::{ConnectParams, ExecOptions};
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbKind, Driver, Session, Value};
use dbiji_driver_mongodb::MongoDriver;
use std::time::Duration;

fn params(database: &str) -> Option<ConnectParams> {
    let addr = std::env::var("DBIJI_TEST_MONGO").ok()?;
    let (host, port) = addr.rsplit_once(':')?;
    Some(ConnectParams {
        kind: DbKind::MongoDb,
        host: host.to_string(),
        port: port.parse().ok()?,
        tls_server_name: None,
        database: Some(database.to_string()),
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
    let Some(params) = params("dbiji_smoke") else {
        eprintln!("DBIJI_TEST_MONGO is not set: skipping");
        return;
    };
    let mut session = MongoDriver::new().connect(&params).await.expect("connect");
    session.ping().await.expect("ping");
    assert_eq!(session.server_info().product, "MongoDB");

    // Clean slate, then writes through the shell language.
    let _ = session.execute("db.users.drop()", &[], &ExecOptions::default()).await;
    query(session.as_mut(), r#"db.users.insertMany([{name: "Ann", age: 31, tags: ["a", "b"]}, {name: "Bob", age: 24}])"#).await;
    let (cols, rows) = query(session.as_mut(), "db.users.countDocuments({})").await;
    assert_eq!(cols, ["count"]);
    assert_eq!(rows[0][0], Value::Int(2));

    // find: one column per field, streaming.
    let (cols, rows) = query(session.as_mut(), "db.users.find({age: {$gt: 30}})").await;
    assert_eq!(cols, ["_id", "name", "age", "tags"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Text("Ann".into()));
    assert_eq!(rows[0][2], Value::Int(31));
    assert!(matches!(&rows[0][3], Value::Json(j) if j.contains("\"a\"")));

    // sort / limit / projection chains.
    let (_, rows) = query(session.as_mut(), "db.users.find({}, {name: 1, _id: 0}).sort({age: -1}).limit(1)").await;
    assert_eq!(rows[0], vec![Value::Text("Ann".into())]);

    // Aggregation.
    let (cols, rows) = query(session.as_mut(), "db.users.aggregate([{$group: {_id: null, total: {$sum: \"$age\"}}}])").await;
    assert_eq!(cols, ["_id", "total"]);
    assert_eq!(rows[0][1], Value::Int(55));

    // Updates and deletes report their counts in the statement header.
    let mut cursor = session.execute("db.users.updateMany({}, {$set: {active: true}})", &[], &ExecOptions::default()).await.expect("update");
    match cursor.next_result().await.expect("header") {
        Some(dbiji_core::ResultHeader::Update { rows_affected, tag, .. }) => {
            assert_eq!(rows_affected, Some(2));
            assert_eq!(tag.as_deref(), Some("matched 2, modified 2"));
        }
        other => panic!("unexpected header {other:?}"),
    }
    cursor.close().await.expect("close cursor");

    // The data editor's read statement.
    let coll = ObjectRef::table(Some("dbiji_smoke".into()), "users");
    let sql = session.dialect().table_data_query(&coll, Some(10));
    assert_eq!(sql, "db.getSiblingDB(\"dbiji_smoke\").getCollection(\"users\").find({}).limit(10)");
    let (cols, _) = query(session.as_mut(), &sql).await;
    assert!(cols.contains(&"active".to_string()), "{cols:?}");

    // Catalog: databases are schemas, collections are tables.
    let schemas = session.list_schemas().await.expect("schemas");
    assert!(schemas.iter().any(|s| s.name == "dbiji_smoke" && s.is_current));
    let objects = session.list_objects(&SchemaRef::schema("dbiji_smoke"), ObjectKind::Table).await.expect("objects");
    assert!(objects.iter().any(|o| o.obj.name == "users"), "{objects:?}");

    // Indexes and fields.
    query(session.as_mut(), "db.users.createIndex({name: 1}, {unique: true})").await;
    let indexes = session.list_children(&coll, ObjectKind::Index).await.expect("indexes");
    assert!(indexes.iter().any(|i| i.obj.name == "name_1" && i.detail.as_deref() == Some("unique {name: 1}")), "{indexes:?}");
    let columns = session.list_children(&coll, ObjectKind::Column).await.expect("columns");
    let names: Vec<&str> = columns.iter().map(|c| c.obj.name.as_str()).collect();
    assert!(names.contains(&"name") && names.contains(&"age"), "{names:?}");

    // DDL, properties and statistics.
    let ddl = session.object_ddl(&coll).await.expect("ddl");
    assert!(ddl.contains("createCollection(\"users\")"), "{ddl}");
    assert!(ddl.contains("createIndex({\"name\": 1}"), "{ddl}");
    let props = session.object_properties(&coll).await.expect("properties");
    assert!(props.iter().any(|p| p.name == "documents (estimated)"), "{props:?}");
    let stats = session.object_statistics(&coll).await.expect("stats");
    assert!(stats.iter().any(|p| p.name == "count"), "{stats:?}");

    // Snapshot (autocomplete, ER diagram).
    let snap = session.schema_snapshot(&SchemaRef::schema("dbiji_smoke"), &Default::default()).await.expect("snapshot");
    assert!(snap.tables.iter().any(|t| t.name == "users"));

    // Metadata search.
    let found =
        session.search_metadata(&dbiji_core::meta::MetadataSearch { pattern: "use".into(), schemas: vec!["dbiji_smoke".into()], ..Default::default() }).await.expect("search");
    assert!(found.iter().any(|o| o.obj.name == "users"), "{found:?}");

    // Explain.
    let plan = session.explain("db.users.find({age: 31})", false).await.expect("explain");
    assert!(!plan.roots.is_empty(), "{plan:?}");
    assert!(!plan.to_text_tree().trim().is_empty());

    // `show` and `use`.
    let (_, rows) = query(session.as_mut(), "show collections").await;
    assert!(rows.iter().any(|r| r[0] == Value::Text("users".into())));
    query(session.as_mut(), "use admin").await;
    assert_eq!(session.current_database().await.unwrap().as_deref(), Some("admin"));
    session.set_current_schema("dbiji_smoke").await.expect("back");

    // Read-only sessions refuse writes and allow reads.
    let mut read_only = MongoDriver::new().connect(&ConnectParams { read_only: true, ..params.clone() }).await.expect("read-only");
    let e = read_only.execute("db.users.insertOne({x: 1})", &[], &ExecOptions::default()).await.err().expect("refused");
    assert_eq!(e.kind, dbiji_core::ErrorKind::ReadOnly);
    assert!(read_only.execute("db.users.find({})", &[], &ExecOptions::default()).await.is_ok());

    // Server errors keep the code.
    query(session.as_mut(), "db.users.updateOne({name: \"Bob\"}, {$set: {age: 31}})").await;
    let e = session.execute("db.users.createIndex({age: 1}, {unique: true})", &[], &ExecOptions::default()).await.err().expect("duplicate");
    assert_eq!(e.code.as_deref(), Some("11000"), "{}", e.full_text());
    // Syntax errors are reported by the parser.
    let e = session.execute("db.users.find({", &[], &ExecOptions::default()).await.err().expect("syntax");
    assert_eq!(e.kind, dbiji_core::ErrorKind::Syntax, "{e}");

    let _ = session.execute("db.users.drop()", &[], &ExecOptions::default()).await;
    session.close().await.expect("close");
}
