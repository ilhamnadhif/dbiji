//! Helpers shared by the integration tests.
#![allow(dead_code)]

use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::{ConnectParams, ConnectionConfig, DbKind, Row, Session, Value};
use dbiji_driver_sqlite::{SqliteDriver, SqliteSession};
use std::path::Path;

/// Connection parameters for `path` with extra properties.
pub fn params(path: &str, props: &[(&str, &str)]) -> ConnectParams {
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite, "test");
    cfg.file_path = Some(path.to_string());
    for (k, v) in props {
        cfg.properties.insert((*k).to_string(), (*v).to_string());
    }
    ConnectParams::from_config(&cfg, None)
}

pub async fn connect_with(path: &str, props: &[(&str, &str)]) -> SqliteSession {
    SqliteDriver::new()
        .connect_sqlite(&params(path, props))
        .await
        .expect("connect")
}

pub async fn connect(path: &str) -> SqliteSession {
    connect_with(path, &[]).await
}

pub async fn memory() -> SqliteSession {
    connect(":memory:").await
}

pub fn file_in(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// Execute statements, panicking on errors.
pub async fn exec(s: &mut SqliteSession, sql: &str) {
    execute_update(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

pub async fn rows(s: &mut SqliteSession, sql: &str) -> Vec<Row> {
    query_all(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .1
}

pub async fn scalar(s: &mut SqliteSession, sql: &str) -> Value {
    query_scalar(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

pub async fn int(s: &mut SqliteSession, sql: &str) -> i64 {
    match scalar(s, sql).await {
        Value::Int(i) => i,
        other => panic!("{sql}: expected an integer, got {other:?}"),
    }
}

/// Close a session explicitly.
pub async fn close(mut s: SqliteSession) {
    s.close().await.expect("close");
}
