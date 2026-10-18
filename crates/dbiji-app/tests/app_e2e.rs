//! End-to-end tests of the application plumbing (connection manager,
//! sessions, execution engine, incremental fetching, metadata cache) against
//! real servers. Every test returns early unless its server URL is set:
//!
//! * `DBIJI_TEST_MYSQL`   — `mysql://root:pw@127.0.0.1:3306/db`
//! * `DBIJI_TEST_MARIADB` — `mariadb://root:pw@127.0.0.1:3307/db`
//! * `DBIJI_TEST_PG`      — `postgres://user:pw@127.0.0.1:5432/db`
//!
//! Each test works on uniquely named tables that it drops again.

use dbiji_app::backend::Backend;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_app::exec::{ExecEvent, ExecOpts, Execution, StmtPlan};
use dbiji_app::results::source::ResultSource;
use dbiji_core::config::{SecretSlot, secret_key};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn servers() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (name, var) in [("mysql", "DBIJI_TEST_MYSQL"), ("mariadb", "DBIJI_TEST_MARIADB"), ("postgres", "DBIJI_TEST_PG")] {
        if let Ok(url) = std::env::var(var) {
            if !url.trim().is_empty() {
                out.push((name, url));
            }
        }
    }
    out
}

fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Connect through the connection manager exactly like the UI does.
fn connect(backend: &Backend, url: &str) -> (ConnectionManager, String) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (mut cfg, password) = dbiji_config::parse_connection_url(url).expect("valid test URL");
    cfg.name = "e2e".into();
    cfg.save_password = true;
    let secrets = Arc::new(MemorySecrets::default());
    if let Some(pw) = &password {
        secrets.set(&secret_key(&cfg.id, SecretSlot::Password), pw).unwrap();
    }
    let mut cm = ConnectionManager::new(dbiji_app::driver_registry(), secrets, Arc::new(DirectNet));
    let id = cfg.id.clone();
    cm.upsert(cfg);
    assert!(cm.missing_credentials(&id).is_empty(), "password should come from the secret store");
    cm.connect(backend, &id, Credentials::default()).expect("driver registered");
    wait_until(
        || {
            cm.poll(backend);
            cm.is_connected(&id) || !cm.errors().is_empty()
        },
        "connect",
    );
    assert!(cm.errors().is_empty(), "connect failed: {:?}", cm.errors());
    (cm, id)
}

fn run_script(backend: &Backend, cm: &ConnectionManager, id: &str, stmts: &[&str]) -> Vec<ExecEvent> {
    let session = cm.live(id).unwrap().main.clone();
    let plans: Vec<StmtPlan> = stmts
        .iter()
        .enumerate()
        .map(|(i, s)| StmtPlan { index: i, sql: s.to_string(), original: s.to_string(), range: 0..s.len(), is_write: true, client: None })
        .collect();
    let mut exec = Execution::start(backend, session, plans, ExecOpts { fetch_size: 200, ..Default::default() }, None);
    let mut events = Vec::new();
    wait_until(
        || {
            events.extend(exec.poll());
            exec.is_finished()
        },
        "execution",
    );
    events.extend(exec.poll());
    events
}

fn errors(events: &[ExecEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Error { error, .. } => Some(error.full_text()),
            _ => None,
        })
        .collect()
}

