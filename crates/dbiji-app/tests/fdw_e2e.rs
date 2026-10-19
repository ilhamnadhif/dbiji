//! The foreign data wrapper script of the configurator runs on PostgreSQL:
//! a throwaway database imports a table of the test database through
//! postgres_fdw (the server connects to itself). Needs DBIJI_TEST_PG.

use dbiji_app::tabs::dbtools::{fdw_script, spec_for};
use dbiji_core::driver::{ConnectParams, Session, execute_update, query_all};
use dbiji_core::{DbKind, Secret, Value};

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    execute_update(s.as_mut(), sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
}

#[tokio::test]
async fn postgres_fdw_script_imports_a_schema() {
    let Some(url) = std::env::var("DBIJI_TEST_PG").ok().filter(|u| !u.trim().is_empty()) else { return };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (cfg, password) = dbiji_config::parse_connection_url(&url).expect("valid test URL");
    let password = password.map(|p| p.expose().to_string()).unwrap_or_default();
    let driver = dbiji_app::driver_registry().get(DbKind::Postgres).unwrap();
    let params = ConnectParams::from_config(&cfg, Some(Secret::new(password.clone())));
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..10];
    let (schema, db) = (format!("fdw_src_{suffix}"), format!("dbiji_fdw_{suffix}"));

    let mut main = driver.connect(&params).await.unwrap();
    exec(&mut main, &format!("CREATE SCHEMA {schema}")).await;
    exec(&mut main, &format!("CREATE TABLE {schema}.items (id int PRIMARY KEY, v text)")).await;
    exec(&mut main, &format!("INSERT INTO {schema}.items VALUES (1, 'one'), (2, 'it''s two')")).await;
    exec(&mut main, &format!("CREATE DATABASE {db}")).await;

    let result = async {
        let mut spec = spec_for(&cfg);
        // the database server reaches itself on its own port
        spec.host = "127.0.0.1".into();
        spec.port = Some(5432);
        spec.remote_schema = schema.clone();
        spec.local_schema = "imported".into();
        let script = fdw_script(&spec).unwrap();
        assert!(script.contains("password :password"), "the password is a parameter: {script}");
        let script = script.replace(":password", &format!("'{}'", password.replace('\'', "''")));
        let mut target = driver.connect(&ConnectParams { database: Some(db.clone()), ..params.clone() }).await.unwrap();
        for stmt in script.split(";\n").map(str::trim).filter(|s| s.lines().any(|l| !l.trim().is_empty() && !l.trim_start().starts_with("--"))) {
            exec(&mut target, stmt).await;
        }
        let (_, rows) = query_all(target.as_mut(), "SELECT v FROM imported.items ORDER BY id", &[]).await.unwrap();
        target.close().await.ok();
        rows
    }
    .await;

    exec(&mut main, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
    exec(&mut main, &format!("DROP SCHEMA {schema} CASCADE")).await;
    main.close().await.ok();
    assert_eq!(result, vec![vec![Value::text("one")], vec![Value::text("it's two")]]);
}
