//! Connection, TLS, authentication, properties and error mapping against
//! real servers (`DBIJI_TEST_MYSQL`, `DBIJI_TEST_MARIADB`).

mod common;

use common::*;
use dbiji_core::config::SslMode;
use dbiji_core::{DbKind, Driver, ErrorKind, Session, Value};
use dbiji_driver_mysql::{MariaDbDriver, MySqlDriver};

/// Mutual TLS: a user created with `REQUIRE X509` can only log in when a
/// client certificate signed by the server CA is presented
/// (`DBIJI_TEST_MYSQL_CLIENT_CERT` / `DBIJI_TEST_MYSQL_CLIENT_KEY`, PEM).
#[tokio::test]
async fn tls_client_certificate() {
    let (Ok(cert), Ok(key)) = (
        std::env::var("DBIJI_TEST_MYSQL_CLIENT_CERT"),
        std::env::var("DBIJI_TEST_MYSQL_CLIENT_KEY"),
    ) else {
        return;
    };
    for srv in servers().into_iter().filter(|s| s.kind == DbKind::MySql) {
        let user = format!("dbiji_x509_{}", suffix());
        let mut admin = srv.connect().await;
        exec(
            &mut admin,
            &format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'Pw-x509-1' REQUIRE X509"),
        )
        .await;
        let mut p = srv.with_ssl(SslMode::Require);
        p.user = Some(user.clone());
        p.password = Some("Pw-x509-1".into());
        p.database = None;
        let without = srv.try_connect(&p).await;
        p.ssl.client_cert = Some(cert.clone());
        p.ssl.client_key = Some(key.clone());
        let with = srv.try_connect(&p).await;
        exec(&mut admin, &format!("DROP USER '{user}'@'%'")).await;
        admin.close().await.unwrap();
        let e = without
            .err()
            .expect("REQUIRE X509 without a client certificate must fail");
        assert_eq!(e.kind, ErrorKind::Auth, "{}", e.full_text());
        let mut s = with.unwrap_or_else(|e| panic!("mutual TLS login failed: {}", e.full_text()));
        assert!(s.is_tls());
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn connect_plain_and_server_info() {
    for srv in servers() {
        let mut s = srv.connect().await;
        let info = s.server_info().clone();
        assert_eq!(s.kind(), srv.kind, "[{}] detected kind", srv.name);
        assert!(
            info.version_number.0 >= 5,
            "[{}] version {:?}",
            srv.name,
            info.version_number
        );
        let id = info.session_id.clone().expect("session id");
        let rows = query(&mut s, "SELECT CONNECTION_ID()").await;
        assert_eq!(
            rows[0][0].as_i64().map(|v| v.to_string()),
            Some(id),
            "[{}]",
            srv.name
        );
        if srv.is_mariadb() {
            assert_eq!(info.product, "MariaDB");
            assert!(info.version.contains("MariaDB"));
        } else {
            assert!(info.product == "MySQL" || info.product == "Percona Server");
        }
        assert!(!s.is_tls(), "[{}] Disable must not use TLS", srv.name);
        s.ping().await.unwrap();
        assert!(!s.is_closed());
        // SET NAMES utf8mb4 by default
        let rows = query(
            &mut s,
            "SELECT @@character_set_client, @@character_set_results",
        )
        .await;
        assert_eq!(rows[0][0], Value::text("utf8mb4"));
        assert_eq!(rows[0][1], Value::text("utf8mb4"));
        let cancel = s.cancel_handle().expect("cancel handle");
        cancel.cancel().await.unwrap(); // nothing running: harmless
        s.ping().await.unwrap();
        s.close().await.unwrap();
        assert!(s.is_closed());
        let e = s.ping().await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Connection);
    }
}

#[tokio::test]
async fn driver_trait_connect() {
    for srv in servers() {
        let driver: Box<dyn Driver> = match srv.kind {
            DbKind::MariaDb => Box::new(MariaDbDriver::new()),
            _ => Box::new(MySqlDriver::new()),
        };
        let mut s = driver.connect(&srv.params).await.unwrap();
        assert_eq!(s.kind(), srv.kind);
        assert_eq!(s.dialect().kind(), srv.kind);
        let v = dbiji_core::driver::query_scalar(s.as_mut(), "SELECT 40 + 2", &[])
            .await
            .unwrap();
        assert_eq!(v, Value::Int(42));
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn tls_require_and_prefer() {
    for srv in servers() {
        for mode in [SslMode::Require, SslMode::Prefer] {
            let p = srv.with_ssl(mode);
            let mut s = srv.connect_with(&p).await;
            assert!(s.is_tls(), "[{}] {mode:?} should use TLS", srv.name);
            let rows = query(&mut s, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
            let cipher = rows
                .first()
                .map(|r| r[1].to_text().into_owned())
                .unwrap_or_default();
            assert!(!cipher.is_empty(), "[{}] {mode:?}: no TLS cipher", srv.name);
            s.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn tls_verification_failures() {
    for srv in servers() {
        // The test servers use self-signed / private CA certificates: without
        // the CA, chain verification must fail.
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let p = srv.with_ssl(mode);
            match srv.try_connect(&p).await {
                Ok(_) => panic!("[{}] {mode:?} must fail without the server CA", srv.name),
                Err(e) => assert_eq!(
                    e.kind,
                    ErrorKind::Tls,
                    "[{}] {mode:?}: {}",
                    srv.name,
                    e.full_text()
                ),
            }
        }
        // Custom CA only, missing file -> configuration error
        let mut p = srv.with_ssl(SslMode::VerifyCa);
        p.ssl.ca_cert = Some("/nonexistent/ca.pem".into());
        let e = srv.try_connect(&p).await.err().expect("missing CA file");
        assert_eq!(e.kind, ErrorKind::Config);
    }
}

/// Positive VerifyCa / VerifyFull checks with the server CA certificate
/// (`DBIJI_TEST_MYSQL_CA` = path of the MySQL test server's `ca.pem`).
#[tokio::test]
async fn tls_verify_with_ca() {
    let Ok(ca) = std::env::var("DBIJI_TEST_MYSQL_CA") else {
        return;
    };
    for srv in servers().into_iter().filter(|s| s.kind == DbKind::MySql) {
        let mut p = srv.with_ssl(SslMode::VerifyCa);
        p.ssl.ca_cert = Some(ca.clone());
        p.ssl.only_custom_ca = true;
        let mut s = srv.connect_with(&p).await;
        assert!(s.is_tls());
        assert_eq!(query(&mut s, "SELECT 1").await[0][0], Value::Int(1));
        s.close().await.unwrap();
        // VerifyFull: the auto-generated certificate is not valid for 127.0.0.1
        let mut p = srv.with_ssl(SslMode::VerifyFull);
        p.ssl.ca_cert = Some(ca.clone());
        let e = srv.try_connect(&p).await.err().expect("host name mismatch");
        assert_eq!(e.kind, ErrorKind::Tls, "{}", e.full_text());
        // ... unless the TLS name matches the certificate: it has no SAN, so
        // any name fails; the override must be applied (error mentions it)
        let mut p = srv.with_ssl(SslMode::VerifyFull);
        p.ssl.ca_cert = Some(ca.clone());
        p.tls_server_name = Some("db.internal.example".into());
        let e = srv.try_connect(&p).await.err().expect("host name mismatch");
        assert!(
            e.message.contains("db.internal.example"),
            "{}",
            e.full_text()
        );
    }
}

#[tokio::test]
async fn auth_errors_and_caching_sha2() {
    for srv in servers() {
        let mut p = srv.params.clone();
        p.password = Some("definitely-wrong".into());
        let e = srv.try_connect(&p).await.err().expect("wrong password");
        assert_eq!(e.kind, ErrorKind::Auth, "[{}] {}", srv.name, e.full_text());
        assert_eq!(e.code.as_deref(), Some("1045"));
        assert!(!e.message.contains("definitely-wrong"));

        // A fresh user forces a full authentication round (no cached hash):
        // over plain TCP caching_sha2_password needs the RSA key exchange.
        let user = format!("dbiji_u_{}", suffix());
        let mut admin = srv.connect().await;
        let plugin = if srv.is_mariadb() {
            "mysql_native_password"
        } else {
            "caching_sha2_password"
        };
        let create = if srv.is_mariadb() {
            format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'Pw-123-xyz'")
        } else {
            format!("CREATE USER '{user}'@'%' IDENTIFIED WITH {plugin} BY 'Pw-123-xyz'")
        };
        exec(&mut admin, &create).await;
        let result = async {
            for mode in [SslMode::Disable, SslMode::Disable, SslMode::Require] {
                let mut up = srv.with_ssl(mode);
                up.user = Some(user.clone());
                up.password = Some("Pw-123-xyz".into());
                up.database = None;
                let mut s = srv
                    .try_connect(&up)
                    .await
                    .map_err(|e| format!("{mode:?}: {}", e.full_text()))?;
                let rows = query(&mut s, "SELECT CURRENT_USER()").await;
                assert!(rows[0][0].to_text().starts_with(&user));
                s.close().await.map_err(|e| e.full_text())?;
            }
            Ok::<(), String>(())
        }
        .await;
        exec(&mut admin, &format!("DROP USER '{user}'@'%'")).await;
        admin.close().await.unwrap();
        if let Err(e) = result {
            panic!("[{}] {plugin} login failed: {e}", srv.name);
        }
    }
}

#[tokio::test]
async fn connection_errors() {
    for srv in servers() {
        let mut p = srv.params.clone();
        p.port = 1; // nothing listens there
        let e = srv.try_connect(&p).await.err().expect("refused");
        assert_eq!(e.kind, ErrorKind::Connection, "{}", e.full_text());
        let mut p = srv.params.clone();
        p.database = Some(format!("dbiji_missing_{}", suffix()));
        let e = srv.try_connect(&p).await.err().expect("unknown database");
        assert_eq!(e.code.as_deref(), Some("1049"), "{}", e.full_text());
    }
}

#[tokio::test]
async fn properties_and_session_setup() {
    for srv in servers() {
        let mut p = srv.params.clone();
        p.properties.insert("time_zone".into(), "+07:00".into());
        p.properties.insert(
            "init_sql".into(),
            "SET @dbiji_a = 41; SET @dbiji_b = ';'".into(),
        );
        p.properties
            .insert("transaction_isolation".into(), "READ COMMITTED".into());
        p.properties.insert("compress".into(), "true".into());
        p.read_only = true;
        let mut s = srv.connect_with(&p).await;
        let iso = if srv.is_mariadb() {
            "@@tx_isolation"
        } else {
            "@@transaction_isolation"
        };
        let ro = if srv.is_mariadb() {
            "@@tx_read_only"
        } else {
            "@@transaction_read_only"
        };
        let rows = query(
            &mut s,
            &format!("SELECT @@time_zone, @dbiji_a + 1, @dbiji_b, {iso}, {ro}"),
        )
        .await;
        assert_eq!(rows[0][0], Value::text("+07:00"));
        assert_eq!(rows[0][1].as_i64(), Some(42));
        assert_eq!(rows[0][2], Value::text(";"));
        assert_eq!(rows[0][3], Value::text("READ-COMMITTED"));
        assert_eq!(rows[0][4].as_i64(), Some(1));
        // read-only session rejects writes with ReadOnly
        let db = TestDb::create(&srv).await;
        let mut admin = db.session().await;
        exec(&mut admin, "CREATE TABLE IF NOT EXISTS w (a int)").await;
        let e = dbiji_core::driver::execute_update(
            &mut s,
            &format!("INSERT INTO `{}`.w VALUES (1)", db.name),
            &[],
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.kind,
            ErrorKind::ReadOnly,
            "[{}] {}",
            srv.name,
            e.full_text()
        );
        assert_eq!(e.code.as_deref(), Some("1792"));
        admin.close().await.unwrap();
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn latin1_connection_charset() {
    for srv in servers() {
        let mut p = srv.params.clone();
        p.properties.insert("charset".into(), "latin1".into());
        let mut s = srv.connect_with(&p).await;
        let rows = query(
            &mut s,
            "SELECT @@character_set_client, 'café', CHAR_LENGTH('café')",
        )
        .await;
        assert_eq!(rows[0][0], Value::text("latin1"));
        assert_eq!(rows[0][1], Value::text("café"));
        assert_eq!(rows[0][2].as_i64(), Some(4));
        let r = run(
            &mut s,
            "SELECT ?, CHAR_LENGTH(?)",
            &[Value::text("é!"), Value::text("é!")],
        )
        .await
        .unwrap();
        assert_eq!(r[0].1[0][0], Value::text("é!"));
        assert_eq!(r[0].1[0][1].as_i64(), Some(2));
        // characters outside latin1 are rejected client side
        let e = s
            .execute("SELECT '日本'", &[], &Default::default())
            .await
            .err()
            .expect("unrepresentable");
        assert_eq!(e.kind, ErrorKind::Query);
        s.ping().await.unwrap();
        s.close().await.unwrap();
    }
}
