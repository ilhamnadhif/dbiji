//! ALTERs of tables other entries depend on (DuckDB "Dependency Error").
mod common;

use dbiji_core::driver::{execute_update, query_scalar};
use dbiji_core::{ObjectRef, Session, Value};

async fn run(s: &mut dbiji_driver_duckdb::DuckDbSession, sql: &[&str]) {
    for q in sql {
        execute_update(s, q, &[])
            .await
            .unwrap_or_else(|e| panic!("{q}: {}", e.message));
    }
}

#[tokio::test]
async fn alter_keeps_indexes_of_the_table() {
    let mut s = common::memory().await;
    run(
        &mut s,
        &[
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name VARCHAR, price DECIMAL(10,2))",
            "CREATE INDEX ix_name ON items(name)",
            "INSERT INTO items VALUES (1, 'a', 2.5)",
        ],
    )
    .await;
    let table = ObjectRef::table(Some("main".into()), "items");
    let from = s.table_definition(&table).await.unwrap();
    let mut to = from.clone();
    to.columns[1].name = "title".into();
    to.columns[2].data_type = "DECIMAL(12,2)".into();
    to.columns[2].nullable = false;
    for c in to.indexes.iter_mut().flat_map(|i| i.columns.iter_mut()) {
        if c.name == "name" {
            c.name = "title".into();
        }
    }
    let sql = s
        .dialect()
        .ddl()
        .alter_table(&from, &to, &[("name".into(), "title".into())]);
    for q in &sql {
        execute_update(&mut s, q, &[])
            .await
            .unwrap_or_else(|e| panic!("{q}: {}", e.message));
    }
    let after = s.table_definition(&table).await.unwrap();
    assert!(
        after
            .indexes
            .iter()
            .any(|i| i.name == "ix_name" && i.columns.iter().any(|c| c.name == "title")),
        "{:?}",
        after.indexes
    );
    assert_eq!(
        query_scalar(&mut s, "SELECT title FROM items", &[])
            .await
            .unwrap(),
        Value::text("a")
    );
    let _ = s.close().await;
}

#[tokio::test]
async fn referenced_table_change_fails_with_a_clear_message() {
    let mut s = common::memory().await;
    run(
        &mut s,
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY, name VARCHAR)",
            "CREATE TABLE c (id INTEGER, p_id INTEGER REFERENCES p(id))",
        ],
    )
    .await;
    let from = s
        .table_definition(&ObjectRef::table(Some("main".into()), "p"))
        .await
        .unwrap();
    let mut to = from.clone();
    to.columns[1].name = "title".into();
    let sql = s
        .dialect()
        .ddl()
        .alter_table(&from, &to, &[("name".into(), "title".into())]);
    let err = execute_update(&mut s, &sql[0], &[]).await.unwrap_err();
    assert!(
        err.message.contains("reference it with foreign keys"),
        "{}",
        err.message
    );
    let _ = s.close().await;
}
