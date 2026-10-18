//! Mock data generation into SQLite.

mod common;

use common::*;
use dbiji_core::ddl::TableDef;
use dbiji_core::{ErrorKind, ObjectRef, Session, Value};
use dbiji_transfer::*;
use std::collections::HashSet;

const SCHEMA: &str = "
    CREATE TABLE countries (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE);
    INSERT INTO countries VALUES (10, 'ID'), (20, 'JP'), (30, 'FR');
    CREATE TABLE customers (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        code INTEGER NOT NULL UNIQUE,
        email VARCHAR(24) NOT NULL UNIQUE,
        first_name VARCHAR(8),
        name VARCHAR(30),
        phone VARCHAR(20),
        city TEXT,
        status VARCHAR(10),
        balance NUMERIC(8,2),
        created_at TIMESTAMP,
        birth_date DATE,
        token UUID,
        country_id INTEGER NOT NULL REFERENCES countries(id),
        active BOOLEAN,
        notes TEXT,
        payload JSON,
        ip TEXT
    );";

async fn customers_def(session: &mut SqliteSession) -> TableDef {
    let mut def = session.table_definition(&ObjectRef::table(None, "customers")).await.unwrap();
    // SQLite's pragmas do not report AUTOINCREMENT; a driver would.
    def.columns[0].auto_increment = true;
    def
}

#[tokio::test]
async fn auto_generators_fill_a_table() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    session.exec_batch(SCHEMA);
    let def = customers_def(&mut session).await;
    let generators = auto_generators(&def);
    let config = MockConfig { table: def.clone(), rows: 300, generators, batch_size: 64, seed: Some(42), ..Default::default() };
    let progress = Progress::new();
    let s = run_mock(&config, &mut session, &progress, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.rows_inserted, 300);
    assert_eq!(progress.rows(), 300);
    assert_eq!(progress.total_rows(), Some(300));
    let rows = select(&session.conn, "SELECT * FROM customers ORDER BY id");
    assert_eq!(rows.len(), 300);
    let idx = |name: &str| def.columns.iter().position(|c| c.name == name).unwrap();
    let mut emails = HashSet::new();
    let mut codes = HashSet::new();
    for r in &rows {
        let email = r[idx("email")].to_text().into_owned();
        assert!(email.contains('@') && [".com", ".org", ".net"].iter().any(|d| email.ends_with(d)), "{email}");
        assert!(email.chars().count() <= 24, "{email}");
        assert!(emails.insert(email));
        assert!(codes.insert(r[idx("code")].as_i64().unwrap()));
        assert!(r[idx("first_name")].to_text().chars().count() <= 8);
        assert!([10, 20, 30].contains(&r[idx("country_id")].as_i64().unwrap()));
        assert!(["active", "inactive", "pending"].contains(&r[idx("status")].to_text().as_ref()));
        let balance: f64 = r[idx("balance")].as_f64().unwrap();
        assert!((1.0..=1000.0).contains(&balance), "{balance}");
        assert!(uuid::Uuid::parse_str(&r[idx("token")].to_text()).is_ok());
        assert!(serde_json::from_str::<serde_json::Value>(&r[idx("payload")].to_text()).is_ok());
        let ip = r[idx("ip")].to_text().into_owned();
        assert_eq!(ip.split('.').count(), 4, "{ip}");
        assert!(dbiji_core::value::parse_datetime(&r[idx("created_at")].to_text()).is_some());
        assert!(dbiji_core::value::parse_date(&r[idx("birth_date")].to_text()).is_some());
    }
    // Codes come from a sequence.
    assert_eq!(rows[0][idx("code")], Value::Int(1));
    assert_eq!(rows[299][idx("code")], Value::Int(300));
}

