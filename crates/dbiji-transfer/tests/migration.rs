//! Migration between two SQLite databases through the `Session` interface.

mod common;

use common::*;
use dbiji_core::{ErrorKind, ObjectRef, Value};
use dbiji_transfer::*;
use std::collections::BTreeMap;
use std::path::Path;

fn source_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE countries (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE, name TEXT);
         CREATE TABLE customers (
             id INTEGER PRIMARY KEY,
             country_id INTEGER REFERENCES countries(id) ON DELETE CASCADE,
             email TEXT NOT NULL,
             balance NUMERIC,
             note TEXT DEFAULT 'none'
         );
         CREATE INDEX customers_email ON customers(email);
         INSERT INTO countries VALUES (1, 'ID', 'Indonesia'), (2, 'JP', 'Japan'), (3, 'FR', 'France');",
    )
    .unwrap();
    let mut stmt = conn.prepare("INSERT INTO customers VALUES (?, ?, ?, ?, ?)").unwrap();
    for i in 1..=50i64 {
        let note = if i % 7 == 0 { rusqlite::types::Value::Null } else { rusqlite::types::Value::Text(format!("note {i} 'q' ü")) };
        stmt.execute(rusqlite::params![i, (i % 3) + 1, format!("user{i}@example.com"), i as f64 * 1.5, note]).unwrap();
    }
}

fn tables() -> Vec<ObjectRef> {
    // Children first on purpose: foreign keys are created after all data.
    vec![ObjectRef::table(None, "customers"), ObjectRef::table(None, "countries")]
}

fn count(path: &Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    select(&conn, &format!("SELECT count(*) FROM {table}"))[0][0].as_i64().unwrap()
}

#[tokio::test]
async fn drop_and_create_copies_definition_data_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let (src_path, dst_path) = (dir.path().join("src.db"), dir.path().join("dst.db"));
    source_db(&src_path);
    // A stale target table is dropped first.
    rusqlite::Connection::open(&dst_path).unwrap().execute_batch("CREATE TABLE customers (x TEXT); INSERT INTO customers VALUES ('stale');").unwrap();
    let source = SqliteFactory::new(&src_path, sqlite_dialect());
    let target = SqliteFactory::new(&dst_path, sqlite_dialect());
    let job = MigrationJob { tables: tables(), mode: MigrationMode::DropAndCreate, batch_size: 7, ..Default::default() };
    let progress = Progress::new();
    let s = run_migration(job, source.clone(), target.clone(), &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.tables.len(), 2);
    assert!(s.tables.iter().all(|t| t.error.is_none() && t.created), "{:?}", s.tables);
    assert_eq!(s.rows, 53);
    assert_eq!(progress.rows(), 53);
    assert_eq!(progress.items_done(), 2);
    assert_eq!(s.tables[0].indexes_created, 1);
    assert_eq!(count(&dst_path, "customers"), 50);
    assert_eq!(count(&dst_path, "countries"), 3);

    let src = rusqlite::Connection::open(&src_path).unwrap();
    let dst = rusqlite::Connection::open(&dst_path).unwrap();
    for t in ["customers", "countries"] {
        let q = format!("SELECT * FROM {t} ORDER BY id");
        assert_eq!(select(&src, &q), select(&dst, &q), "{t}");
    }
    // Same kind: defaults, NOT NULL, unique keys and indexes survive.
    let def = table_def(&dst, None, "customers").unwrap();
    assert_eq!(def.column("note").unwrap().default.as_deref(), Some("'none'"));
    assert!(!def.column("email").unwrap().nullable);
    assert_eq!(def.primary_key.unwrap().columns, vec!["id".to_string()]);
    assert!(def.indexes.iter().any(|i| i.name == "customers_email"));
    let countries = table_def(&dst, None, "countries").unwrap();
    assert_eq!(countries.unique_keys.len(), 1);
    // SQLite cannot add foreign keys to existing tables: reported, not fatal,
    // and attempted only after every row was inserted.
    assert_eq!(s.foreign_keys_created, 0);
    assert!(s.warnings.iter().any(|w| w.contains("foreign key not created")), "{:?}", s.warnings);
    let log = target.log.lock().clone();
    let fk_at = log.iter().position(|q| q.contains("FOREIGN KEY")).expect("foreign key statement");
    let last_insert = log.iter().rposition(|q| q.starts_with("INSERT")).unwrap();
    assert!(fk_at > last_insert, "foreign keys must come last");
    assert!(log.iter().any(|q| q == "DROP TABLE customers"), "{log:?}");
}

