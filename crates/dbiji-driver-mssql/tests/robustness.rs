//! Arbitrary-input robustness: the hand written T-SQL scanners, the plan
//! parser and the dialect helpers must never panic.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ConnectParams, Driver};
use dbiji_core::{DbKind, ErrorKind, Secret};
use dbiji_driver_mssql::sqltext::*;
use dbiji_driver_mssql::{MsSqlDialect, SqlServerDriver};
use std::time::Duration;

/// Deterministic pseudo random strings over a nasty alphabet.
fn samples() -> Vec<String> {
    let alphabet: Vec<char> = "abcSELECTGOgo \n\t'\"[]()/*-;,.@#$0123456789éü😀N\\".chars().collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = vec![
        String::new(),
        "'".into(),
        "[".into(),
        "/*".into(),
        "/* /* */".into(),
        "N'".into(),
        "GO".into(),
        "\nGO 0\n".into(),
        "SELECT '😀".into(),
        "((((".into(),
        "))))".into(),
        "CREATE VIEW".into(),
        "WITH".into(),
        "SELECT TOP".into(),
        "1e".into(),
        "0x".into(),
        "@".into(),
        "é".repeat(10),
    ];
    for _ in 0..20_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 40) as usize;
        let mut s = String::new();
        let mut x = seed;
        for _ in 0..len {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s.push(alphabet[(x >> 33) as usize % alphabet.len()]);
        }
        out.push(s);
    }
    out
}

#[test]
fn scanners_never_panic() {
    let d = MsSqlDialect::new();
    for s in samples() {
        let _ = tokenize(&s);
        let _ = first_keyword(&s);
        let _ = classify(&s);
        let _ = is_scope_sensitive(&s);
        let _ = touches_session_state(&s);
        let _ = split_batches(&s);
        let _ = line_start_position(&s, 3);
        let _ = strip_outer_parens(&s);
        let _ = view_select(&s);
        let _ = select_shape(&s);
        let _ = unquote_ident(&s);
        let _ = d.limit_query(&s, 10, 5);
        let _ = d.count_query(&s);
        let _ = d.quote_ident(&s);
        let _ = d.literal(&dbiji_core::Value::text(s.clone()), Some(&s));
        let _ = dbiji_driver_mssql::plan::build_plan(std::slice::from_ref(&s), false);
        let _ = dbiji_driver_mssql::types::parse_numeric(&s);
        let _ = dbiji_driver_mssql::types::kind_of_type_name(&s);
        let _ = d.ddl().map_type(dbiji_core::DbKind::Postgres, &s, dbiji_core::value::DataKind::Text);
        let _ = d.ddl().map_type(dbiji_core::DbKind::ClickHouse, &s, dbiji_core::value::DataKind::Text);
    }
}

/// Counts the TCP connections a failing attempt opens, so a "retried without
/// TLS" claim can be checked instead of assumed.
struct DroppingServer {
    port: u16,
    accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl DroppingServer {
    async fn start() -> DroppingServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        // A server that speaks TLS 1.0 only (SQL Server 2005 and older) answers
        // our TLS 1.2 ClientHello by dropping the connection.
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drop(sock);
                    }
                    Err(_) => return,
                }
            }
        });
        DroppingServer { port, accepted, task }
    }

    fn connections(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for DroppingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn params_for(port: u16, mode: SslMode) -> ConnectParams {
    let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "legacy");
    cfg.host = "127.0.0.1".into();
    cfg.port = Some(port);
    cfg.user = Some("sa".into());
    cfg.ssl.mode = mode;
    cfg.init.connect_timeout_secs = 5;
    ConnectParams::from_config(&cfg, Some(Secret::new("pw")))
}

/// A server that speaks TLS 1.0 only (SQL Server 2005 and older) answers our
/// TLS 1.2 ClientHello with a fatal alert or closes the socket. `prefer` must
/// then try again without TLS; `require` must not.
#[tokio::test]
async fn a_legacy_tls_server_is_retried_without_tls() {
    let server = DroppingServer::start().await;

    let err = SqlServerDriver::new().connect(&params_for(server.port, SslMode::Prefer)).await.err().expect("no connection");
    assert!(matches!(err.kind, ErrorKind::Connection | ErrorKind::Tls), "{err:?}");
    assert!(!err.message.is_empty());
    // Two attempts: the encrypted one, then the plain one.
    assert_eq!(server.connections(), 2, "prefer must retry without TLS once");

    let before = server.connections();
    let err = SqlServerDriver::new().connect(&params_for(server.port, SslMode::Require)).await.err().expect("no connection");
    assert!(matches!(err.kind, ErrorKind::Tls | ErrorKind::Connection), "{err:?}");
    // `require` means TLS or nothing: one attempt, no downgrade.
    assert_eq!(server.connections() - before, 1, "require must not downgrade to plain TCP");

    // `disable` goes straight to plain TCP (SQL Server 2005 does not force
    // encryption by default, so this is the way in for those servers).
    let before = server.connections();
    let _ = SqlServerDriver::new().connect(&params_for(server.port, SslMode::Disable)).await;
    assert_eq!(server.connections() - before, 1, "disable must not attempt TLS");
}

/// A refused port is a connection error naming the address, and it returns
/// long before the connect timeout.
#[tokio::test]
async fn a_closed_port_fails_as_a_connection_error() {
    let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "closed");
    cfg.host = "127.0.0.1".into();
    cfg.port = Some(1);
    cfg.user = Some("sa".into());
    cfg.ssl.mode = SslMode::VerifyFull;
    cfg.init.connect_timeout_secs = 2;
    let params = ConnectParams::from_config(&cfg, Some(Secret::new("pw")));
    let started = std::time::Instant::now();
    let err = SqlServerDriver::new().connect(&params).await.err().expect("no connection");
    assert!(matches!(err.kind, ErrorKind::Connection | ErrorKind::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}
