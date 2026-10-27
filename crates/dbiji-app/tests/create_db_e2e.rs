//! "New database…" / "New schema…" statements run on real servers
//! (DBIJI_TEST_PG, DBIJI_TEST_MYSQL, DBIJI_TEST_MARIADB); the objects are
//! dropped afterwards.

use dbiji_core::driver::{ConnectParams, Session, TransactionMode, execute_update, query_all};
use dbiji_core::{DatabaseProps, NewDatabase, Secret, Value};

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

/// Edit database…: its properties as read, then a rename and the other changes
/// the server allows, read back.
#[tokio::test]
async fn edit_database() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    for var in ["DBIJI_TEST_PG", "DBIJI_TEST_COCKROACH", "DBIJI_TEST_MYSQL", "DBIJI_TEST_MARIADB"] {
        let Some(url) = std::env::var(var).ok().filter(|u| !u.trim().is_empty()) else { continue };
        let (mut s, d) = connect(&url).await;
        let name = format!("dbiji_editdb_{suffix}");
        let mysql = matches!(var, "DBIJI_TEST_MYSQL" | "DBIJI_TEST_MARIADB");
        let pg = var == "DBIJI_TEST_PG";
        let create = d.create_database_sql(&NewDatabase { name: name.clone(), charset: mysql.then(|| "utf8mb4".into()), ..Default::default() }).unwrap();
        execute_update(s.as_mut(), &create, &[]).await.unwrap_or_else(|e| panic!("[{var}] {create}: {}", e.full_text()));
        let sql = d.database_properties_sql(&name).expect("properties are read");
        let (_, rows) = query_all(s.as_mut(), &sql, &[]).await.unwrap_or_else(|e| panic!("[{var}] {sql}: {}", e.full_text()));
        let old = DatabaseProps::from_row(&name, &rows[0]);
        if mysql {
            assert_eq!(old.encoding.as_deref(), Some("utf8mb4"), "[{var}] {old:?}");
            assert!(old.collation.is_some(), "[{var}] {old:?}");
        } else {
            assert!(old.owner.is_some() && old.encoding.is_some(), "[{var}] {old:?}");
        }
        if pg {
            assert_eq!((old.connection_limit, old.allow_connections, old.is_template), (Some(-1), Some(true), Some(false)), "{old:?}");
            assert!(old.size_bytes.is_some_and(|b| b > 0), "{old:?}");
        }
        let mut new = old.clone();
        let renamed = format!("dbiji_renamed_{suffix}");
        if d.database_edits().name {
            new.name = renamed.clone();
        }
        if d.database_edits().comment {
            new.comment = Some("edited by DBiji".into());
        }
        if pg {
            new.connection_limit = Some(5);
            new.is_template = Some(true);
        }
        if mysql {
            new.collation = Some("utf8mb4_bin".into());
        }
        let statements = d.alter_database_sql(&old, &new);
        assert!(!statements.is_empty(), "[{var}]");
        for stmt in &statements {
            execute_update(s.as_mut(), stmt, &[]).await.unwrap_or_else(|e| panic!("[{var}] {stmt}: {}", e.full_text()));
        }
        let n = new.name.clone();
        let sql = d.database_properties_sql(&n).unwrap();
        let (_, rows) = query_all(s.as_mut(), &sql, &[]).await.unwrap();
        let after = DatabaseProps::from_row(&n, &rows.first().unwrap_or_else(|| panic!("[{var}] {n} after {statements:?}")).clone());
        if d.database_edits().comment {
            assert_eq!(after.comment.as_deref(), Some("edited by DBiji"), "[{var}]");
        }
        if pg {
            assert_eq!((after.connection_limit, after.is_template), (Some(5), Some(true)));
            // A template is dropped only once it is none.
            execute_update(s.as_mut(), &format!("ALTER DATABASE {} WITH IS_TEMPLATE false", d.quote_ident(&n)), &[]).await.unwrap();
        }
        if mysql {
            assert_eq!(after.collation.as_deref(), Some("utf8mb4_bin"));
        }
        execute_update(s.as_mut(), &format!("DROP DATABASE {}", d.quote_ident(&n)), &[]).await.unwrap();
        s.close().await.ok();
    }
}
