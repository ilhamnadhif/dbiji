//! PRD gate: "import connections from DBeaver without losing main fields".
//! Synthetic DBeaver workspace -> import -> merge into the store -> secrets in
//! a secret store -> save -> reload, using only the public API.

use dbiji_config::import::dbeaver::{encrypt_credentials, import_workspace};
use dbiji_config::import::{DuplicatePolicy, merge_into_store};
use dbiji_config::{AppPaths, ConnectionStore, MemoryStore, SecretBackend, SecretsManager};
use dbiji_core::DbKind;
use dbiji_core::config::{SecretSlot, SshAuth, SslMode};
use std::sync::Arc;

const DATA_SOURCES: &str = r#"{
  "folders": {"Production": {}, "Production/Payments": {"parent": "Production"}},
  "connection-types": {"prod": {"name": "Production", "color": "247,159,129", "auto-commit": false, "confirm-execute": true, "confirm-data-change": true}},
  "connections": {
    "postgres-jdbc-18a9f": {
      "provider": "postgresql", "driver": "postgres-jdbc", "name": "Payments primary",
      "save-password": true, "folder": "Production/Payments", "read-only": true,
      "configuration": {
        "host": "payments-db.internal", "port": "5432", "database": "payments",
        "type": "prod", "auth-model": "native",
        "bootstrap": {"autocommit": false, "defaultSchema": "ledger"},
        "properties": {"sslmode": "verify-full", "sslrootcert": "/etc/ssl/corp-ca.pem"},
        "handlers": {"ssh_tunnel": {"enabled": true, "save-password": true,
          "properties": {"host": "bastion.corp", "port": "22", "authType": "PASSWORD"}}}
      }
    },
    "mysql8-77": {
      "provider": "mysql", "driver": "mysql8", "name": "Shop",
      "save-password": true,
      "configuration": {"host": "shop-mysql", "port": "3306", "database": "shop", "type": "dev"}
    }
  }
}"#;

const CREDENTIALS: &str = r##"{
  "postgres-jdbc-18a9f": {"#connection": {"user": "payments_rw", "password": "pg-pass-123"},
                          "network/ssh_tunnel": {"user": "ops", "password": "ssh-pass-456"}},
  "mysql8-77": {"#connection": {"user": "shop_app", "password": "my-pass-789"}}
}"##;

#[test]
fn dbeaver_import_keeps_main_fields() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("DBeaverData").join("workspace6");
    let meta = ws.join("General").join(".dbeaver");
    std::fs::create_dir_all(&meta).unwrap();
    std::fs::write(meta.join("data-sources.json"), DATA_SOURCES).unwrap();
    std::fs::write(
        meta.join("credentials-config.json"),
        encrypt_credentials(CREDENTIALS.as_bytes()).unwrap(),
    )
    .unwrap();

    let result = import_workspace(&ws).unwrap();
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);

    let paths = AppPaths::with_root(dir.path().join("dbiji"));
    paths.ensure_dirs().unwrap();
    let mut store = ConnectionStore::load(&paths).unwrap();
    let summary = merge_into_store(&mut store, result, DuplicatePolicy::Skip);
    assert_eq!(summary.added, 2);

    let memory = Arc::new(MemoryStore::new());
    let secrets = SecretsManager::with_store(SecretBackend::Keychain, memory.clone());
    secrets.store_all(&summary.secrets).unwrap();
    store.save(&paths).unwrap();

    // Nothing secret on disk.
    let on_disk = std::fs::read_to_string(paths.connections_file()).unwrap();
    for s in ["pg-pass-123", "ssh-pass-456", "my-pass-789"] {
        assert!(!on_disk.contains(s), "{s} leaked into connections.json");
    }

    let store = ConnectionStore::load(&paths).unwrap();
    let pg = store.get("postgres-jdbc-18a9f").expect("pg imported");
    assert_eq!(pg.kind, DbKind::Postgres);
    assert_eq!(pg.name, "Payments primary");
    assert_eq!(pg.host, "payments-db.internal");
    assert_eq!(pg.port, Some(5432));
    assert_eq!(pg.database.as_deref(), Some("payments"));
    assert_eq!(pg.user.as_deref(), Some("payments_rw"));
    assert_eq!(pg.folder.as_deref(), Some("Production/Payments"));
    assert_eq!(pg.connection_type, "prod");
    assert!(store.connection_type("prod").is_production());
    assert!(pg.read_only);
    assert_eq!(pg.auto_commit, Some(false));
    assert_eq!(pg.init.default_schema.as_deref(), Some("ledger"));
    assert_eq!(pg.ssl.mode, SslMode::VerifyFull);
    assert_eq!(pg.ssl.ca_cert.as_deref(), Some("/etc/ssl/corp-ca.pem"));
    let ssh = store.effective_ssh(pg).expect("ssh tunnel");
    assert_eq!(
        (ssh.host.as_str(), ssh.port, ssh.user.as_str()),
        ("bastion.corp", 22, "ops")
    );
    assert_eq!(ssh.auth, SshAuth::Password);
    assert!(store.folders.contains(&"Production/Payments".to_string()));
    assert!(
        dbiji_config::validate(pg).is_empty(),
        "{:?}",
        dbiji_config::validate(pg)
    );

    assert_eq!(
        secrets
            .connection_secret(&pg.id, SecretSlot::Password)
            .unwrap()
            .unwrap()
            .expose(),
        "pg-pass-123"
    );
    assert_eq!(
        secrets
            .connection_secret(&pg.id, SecretSlot::SshPassword)
            .unwrap()
            .unwrap()
            .expose(),
        "ssh-pass-456"
    );

    let my = store.get("mysql8-77").expect("mysql imported");
    assert_eq!(
        (my.kind, my.host.as_str(), my.database.as_deref()),
        (DbKind::MySql, "shop-mysql", Some("shop"))
    );
    assert_eq!(
        secrets
            .connection_secret(&my.id, SecretSlot::Password)
            .unwrap()
            .unwrap()
            .expose(),
        "my-pass-789"
    );

    // Importing the same workspace again does not duplicate anything.
    let again = import_workspace(&ws).unwrap();
    let mut store = store;
    let summary = merge_into_store(&mut store, again, DuplicatePolicy::Skip);
    assert_eq!((summary.added, summary.skipped), (0, 2));
    assert_eq!(store.connections.len(), 2);
}