#[test]
fn execute_fetch_count_and_metadata() {
    for (name, url) in servers() {
        let backend = Backend::new().unwrap();
        let (mut cm, id) = connect(&backend, &url);
        let table = format!("dbiji_e2e_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let is_pg = name == "postgres";
        let create = format!("CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(40), amount DECIMAL(10,2))");
        let insert = if is_pg {
            format!("INSERT INTO {table} SELECT g, 'row ' || g, g * 1.5 FROM generate_series(1, 1000) g")
        } else {
            format!(
                "INSERT INTO {table} WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1000) SELECT n, CONCAT('row ', n), n * 1.5 FROM seq"
            )
        };
        let select = format!("SELECT * FROM {table} ORDER BY id");
        let events = run_script(&backend, &cm, &id, &[&create, &insert, &select]);
        assert!(errors(&events).is_empty(), "{name}: {:?}", errors(&events));
        let updated: Vec<Option<u64>> = events.iter().filter_map(|e| if let ExecEvent::Update { index: 1, rows, .. } = e { Some(*rows) } else { None }).collect();
        assert_eq!(updated, vec![Some(1000)], "{name}: insert count");
        let (columns, store, token, exhausted) = events
            .into_iter()
            .find_map(|e| match e {
                ExecEvent::Rows { columns, store, token, exhausted, .. } => Some((columns, store, token, exhausted)),
                _ => None,
            })
            .expect("row result");
        assert_eq!(columns.len(), 3, "{name}");
        assert_eq!(store.lock().len(), 200, "{name}: first page");
        assert!(!exhausted, "{name}: more rows to fetch");

        // Fetch the rest on demand, then count.
        let session = cm.live(&id).unwrap().main.clone();
        let mut src = ResultSource::from_executed(session.clone(), select.clone(), Vec::new(), columns, store, token, exhausted, 64 << 20);
        src.fetch_more(&backend, usize::MAX, None);
        wait_until(
            || {
                src.poll();
                !src.is_fetching()
            },
            "fetch all",
        );
        assert!(src.error.is_none(), "{name}: {:?}", src.error);
        assert_eq!(src.row_count(), 1000, "{name}: all rows");
        assert!(!src.has_more());
        let first = src.store.lock().get(0).cloned().unwrap();
        assert_eq!(first[0].as_i64(), Some(1), "{name}: ordered");
        src.count_total(&backend, session.dialect());
        wait_until(
            || {
                src.poll();
                !src.total_count.is_loading()
            },
            "count",
        );
        assert_eq!(src.total_count.get().copied(), Some(1000), "{name}: count(*)");

        // Refresh re-executes and starts over with the first page.
        src.refresh(&backend, 50);
        wait_until(
            || {
                src.poll();
                !src.is_fetching()
            },
            "refresh",
        );
        assert_eq!(src.row_count(), 50, "{name}: refreshed first page");

        // The navigator's metadata cache sees the new table.
        let live = cm.live_mut(&id).unwrap();
        let schema = dbiji_core::meta::SchemaRef::new(None, live.current_schema.clone().or(live.current_database.clone()));
        let main = live.main.clone();
        live.meta.request_objects(&backend, &main, &schema, dbiji_core::meta::ObjectKind::Table);
        wait_until(
            || {
                let live = cm.live_mut(&id).unwrap();
                live.meta.poll();
                !live.meta.is_loading()
            },
            "metadata",
        );
        let live = cm.live(&id).unwrap();
        let tables = live.meta.objects.get(&(schema.clone(), dbiji_core::meta::ObjectKind::Table)).and_then(|l| l.get()).cloned().unwrap_or_default();
        assert!(tables.iter().any(|t| t.obj.name == table), "{name}: {table} listed in {schema:?}");

        let events = run_script(&backend, &cm, &id, &[&format!("DROP TABLE {table}")]);
        assert!(errors(&events).is_empty(), "{name}: drop {:?}", errors(&events));
        cm.disconnect_all(&backend);
    }
}

#[test]
fn errors_stop_the_script_and_cancel_works() {
    for (name, url) in servers() {
        let backend = Backend::new().unwrap();
        let (cm, id) = connect(&backend, &url);
        let events = run_script(&backend, &cm, &id, &["SELECT 1", "SELEC broken", "SELECT 2"]);
        let errs = errors(&events);
        assert_eq!(errs.len(), 1, "{name}: one error");
        let done = events.iter().find_map(|e| if let ExecEvent::Done { executed, failed, .. } = e { Some((*executed, *failed)) } else { None });
        assert_eq!(done, Some((2, 1)), "{name}: stops on the first error");

        // A long query is cancelled through the session.
        let sleep = if name == "postgres" { "SELECT pg_sleep(20)" } else { "SELECT SLEEP(20)" };
        let session = cm.live(&id).unwrap().main.clone();
        let plan = StmtPlan { index: 0, sql: sleep.into(), original: sleep.into(), range: 0..sleep.len(), is_write: false, client: None };
        let mut exec = Execution::start(&backend, session, vec![plan], ExecOpts::default(), None);
        std::thread::sleep(Duration::from_millis(500));
        let t0 = Instant::now();
        exec.cancel(&backend);
        wait_until(
            || {
                exec.poll();
                exec.is_finished()
            },
            "cancel",
        );
        assert!(t0.elapsed() < Duration::from_secs(10), "{name}: cancel took {:?}", t0.elapsed());
        // The session is usable afterwards.
        let events = run_script(&backend, &cm, &id, &["SELECT 3"]);
        assert!(errors(&events).is_empty(), "{name}: after cancel {:?}", errors(&events));
    }
}

fn set_schema(backend: &Backend, session: &dbiji_app::conn::session::SessionHandle, schema: &str) {
    let s = session.clone();
    let schema = schema.to_string();
    let mut job = backend.spawn(async move { s.lock().await.set_current_schema(&schema).await });
    let mut out = None;
    wait_until(
        || {
            out = job.poll();
            out.is_some()
        },
        "set schema",
    );
    out.unwrap().expect("set current schema");
}

/// PRD gate: schema compare produces DDL that, executed on the target, makes
/// both schemas identical.
#[test]
fn schema_compare_gate() {
    use dbiji_compare::{CompareOptions, ScriptOptions, migration_script, schema_diff};
    use dbiji_core::ddl::SnapshotOptions;
    use dbiji_core::meta::SchemaRef;
    for (name, url) in servers() {
        let backend = Backend::new().unwrap();
        let (cm, id) = connect(&backend, &url);
        let session = cm.live(&id).unwrap().main.clone();
        let sfx = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let is_pg = name == "postgres";
        let (a, b) = (format!("dbiji_cmp_a_{sfx}"), format!("dbiji_cmp_b_{sfx}"));
        let create = if is_pg { "CREATE SCHEMA" } else { "CREATE DATABASE" };
        let drop = if is_pg { "DROP SCHEMA" } else { "DROP DATABASE" };
        let cascade = if is_pg { " CASCADE" } else { "" };
        let dt = if is_pg { "TIMESTAMP" } else { "DATETIME" };
        let source = [
            format!("{create} {a}"),
            format!("{create} {b}"),
            format!("CREATE TABLE {a}.customer (id INT PRIMARY KEY, name VARCHAR(80) NOT NULL, email VARCHAR(120), created {dt})"),
            format!("CREATE INDEX ix_customer_name ON {a}.customer (name)"),
            format!("CREATE TABLE {a}.orders (id INT PRIMARY KEY, customer_id INT NOT NULL, total DECIMAL(12,2) DEFAULT 0, CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES {a}.customer (id))"),
            format!("CREATE VIEW {a}.big_orders AS SELECT id, total FROM {a}.orders WHERE total > 100"),
            format!("CREATE TABLE {b}.customer (id INT PRIMARY KEY, name VARCHAR(20), legacy_code CHAR(3))"),
            format!("CREATE TABLE {b}.obsolete (x INT)"),
        ];
        let refs: Vec<&str> = source.iter().map(String::as_str).collect();
        let events = run_script(&backend, &cm, &id, &refs);
        assert!(errors(&events).is_empty(), "{name}: setup {:?}", errors(&events));
        let snap = |schema: &str| {
            let s = session.clone();
            let schema = SchemaRef::new(None, Some(schema.to_string()));
            let job = backend.spawn(async move { s.lock().await.schema_snapshot(&schema, &SnapshotOptions::full()).await });
            let mut job = job;
            let mut out = None;
            wait_until(
                || {
                    out = job.poll();
                    out.is_some()
                },
                "snapshot",
            );
            out.unwrap().expect("snapshot")
        };
        let sa = snap(&a);
        let sb = snap(&b);
        let diff = schema_diff(&sa, &sb, &CompareOptions::default());
        assert!(diff.has_differences(), "{name}: schemas differ before the migration");
        let script = migration_script(&diff, &sa, &sb, session.dialect().as_ref(), &ScriptOptions::full_sync());
        // Like the compare tab: run the migration with the target schema active,
        // so unqualified names in view bodies and foreign keys resolve there.
        let previous = cm.live(&id).unwrap().current_schema.clone().or(cm.live(&id).unwrap().current_database.clone());
        set_schema(&backend, &session, &b);
        let refs: Vec<&str> = script.iter().map(String::as_str).collect();
        let events = run_script(&backend, &cm, &id, &refs);
        if let Some(p) = &previous {
            set_schema(&backend, &session, p);
        }
        let errs = errors(&events);
        let sb2 = snap(&b);
        let after = schema_diff(&sa, &sb2, &CompareOptions::default());
        let remaining: Vec<String> = after
            .changed_items()
            .map(|i| format!("{:?} {} {:?} {:?}", i.kind, i.qualified_name(), i.status, i.changes.iter().map(|c| (&c.property, &c.source, &c.target)).collect::<Vec<_>>()))
            .collect();
        // Clean up before asserting.
        let cleanup = [format!("{drop} {a}{cascade}"), format!("{drop} {b}{cascade}")];
        let refs: Vec<&str> = cleanup.iter().map(String::as_str).collect();
        run_script(&backend, &cm, &id, &refs);
        assert!(errs.is_empty(), "{name}: migration failed {errs:?}\nscript:\n{}", script.join(";\n"));
        assert!(remaining.is_empty(), "{name}: still different after the migration: {remaining:#?}\nscript:\n{}", script.join(";\n"));
    }
}
