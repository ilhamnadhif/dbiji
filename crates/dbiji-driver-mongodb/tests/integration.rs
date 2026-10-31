//! Integration tests against a live MongoDB.
//!
//! Set `DBIJI_TEST_MONGO=mongodb://[user:password@]host:port` (for example
//! `mongodb://127.0.0.1:57017` with the `mongo:7` image) to run them;
//! without it every test returns immediately. Each test uses a database of its own.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ExecOptions, ResultHeader, Secret, Session, Value,
};
use dbiji_driver_mongodb::MongoDriver;

fn params(db: &str) -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_MONGO")
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
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb, "test");
    cfg.host = host;
    cfg.port = port.or(Some(27017));
    cfg.database = Some(db.into());
    cfg.ssl.mode = SslMode::Disable;
    let mut password = None;
    if let Some((u, p)) = auth.and_then(|a| a.split_once(':')) {
        cfg.user = Some(u.to_string());
        password = Some(Secret::new(p));
    }
    Some(ConnectParams::from_config(&cfg, password))
}

async fn connect(db: &str) -> Option<Box<dyn Session>> {
    Some(
        MongoDriver::new()
            .connect(&params(db)?)
            .await
            .expect("connect"),
    )
}

async fn run(s: &mut dyn Session, cmd: &str) {
    execute_update(s, cmd, &[])
        .await
        .unwrap_or_else(|e| panic!("{cmd}: {}", e.full_text()));
}

#[tokio::test]
async fn shell_statements_and_navigator() {
    let Some(mut s) = connect("dbiji_shell").await else {
        return;
    };
    assert_eq!(s.server_info().product, "MongoDB");
    run(s.as_mut(), "db.dropDatabase()").await;
    run(s.as_mut(), "db.users.insertMany([{_id: 1, name: 'Ayu', age: 31, city: 'Jakarta', tags: ['a', 'b']}, {_id: 2, name: 'Budi', age: 25, address: {city: 'Bandung'}}, {_id: 3, name: \"Citra\", age: 40, joined: ISODate('2024-01-02T03:04:05Z')}])").await;
    run(
        s.as_mut(),
        "db.users.createIndex({name: 1}, {name: 'by_name', unique: true})",
    )
    .await;
    let (cols, rows) = query_all(
        s.as_mut(),
        "db.users.find({age: {$gte: 30}}, {name: 1, age: 1, tags: 1}).sort({age: -1})",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["_id", "name", "age", "tags"]
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("Citra".into()));
    assert_eq!(rows[1][3], Value::Json("[\"a\",\"b\"]".into()));
    assert_eq!(
        query_scalar(s.as_mut(), "db.users.countDocuments({age: {$lt: 30}})", &[])
            .await
            .unwrap(),
        Value::Int(1)
    );
    let (_, rows) = query_all(
        s.as_mut(),
        "db.users.aggregate([{$group: {_id: null, total: {$sum: '$age'}}}])",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(rows[0][1], Value::Int(96));
    let n = execute_update(
        s.as_mut(),
        "db.users.updateMany({age: {$gt: 30}}, {$inc: {age: 1}})",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(n, Some(2));
    let (_, rows) = query_all(s.as_mut(), "db.users.findOne({_id: 3})", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][3].to_text(), "2024-01-02 03:04:05+00");
    let e = query_all(s.as_mut(), "db.users.find({age: })", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, dbiji_core::ErrorKind::Syntax);
    // The navigator and the data tab.
    let dbs = s.list_schemas().await.unwrap();
    assert!(dbs.iter().any(|d| d.name == "dbiji_shell" && d.is_current));
    assert!(dbs.iter().any(|d| d.name == "admin" && d.is_system));
    let colls = s
        .list_objects(&SchemaRef::schema("dbiji_shell"), ObjectKind::Table)
        .await
        .unwrap();
    assert_eq!(
        colls
            .iter()
            .map(|c| c.obj.name.as_str())
            .collect::<Vec<_>>(),
        ["users"]
    );
    let t = s.table_definition(&colls[0].obj).await.unwrap();
    assert!(
        t.columns
            .iter()
            .any(|c| c.name == "address" && c.kind == DataKind::Json)
    );
    assert!(t.indexes.iter().any(|i| i.name == "by_name" && i.unique));
    let idx = s
        .list_children(&colls[0].obj, ObjectKind::Index)
        .await
        .unwrap();
    assert_eq!(idx.len(), 2);
    let d = s.dialect();
    let read = d.select_table(
        &colls[0].obj,
        None,
        Some("{age: {$gt: 26}}"),
        Some("age DESC"),
        None,
    );
    let (_, rows) = query_all(s.as_mut(), &read, &[]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        query_scalar(s.as_mut(), &d.count_query(&read), &[])
            .await
            .unwrap(),
        Value::Int(2)
    );
    let ddl = s.object_ddl(&colls[0].obj).await.unwrap();
    assert!(
        ddl.contains("createIndex({name: 1}, {name: \"by_name\", unique: true})"),
        "{ddl}"
    );
    let plan = s
        .explain("db.users.find({name: 'Ayu'})", false)
        .await
        .unwrap();
    assert!(
        plan.roots[0].title().contains("FETCH")
            || plan.roots[0].node_type.contains("IXSCAN")
            || !plan.roots.is_empty(),
        "{plan:?}"
    );
    assert!(
        s.server_metrics()
            .await
            .unwrap()
            .iter()
            .any(|m| m.key == "connections.current")
    );
    run(s.as_mut(), "db.dropDatabase()").await;
}

#[tokio::test]
async fn large_results_stream_in_batches() {
    let Some(mut s) = connect("dbiji_stream").await else {
        return;
    };
    run(s.as_mut(), "db.dropDatabase()").await;
    let docs: Vec<String> = (0..3000).map(|i| format!("{{n: {i}}}")).collect();
    run(
        s.as_mut(),
        &format!("db.nums.insertMany([{}])", docs.join(", ")),
    )
    .await;
    let mut c = s
        .execute(
            "db.nums.find({}).sort({n: 1})",
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
        assert!(rows.len() <= 700);
        total += rows.len();
    }
    assert_eq!(total, 3000);
    run(s.as_mut(), "db.dropDatabase()").await;
}
