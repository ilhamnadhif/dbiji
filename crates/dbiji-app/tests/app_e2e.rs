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
