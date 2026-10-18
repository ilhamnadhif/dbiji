//! PRD phase 3 gate: backup and restore through the vendor tools, exactly as
//! the backup screen and backup tasks run them (driver command line +
//! `tools::run_command`). Runs for PostgreSQL when `DBIJI_TEST_PG` is set and
//! `pg_dump` / `pg_restore` are installed, for MySQL / MariaDB when
//! `DBIJI_TEST_MYSQL` / `DBIJI_TEST_MARIADB` are set and `mysqldump` + `mysql`
//! (or the MariaDB equivalents) are installed.

use dbiji_app::backend::Backend;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_core::admin::{BackupFormat, BackupOptions, RestoreOptions};
use dbiji_core::config::{SecretSlot, secret_key};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn connect(backend: &Backend, url: &str) -> (ConnectionManager, String) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (mut cfg, password) = dbiji_config::parse_connection_url(url).unwrap();
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
        assert!(t0.elapsed() < Duration::from_secs(20));
        cm.poll(backend);
        std::thread::sleep(Duration::from_millis(10));
    }
    (cm, id)
}

fn run(backend: &Backend, cm: &ConnectionManager, id: &str, sql: &str) -> Vec<Vec<String>> {
    let s = cm.live(id).unwrap().main.clone();
    let owned = sql.to_string();
    let (_, rows) = backend.block_on(async move { s.query_all(&owned, &[]).await }).unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.iter().map(|r| r.iter().map(|v| v.to_text().into_owned()).collect()).collect()
}

fn exec(backend: &Backend, cm: &ConnectionManager, id: &str, sql: &str) {
    let s = cm.live(id).unwrap().main.clone();
    let owned = sql.to_string();
    backend.block_on(async move { s.execute_update(&owned, &[]).await }).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn round_trip(name: &str, url: &str, format: BackupFormat) {
    let backend = Backend::new().unwrap();
    let (cm, id) = connect(&backend, url);
    let live = cm.live(&id).unwrap();
    let table = format!("dbiji_bk_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    exec(&backend, &cm, &id, &format!("CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(30))"));
    exec(&backend, &cm, &id, &format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b'), (3, 'c')"));
    let dir = std::env::temp_dir().join(format!("dbiji-backup-e2e-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(if format == BackupFormat::Plain { "dump.sql" } else { "dump.backup" });
    let mut opts = BackupOptions { output: file.clone(), format, tables: vec![table.clone()], ..Default::default() };
    if name == "postgres" {
        opts.no_owner = true;
    } else {
        opts.single_transaction = true;
    }
    let spec = live.driver.backup_command(live.params(), &opts).expect("backup command");
    assert!(!spec.display().contains("dbiji@"), "{name}: password must not be on the command line: {}", spec.display());
    let mut log = Vec::new();
    backend
        .block_on(dbiji_app::tools::run_command(spec, CancellationToken::new(), |l| log.push(l)))
        .unwrap_or_else(|e| panic!("{name}: backup failed: {}\n{}", e.full_text(), log.join("\n")));
    assert!(std::fs::metadata(&file).map(|m| m.len() > 0).unwrap_or(false), "{name}: dump written");
    exec(&backend, &cm, &id, &format!("DROP TABLE {table}"));
    // Newer client tools than the server (pg_dump 17 against PostgreSQL 16)
    // write settings the server does not know (`SET transaction_timeout`);
    // the restore continues past those, like the backup screen's default.
    let ropts = RestoreOptions { input: file.clone(), format, no_owner: name == "postgres", exit_on_error: false, ..Default::default() };
    let spec = live.driver.restore_command(live.params(), &ropts).expect("restore command");
    let mut log = Vec::new();
    if let Err(e) = backend.block_on(dbiji_app::tools::run_command(spec, CancellationToken::new(), |l| log.push(l))) {
        let text = format!("{}\n{}", e.full_text(), log.join("\n"));
        let tolerated = text.lines().filter(|l| l.contains("ERROR")).all(|l| l.contains("transaction_timeout"));
        assert!(tolerated, "{name}: restore failed: {text}");
    }
    let rows = run(&backend, &cm, &id, &format!("SELECT id, name FROM {table} ORDER BY id"));
    exec(&backend, &cm, &id, &format!("DROP TABLE {table}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rows.len(), 3, "{name}: restored rows");
    assert_eq!(rows[2], vec!["3".to_string(), "c".to_string()]);
}

#[test]
fn postgres_backup_restore() {
    let Ok(url) = std::env::var("DBIJI_TEST_PG") else { return };
    if !on_path("pg_dump") || !on_path("pg_restore") || !on_path("psql") {
        eprintln!("pg_dump / pg_restore / psql not installed: skipped");
        return;
    }
    round_trip("postgres", &url, BackupFormat::Custom);
    round_trip("postgres", &url, BackupFormat::Plain);
}

#[test]
fn mysql_backup_restore() {
    for (name, var, tools) in [("mysql", "DBIJI_TEST_MYSQL", ["mysqldump", "mysql"]), ("mariadb", "DBIJI_TEST_MARIADB", ["mariadb-dump", "mariadb"])] {
        let Ok(url) = std::env::var(var) else { continue };
        if !tools.iter().all(|t| on_path(t)) && !(on_path("mysqldump") && on_path("mysql")) {
            eprintln!("{name}: client tools not installed: skipped");
            continue;
        }
        round_trip(name, &url, BackupFormat::Plain);
    }
}
