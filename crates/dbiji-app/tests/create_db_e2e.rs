//! "New database…" / "New schema…" statements run on real servers
//! (DBIJI_TEST_PG, DBIJI_TEST_MYSQL, DBIJI_TEST_MARIADB); the objects are
//! dropped afterwards.

use dbiji_core::driver::{ConnectParams, Session, TransactionMode, execute_update, query_all};
use dbiji_core::{NewDatabase, Secret, Value};

async fn connect(url: &str) -> (Box<dyn Session>, std::sync::Arc<dyn dbiji_core::Dialect>) {
    let (cfg, password) = dbiji_config::parse_connection_url(url).expect("valid test URL");
    let registry = dbiji_app::driver_registry();
    let driver = registry.get(cfg.kind).unwrap();
    let password = password.map(|p| Secret::new(p.expose().to_string()));
    let mut s = driver.connect(&ConnectParams::from_config(&cfg, password)).await.unwrap();
    // Production-like: manual commit, as the dialog must cope with it.
    let _ = s.set_transaction_mode(TransactionMode::Manual).await;
    s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    (s, registry.dialect(cfg.kind))
}

async fn exists(s: &mut Box<dyn Session>, sql: &str) -> bool {
    let (_, rows) = query_all(s.as_mut(), sql, &[]).await.unwrap();
    rows.first().and_then(|r| r.first()).is_some_and(|v| !v.is_null() && *v != Value::Int(0))
}

#[tokio::test]
async fn new_database_and_schema() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    for var in ["DBIJI_TEST_PG", "DBIJI_TEST_MYSQL", "DBIJI_TEST_MARIADB"] {
        let Some(url) = std::env::var(var).ok().filter(|u| !u.trim().is_empty()) else { continue };
        let (mut s, d) = connect(&url).await;
        let name = format!("dbiji_newdb_{suffix}");
        let mysql = var != "DBIJI_TEST_PG";
        let db = NewDatabase {
            name: name.clone(),
            encoding: (!mysql).then(|| "UTF8".into()),
            template: (!mysql).then(|| "template0".into()),
            charset: mysql.then(|| "utf8mb4".into()),
            collation: mysql.then(|| "utf8mb4_unicode_ci".into()),
            ..Default::default()
        };
        let sql = d.create_database_sql(&db).expect("databases can be created");
        execute_update(s.as_mut(), &sql, &[]).await.unwrap_or_else(|e| panic!("[{var}] {sql}: {}", e.full_text()));
        let check = if mysql {
            format!("SELECT count(*) FROM information_schema.schemata WHERE schema_name = '{name}'")
        } else {
            format!("SELECT count(*) FROM pg_database WHERE datname = '{name}'")
        };
        assert!(exists(&mut s, &check).await, "[{var}] {name} created");
        if !mysql {
            let schema_sql = d.create_schema_sql(&format!("reports_{suffix}"), None).unwrap();
            execute_update(s.as_mut(), &schema_sql, &[]).await.unwrap();
            assert!(exists(&mut s, &format!("SELECT count(*) FROM information_schema.schemata WHERE schema_name = 'reports_{suffix}'")).await);
            execute_update(s.as_mut(), &format!("DROP SCHEMA reports_{suffix}"), &[]).await.unwrap();
        } else {
            assert!(d.create_schema_sql("x", None).is_none(), "MySQL schemas are databases");
        }
        execute_update(s.as_mut(), &format!("DROP DATABASE {}", d.quote_ident(&name)), &[]).await.unwrap();
        s.close().await.ok();
    }
}
