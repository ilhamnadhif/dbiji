//! Every catalog folder of every dialect runs against a real server: the
//! list query, the child folders of the first object and its DDL. SQLite is
//! always tested; the other servers with the DBIJI_TEST_* variables.

use dbiji_app::backend::Backend;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_core::catalog::{self, CatalogContext, CatalogScope};
use dbiji_core::config::{SecretSlot, secret_key};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn servers() -> Vec<(&'static str, String)> {
    let file = std::env::temp_dir().join(format!("dbiji-catalog-{}.db", uuid::Uuid::new_v4().simple()));
    let mut out = vec![("sqlite", format!("sqlite://{}", file.display()))];
    for (name, var) in [
        ("postgres", "DBIJI_TEST_PG"),
        ("cockroach", "DBIJI_TEST_COCKROACH"),
        ("mysql", "DBIJI_TEST_MYSQL"),
        ("mariadb", "DBIJI_TEST_MARIADB"),
        ("clickhouse", "DBIJI_TEST_CLICKHOUSE"),
        ("mssql", "DBIJI_TEST_MSSQL"),
    ] {
        if let Ok(url) = std::env::var(var)
            && !url.trim().is_empty()
        {
            // DBIJI_TEST_CLICKHOUSE is the HTTP endpoint.
            let url = if name == "clickhouse" { url.replacen("http://", "clickhouse://", 1) } else { url };
            out.push((name, url));
        }
    }
    out
}

fn connect(backend: &Backend, url: &str) -> (ConnectionManager, String) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (mut cfg, password) = dbiji_config::parse_connection_url(url).expect("valid test URL");
    cfg.save_password = true;
    if password.is_none() {
        cfg.auth = dbiji_core::config::AuthMethod::None;
    }
    let secrets = Arc::new(MemorySecrets::default());
    if let Some(pw) = &password {
        secrets.set(&secret_key(&cfg.id, SecretSlot::Password), pw).unwrap();
    }
    let mut cm = ConnectionManager::new(dbiji_app::driver_registry(), secrets, Arc::new(DirectNet));
    let id = cfg.id.clone();
    cm.upsert(cfg);
    cm.connect(backend, &id, Credentials::default()).unwrap();
    let t0 = Instant::now();
    while !cm.is_connected(&id) {
        assert!(cm.errors().is_empty(), "{:?}", cm.errors());
        assert!(t0.elapsed() < Duration::from_secs(20), "connect timed out");
        cm.poll(backend);
        std::thread::sleep(Duration::from_millis(10));
    }
    (cm, id)
}

#[test]
fn every_catalog_folder_runs() {
    let backend = Backend::new().unwrap();
    for (name, url) in servers() {
        let (cm, id) = connect(&backend, &url);
        let live = cm.live(&id).unwrap();
        let session = live.main.clone();
        let folders = live.dialect.catalog_folders();
        assert!(!folders.is_empty(), "[{name}] has catalog folders");
        let schema = live.current_schema.clone();
        let table = format!("dbiji_cat_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let s = session.clone();
        let create = format!("CREATE TABLE {table} (id INT PRIMARY KEY)");
        let create = if name == "clickhouse" { format!("{create} ENGINE = MergeTree ORDER BY id") } else { create };
        backend.block_on(async move { s.execute_update(&create, &[]).await }).unwrap();
        let mut checked = 0;
        for f in folders.iter().filter(|f| f.scope != CatalogScope::Child) {
            let ctx = CatalogContext {
                database: live.current_database.clone(),
                schema: if f.scope == CatalogScope::Table { schema.clone() } else { None },
                table: (f.scope == CatalogScope::Table).then(|| table.clone()),
                ..Default::default()
            };
            let s = session.clone();
            let (folder, c) = (f.clone(), ctx.clone());
            let rows =
                backend.block_on(async move { catalog::list(s.lock().await.as_mut(), &folder, &c).await }).unwrap_or_else(|e| panic!("[{name}] {}: {}", f.id, e.full_text()));
            checked += 1;
            let Some(first) = rows.first() else { continue };
            assert!(!first.name.is_empty(), "[{name}] {}: object name", f.id);
            let item = CatalogContext { name: Some(first.name.clone()), ..ctx.clone() };
            if !f.ddl_sql.is_empty() {
                let s = session.clone();
                let (folder, c) = (f.clone(), item.clone());
                let ddl = backend
                    .block_on(async move { catalog::ddl(s.lock().await.as_mut(), &folder, &c).await })
                    .unwrap_or_else(|e| panic!("[{name}] {} DDL of {}: {}", f.id, first.name, e.full_text()));
                assert!(ddl.is_some_and(|d| !d.trim().is_empty()), "[{name}] {} DDL of {}", f.id, first.name);
            }
            for child in &f.children {
                let cf = folders.iter().find(|x| x.id == *child).unwrap_or_else(|| panic!("[{name}] child folder {child}"));
                let s = session.clone();
                let c = CatalogContext { parent: Some(first.name.clone()), ..ctx.clone() };
                let cf2 = cf.clone();
                backend
                    .block_on(async move { catalog::list(s.lock().await.as_mut(), &cf2, &c).await })
                    .unwrap_or_else(|e| panic!("[{name}] {child} of {}: {}", first.name, e.full_text()));
                checked += 1;
            }
        }
        let s = session.clone();
        let drop = format!("DROP TABLE {table}");
        backend.block_on(async move { s.execute_update(&drop, &[]).await }).unwrap();
        eprintln!("[{name}] {checked} catalog folders");
        if let Some(p) = url.strip_prefix("sqlite://") {
            let _ = std::fs::remove_file(p);
        }
    }
}