#[tokio::test]
async fn append_truncate_replace() {
    let dir = tempfile::tempdir().unwrap();
    let (src_path, dst_path) = (dir.path().join("src.db"), dir.path().join("dst.db"));
    source_db(&src_path);
    let source = SqliteFactory::new(&src_path, sqlite_dialect());
    let target = SqliteFactory::new(&dst_path, sqlite_dialect());
    let cancel = CancellationToken::new();
    let job = MigrationJob { tables: tables(), copy_foreign_keys: false, ..Default::default() };
    run_migration(job.clone(), source.clone(), target.clone(), &Progress::new(), &cancel).await.unwrap();
    assert_eq!(count(&dst_path, "customers"), 50);

    // Appending the same rows again violates the primary key.
    let err = run_migration(job.clone(), source.clone(), target.clone(), &Progress::new(), &cancel).await.unwrap_err();
    assert!(err.full_text().contains("UNIQUE constraint failed"), "{}", err.full_text());
    // ... with SkipRow every row is reported and nothing is duplicated.
    let skip = MigrationJob { on_error: OnError::SkipRow, ..job.clone() };
    let s = run_migration(skip, source.clone(), target.clone(), &Progress::new(), &cancel).await.unwrap();
    assert_eq!(s.tables[0].rows_failed, 50);
    assert_eq!(s.tables[0].rows_written, 0);
    assert_eq!(count(&dst_path, "customers"), 50);

    // Truncate reloads.
    let truncate = MigrationJob { mode: MigrationMode::Truncate, ..job.clone() };
    let s = run_migration(truncate, source.clone(), target.clone(), &Progress::new(), &cancel).await.unwrap();
    assert_eq!(s.rows, 53);
    assert_eq!(count(&dst_path, "customers"), 50);

    // Replace updates changed rows by primary key.
    rusqlite::Connection::open(&src_path).unwrap().execute_batch("UPDATE customers SET email = 'changed@example.com' WHERE id = 5").unwrap();
    let replace = MigrationJob { mode: MigrationMode::Replace, ..job.clone() };
    run_migration(replace, source.clone(), target.clone(), &Progress::new(), &cancel).await.unwrap();
    let dst = rusqlite::Connection::open(&dst_path).unwrap();
    assert_eq!(select(&dst, "SELECT email FROM customers WHERE id = 5"), vec![vec![Value::text("changed@example.com")]]);
    assert_eq!(count(&dst_path, "customers"), 50);
    assert!(target.log.lock().iter().any(|q| q.contains("ON CONFLICT (id) DO UPDATE")));

    // Without create_tables a missing table is an error.
    let missing = MigrationJob {
        tables: vec![ObjectRef::table(None, "countries")],
        create_tables: false,
        table_names: BTreeMap::from([("countries".to_string(), "nowhere".to_string())]),
        ..job
    };
    let err = run_migration(missing, source, target, &Progress::new(), &cancel).await.unwrap_err();
    assert!(err.message.contains("does not exist"), "{}", err.message);
}

#[tokio::test]
async fn cross_product_mapping_and_names() {
    let dir = tempfile::tempdir().unwrap();
    let (src_path, dst_path) = (dir.path().join("src.db"), dir.path().join("dst.db"));
    source_db(&src_path);
    // The target speaks ANSI SQL (another product): types go through map_type,
    // defaults written for the source product are not copied.
    let source = SqliteFactory::new(&src_path, sqlite_dialect());
    let target = SqliteFactory::new(&dst_path, generic());
    let job = MigrationJob {
        tables: tables(),
        name_case: NameCase::Upper,
        table_names: BTreeMap::from([("customers".to_string(), "clients".to_string())]),
        column_names: BTreeMap::from([("customers".to_string(), BTreeMap::from([("email".to_string(), "mail".to_string())]))]),
        column_types: BTreeMap::from([("customers.note".to_string(), "VARCHAR(200)".to_string())]),
        copy_foreign_keys: false,
        transaction_per_table: false,
        batch_size: 16,
        ..Default::default()
    };
    let s = run_migration(job, source, target.clone(), &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows, 53);
    let dst = rusqlite::Connection::open(&dst_path).unwrap();
    let def = table_def(&dst, None, "clients").unwrap();
    let names: Vec<String> = def.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, vec!["ID", "COUNTRY_ID", "mail", "BALANCE", "NOTE"]);
    assert_eq!(def.column("NOTE").unwrap().data_type, "VARCHAR(200)");
    assert_eq!(def.column("NOTE").unwrap().default, None);
    assert_eq!(select(&dst, "SELECT mail FROM clients WHERE ID = 3"), vec![vec![Value::text("user3@example.com")]]);
    assert_eq!(select(&dst, "SELECT count(*) FROM COUNTRIES"), vec![vec![Value::Int(3)]]);
    // Batches were committed one by one.
    assert!(target.log.lock().iter().filter(|q| *q == "COMMIT").count() >= 4);
}

#[tokio::test]
async fn failures_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let (src_path, dst_path) = (dir.path().join("src.db"), dir.path().join("dst.db"));
    source_db(&src_path);
    let source = SqliteFactory::new(&src_path, sqlite_dialect());
    let target = SqliteFactory::new(&dst_path, sqlite_dialect());
    // A missing source table stops the migration ...
    let mut list = tables();
    list.insert(1, ObjectRef::table(None, "ghost"));
    let job = MigrationJob { tables: list.clone(), copy_foreign_keys: false, ..Default::default() };
    let err = run_migration(job, source.clone(), target.clone(), &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.full_text().contains("ghost"), "{}", err.full_text());
    // ... unless failures are tolerated.
    let job = MigrationJob { tables: list, copy_foreign_keys: false, continue_on_table_error: true, ..Default::default() };
    let progress = Progress::new();
    let s = run_migration(job, source.clone(), target.clone(), &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.tables.len(), 3);
    assert_eq!(s.tables.iter().filter(|t| t.error.is_some()).count(), 1);
    assert_eq!(progress.errors(), 1);
    assert_eq!(progress.items_done(), 3);

    // Cancelled before it starts.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let job = MigrationJob { tables: tables(), mode: MigrationMode::DropAndCreate, ..Default::default() };
    let err = run_migration(job, source, target, &Progress::new(), &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
}

#[test]
fn job_is_serialisable() {
    let job = MigrationJob {
        tables: tables(),
        mode: MigrationMode::Replace,
        target_schema: Some("dst".into()),
        table_names: BTreeMap::from([("a".to_string(), "b".to_string())]),
        ..Default::default()
    };
    let s = serde_json::to_string(&job).unwrap();
    assert_eq!(serde_json::from_str::<MigrationJob>(&s).unwrap(), job);
    let partial: MigrationJob = serde_json::from_str(r#"{"tables":[{"kind":"table","database":null,"schema":null,"table":null,"name":"t","signature":null}]}"#).unwrap();
    assert!(partial.create_tables && partial.copy_indexes);
}
