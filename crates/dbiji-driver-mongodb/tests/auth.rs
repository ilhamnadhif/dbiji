//! Connection smoke test against a server with authentication.
//!
//! Skipped unless `DBIJI_TEST_MONGO_AUTH` is `user:password@host:port`
//! (optionally `…@host:port/database`).

use dbiji_core::config::AuthMethod;
use dbiji_core::driver::{ConnectParams, ExecOptions};
use dbiji_core::{ConnectionConfig, DbKind, Driver, ErrorKind, Secret};
use dbiji_driver_mongodb::MongoDriver;
use std::time::Duration;

fn params() -> Option<(ConnectParams, String)> {
    let spec = std::env::var("DBIJI_TEST_MONGO_AUTH").ok()?;
    let (auth, hostpart) = spec.rsplit_once('@')?;
    let (user, password) = auth.split_once(':')?;
    let (hostport, db) = match hostpart.split_once('/') {
        Some((h, d)) if !d.is_empty() => (h, d.to_string()),
        Some((h, _)) => (h, "test".to_string()),
        None => (hostpart, "test".to_string()),
    };
    let (host, port) = hostport.rsplit_once(':')?;
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb, "auth");
    cfg.host = host.to_string();
    cfg.port = port.parse().ok();
    cfg.user = Some(user.to_string());
    cfg.database = Some(db.clone());
    cfg.auth = AuthMethod::Password;
    cfg.init.connect_timeout_secs = 5;
    Some((ConnectParams::from_config(&cfg, Some(Secret::new(password))), db))
}

#[tokio::test]
async fn a_server_with_authentication_connects() {
    let Some((params, db)) = params() else {
        eprintln!("DBIJI_TEST_MONGO_AUTH is not set: skipping");
        return;
    };
    let mut session = MongoDriver::new().connect(&params).await.unwrap_or_else(|e| panic!("connect: {}\n{}", e.message, e.full_text()));
    session.ping().await.expect("ping");

    // Authentication must actually be in force: an unauthenticated client is
    // refused, so a successful `list_databases` proves the credential was used.
    let dbs = session.list_databases().await.expect("listDatabases");
    assert!(!dbs.is_empty(), "{dbs:?}");

    let (_, rows) = dbiji_core::driver::query_all(session.as_mut(), "db.items.countDocuments({})", &[]).await.expect("count");
    assert_eq!(rows[0][0], dbiji_core::Value::Int(0));

    session.close().await.expect("close");
    let _ = db;
}

#[tokio::test]
async fn a_wrong_password_is_an_auth_error() {
    let Some((mut params, _)) = params() else {
        return;
    };
    params.password = Some(Secret::new("definitely-not-the-password"));
    let err = MongoDriver::new().connect(&params).await.err().expect("refused");
    assert_eq!(err.kind, ErrorKind::Auth, "{}", err.full_text());
    let _ = ExecOptions::default();
    let _ = Duration::from_secs(1);
}
