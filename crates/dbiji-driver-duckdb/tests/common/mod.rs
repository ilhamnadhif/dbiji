//! Helpers shared by the integration tests.
#![allow(dead_code)]

use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::{ConnectParams, ConnectionConfig, DbKind, Row, Session, Value};
use dbiji_driver_duckdb::{DuckDbDriver, DuckDbSession};
use std::path::Path;

/// Connection parameters for `path` with extra properties.
pub fn params(path: &str, props: &[(&str, &str)]) -> ConnectParams {
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb, "test");
    cfg.file_path = Some(path.to_string());
    for (k, v) in props {
        cfg.properties.insert((*k).to_string(), (*v).to_string());
    }
    let mut p = ConnectParams::from_config(&cfg, None);
    // keep the shared test machine responsive
    p.properties
        .entry("threads".into())
        .or_insert_with(|| "2".into());
    p
}

pub async fn connect_with(path: &str, props: &[(&str, &str)]) -> DuckDbSession {
    DuckDbDriver::new()
        .connect_duckdb(&params(path, props))
        .await
        .expect("connect")
}

pub async fn connect(path: &str) -> DuckDbSession {
    connect_with(path, &[]).await
}

pub async fn memory() -> DuckDbSession {
    connect(":memory:").await
}

pub fn file_in(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// Execute statements, panicking on errors.
pub async fn exec(s: &mut DuckDbSession, sql: &str) {
    execute_update(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

pub async fn rows(s: &mut DuckDbSession, sql: &str) -> Vec<Row> {
    query_all(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .1
}

pub async fn scalar(s: &mut DuckDbSession, sql: &str) -> Value {
    query_scalar(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

pub async fn int(s: &mut DuckDbSession, sql: &str) -> i64 {
    match scalar(s, sql).await {
        Value::Int(i) => i,
        other => panic!("{sql}: expected an integer, got {other:?}"),
    }
}

/// Close a session explicitly.
pub async fn close(mut s: DuckDbSession) {
    s.close().await.expect("close");
}
