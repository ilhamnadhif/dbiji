//! Helpers shared by the integration test binaries.
#![allow(dead_code)]

use dbiji_core::config::{SslConfig, SslMode};
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::{ConnectParams, ConnectionConfig, DbKind, Driver, Secret, Session, Value};
use dbiji_driver_postgres::PostgresDriver;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;

pub fn url() -> Option<String> {
    std::env::var("DBIJI_TEST_PG").ok().filter(|s| !s.is_empty())
}

pub fn dbg_url() -> Option<String> {
    std::env::var("DBIJI_TEST_PGDBG").ok().filter(|s| !s.is_empty())
}

/// Parse `postgres://user:password@host:port/db` into connection parameters.
pub fn params_from_url(url: &str) -> ConnectParams {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, "postgres"));
    let db = db.split('?').next().unwrap_or(db);
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "5432"));
    let mut cfg = ConnectionConfig::new(DbKind::Postgres, "test");
    cfg.host = host.to_string();
    cfg.port = Some(port.parse().unwrap_or(5432));
    cfg.user = Some(user.to_string());
    cfg.database = Some(db.to_string());
    cfg.ssl = SslConfig { mode: SslMode::Prefer, ..Default::default() };
    let mut p = ConnectParams::from_config(&cfg, Some(Secret::new(pass)));
    p.application_name = "dbiji-driver-postgres tests".into();
    p
}

pub fn suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

pub async fn connect(params: &ConnectParams) -> Box<dyn Session> {
    PostgresDriver::new().connect(params).await.expect("connect")
}

pub async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    execute_update(&mut **s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
}

pub async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    query_all(&mut **s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text())).1
}

pub async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Value {
    query_scalar(&mut **s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()))
}

/// Run `body`, then drop `schema` with a fresh connection, then propagate panics.
pub async fn with_schema<F, Fut>(params: &ConnectParams, body: F)
where
    F: FnOnce(ConnectParams, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let schema = format!("dbiji_t_{}", suffix());
    {
        let mut s = connect(params).await;
        exec(&mut s, &format!("CREATE SCHEMA {schema}")).await;
        s.close().await.ok();
    }
    let res = AssertUnwindSafe(body(params.clone(), schema.clone())).catch_unwind().await;
    let mut s = connect(params).await;
    // the schema itself plus the ones a test derived from its name (e.g. `<schema>_copy`)
    let derived = query_all(&mut *s, &format!("SELECT nspname::text FROM pg_namespace WHERE nspname LIKE '{schema}\\_%'"), &[]).await.map(|r| r.1).unwrap_or_default();
    for name in derived.iter().filter_map(|r| r.first().and_then(|v| v.as_str()).map(str::to_string)) {
        let _ = execute_update(&mut *s, &format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE"), &[]).await;
    }
    let _ = execute_update(&mut *s, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[]).await;
    s.close().await.ok();
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

/// Run `body` with a temporary database (dropped afterwards).
pub async fn with_database<F, Fut>(params: &ConnectParams, body: F)
where
    F: FnOnce(ConnectParams, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let db = format!("dbiji_tdb_{}", suffix());
    {
        let mut s = connect(params).await;
        exec(&mut s, &format!("CREATE DATABASE {db}")).await;
        s.close().await.ok();
    }
    let mut p = params.clone();
    p.database = Some(db.clone());
    let res = AssertUnwindSafe(body(p, db.clone())).catch_unwind().await;
    let mut s = connect(params).await;
    if let Err(e) = execute_update(&mut *s, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"), &[]).await {
        eprintln!("cleanup of {db} failed: {}", e.full_text());
    }
    s.close().await.ok();
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

/// Process resident set size in KiB (via `ps`).
pub fn rss_kib() -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub fn tls_dir() -> Option<std::path::PathBuf> {
    std::env::var("DBIJI_TEST_PG_TLS_DIR").ok().filter(|s| !s.is_empty()).map(std::path::PathBuf::from)
}
