//! Integration tests against a live Elasticsearch or OpenSearch.
//!
//! Set `DBIJI_TEST_ELASTIC=http://[user:password@]host:port` (for example
//! `http://127.0.0.1:59200` with security disabled) to run them; without it
//! every test returns immediately.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::*;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ExecOptions, ResultHeader, Secret, Session, Value,
};
use dbiji_driver_elasticsearch::SearchDriver;

fn params() -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_ELASTIC")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let https = url.starts_with("https");
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse().ok()))
        .unwrap_or((hostport.to_string(), None));
    let mut cfg = ConnectionConfig::new(DbKind::Elasticsearch, "test");
    cfg.host = host;
    cfg.port = port.or(Some(9200));
    cfg.user = None;
    cfg.ssl.mode = if https {
        SslMode::Require
    } else {
        SslMode::Disable
    };
    let mut password = None;
    if let Some((u, p)) = auth.and_then(|a| a.split_once(':')) {
        cfg.user = Some(u.to_string());
        password = Some(Secret::new(p));
    }
    Some(ConnectParams::from_config(&cfg, password))
}

async fn connect() -> Option<Box<dyn Session>> {
    Some(
        SearchDriver::new()
            .connect(&params()?)
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
async fn sql_rest_and_navigator() {
    let Some(mut s) = connect().await else { return };
    assert!(matches!(
        s.server_info().product.as_str(),
        "Elasticsearch" | "OpenSearch"
    ));
    let _ = execute_update(s.as_mut(), "DELETE /dbiji-logs", &[]).await;
    run(s.as_mut(), "PUT /dbiji-logs\n{\"mappings\": {\"properties\": {\"msg\": {\"type\": \"text\"}, \"level\": {\"type\": \"keyword\"}, \"n\": {\"type\": \"long\"}, \"at\": {\"type\": \"date\"}, \"user\": {\"properties\": {\"name\": {\"type\": \"keyword\"}}}}}}").await;
    let mut bulk = String::from("POST /dbiji-logs/_bulk?refresh=true\n");
    for i in 0..2500 {
        bulk.push_str(&format!("{{\"index\": {{}}}}\n{{\"msg\": \"event {i}\", \"level\": \"{}\", \"n\": {i}, \"at\": \"2024-01-02T03:04:05Z\", \"user\": {{\"name\": \"u{}\"}}}}\n", if i % 10 == 0 { "error" } else { "info" }, i % 7));
    }
    run(s.as_mut(), &bulk).await;
    // SQL, paged with the cursor.
    let mut c = s
        .execute(
            "SELECT n, level, at FROM \"dbiji-logs\" ORDER BY n",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    let Some(ResultHeader::Rows(cols)) = c.next_result().await.unwrap() else {
        panic!()
    };
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["n", "level", "at"]
    );
    let mut total = 0;
    loop {
        let rows = c.fetch(600).await.unwrap();
        if rows.is_empty() {
            break;
        }
        total += rows.len();
    }
    assert_eq!(total, 2500);
    assert_eq!(
        query_scalar(
            s.as_mut(),
            "SELECT COUNT(*) FROM \"dbiji-logs\" WHERE level = 'error'",
            &[]
        )
        .await
        .unwrap(),
        Value::Int(250)
    );
    // REST: hits become rows.
    let (cols, rows) = query_all(s.as_mut(), "GET /dbiji-logs/_search\n{\"size\": 3, \"query\": {\"term\": {\"level\": \"error\"}}, \"sort\": [{\"n\": \"asc\"}]}", &[]).await.unwrap();
    assert!(cols.iter().any(|c| c.name == "_id") && cols.iter().any(|c| c.name == "msg"));
    assert_eq!(rows.len(), 3);
    let e = query_all(s.as_mut(), "SELECT nope FROM \"dbiji-logs\"", &[])
        .await
        .unwrap_err();
    assert!(e.message.contains("nope"), "{e:?}");
    // Navigator and the data tab.
    let tables = s
        .list_objects(&SchemaRef::schema("x"), ObjectKind::Table)
        .await
        .unwrap();
    let logs = tables
        .iter()
        .find(|t| t.obj.name == "dbiji-logs")
        .expect("index listed");
    assert_eq!(logs.row_estimate, Some(2500));
    let fields = s
        .list_children(&logs.obj, ObjectKind::Column)
        .await
        .unwrap();
    assert!(
        fields
            .iter()
            .any(|f| f.obj.name == "user.name" && f.detail.as_deref() == Some("keyword"))
    );
    let d = s.dialect();
    let read = d.select_table(&logs.obj, None, Some("level = 'error'"), None, Some(5));
    assert_eq!(query_all(s.as_mut(), &read, &[]).await.unwrap().1.len(), 5);
    assert_eq!(
        query_scalar(s.as_mut(), &d.count_query(&read), &[])
            .await
            .unwrap(),
        Value::Int(250)
    );
    let ddl = s.object_ddl(&logs.obj).await.unwrap();
    assert!(
        ddl.starts_with("PUT /dbiji-logs") && ddl.contains("\"keyword\""),
        "{ddl}"
    );
    assert!(
        !s.explain("SELECT * FROM \"dbiji-logs\" WHERE n > 3", false)
            .await
            .unwrap()
            .text
            .is_empty()
    );
    assert!(
        s.server_metrics()
            .await
            .unwrap()
            .iter()
            .any(|m| m.key == "docs")
    );
    run(s.as_mut(), "DELETE /dbiji-logs").await;
}