#[tokio::test]
async fn seeds_reproduce_and_sequences_continue() {
    let mut a = SqliteSession::memory(sqlite_dialect());
    let mut b = SqliteSession::memory(sqlite_dialect());
    a.exec_batch(SCHEMA);
    b.exec_batch(SCHEMA);
    let def = customers_def(&mut a).await;
    let mut gens = auto_generators(&def);
    // Fixed timestamps so both runs are identical.
    for g in &mut gens {
        if let GeneratorKind::DateTimeRange { from, to } = &mut g.generator {
            *from = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
            *to = chrono::NaiveDate::from_ymd_opt(2021, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
        }
    }
    let config = MockConfig { table: def, rows: 50, generators: gens, seed: Some(7), ..Default::default() };
    run_mock(&config, &mut a, &Progress::new(), &CancellationToken::new()).await.unwrap();
    run_mock(&config, &mut b, &Progress::new(), &CancellationToken::new()).await.unwrap();
    let q = "SELECT * FROM customers ORDER BY id";
    assert_eq!(select(&a.conn, q), select(&b.conn, q));
    // A second run continues the unique sequence after MAX(code).
    run_mock(&config, &mut a, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(select(&a.conn, "SELECT min(code), max(code), count(*) FROM customers"), vec![vec![Value::Int(1), Value::Int(100), Value::Int(100)]]);
    // Truncate first starts over.
    let config = MockConfig { mode: MockMode::TruncateFirst, rows: 5, ..config };
    run_mock(&config, &mut a, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(select(&a.conn, "SELECT count(*), max(code) FROM customers"), vec![vec![Value::Int(5), Value::Int(5)]]);
}

#[tokio::test]
async fn explicit_generators_null_ratio_and_errors() {
    let mut session = SqliteSession::memory(sqlite_dialect());
    session.exec_batch("CREATE TABLE t (id INTEGER, label TEXT, price NUMERIC, flag BOOLEAN, kind TEXT, code TEXT)");
    let def = session.table_definition(&ObjectRef::table(None, "t")).await.unwrap();
    let generators = vec![
        ColumnGenerator::new("id", GeneratorKind::Sequence { start: 100, step: 5, after_max: false }),
        ColumnGenerator { null_ratio: 1.0, ..ColumnGenerator::new("label", GeneratorKind::Words { min: 1, max: 2 }) },
        ColumnGenerator::new("price", GeneratorKind::DecimalRange { min: 5.0, max: 5.0, scale: 2 }),
        ColumnGenerator::new("flag", GeneratorKind::Bool { true_ratio: 1.0 }),
        ColumnGenerator::new("kind", GeneratorKind::Enum { values: vec!["a".into()] }),
        ColumnGenerator::new("code", GeneratorKind::Pattern { pattern: "INV-####".into() }),
    ];
    let config = MockConfig { table: def.clone(), rows: 3, generators, ..Default::default() };
    run_mock(&config, &mut session, &Progress::new(), &CancellationToken::new()).await.unwrap();
    let rows = select(&session.conn, "SELECT id, label, price, flag, kind FROM t ORDER BY id");
    assert_eq!(rows[0], vec![Value::Int(100), Value::Null, Value::Int(5), Value::Int(1), Value::text("a")]);
    assert_eq!(rows[2][0], Value::Int(110));
    let code = select(&session.conn, "SELECT code FROM t LIMIT 1")[0][0].to_text().into_owned();
    assert!(code.starts_with("INV-") && code.len() == 8 && code[4..].chars().all(|c| c.is_ascii_digit()), "{code}");

    // Unknown columns and empty referenced tables are reported.
    let bad = MockConfig { generators: vec![ColumnGenerator::new("nope", GeneratorKind::Uuid)], ..config.clone() };
    assert!(run_mock(&bad, &mut session, &Progress::new(), &CancellationToken::new()).await.is_err());
    session.exec_batch("CREATE TABLE parent (id INTEGER PRIMARY KEY); CREATE TABLE child (pid INTEGER NOT NULL)");
    let child = session.table_definition(&ObjectRef::table(None, "child")).await.unwrap();
    let fk = MockConfig {
        table: child,
        rows: 2,
        generators: vec![ColumnGenerator::new("pid", GeneratorKind::ForeignKey { table: ObjectRef::table(None, "parent"), column: "id".into(), group: None })],
        ..Default::default()
    };
    let err = run_mock(&fk, &mut session, &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.message.contains("has no rows"), "{}", err.message);

    // Cancellation stops between batches.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = run_mock(&config, &mut session, &Progress::new(), &cancel).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
}

#[test]
fn config_is_serialisable() {
    let mut def = TableDef::new(None, "t");
    def.columns.push(dbiji_core::ddl::ColumnDef::new("d", "date"));
    let config = MockConfig { table: def.clone(), rows: 10, generators: auto_generators(&def), seed: Some(1), ..Default::default() };
    let s = serde_json::to_string(&config).unwrap();
    assert!(s.contains("\"type\":\"date_range\""), "{s}");
    assert_eq!(serde_json::from_str::<MockConfig>(&s).unwrap(), config);
}
