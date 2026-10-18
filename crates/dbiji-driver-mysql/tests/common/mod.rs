//! Helpers shared by the integration tests.
//!
//! Servers are taken from `DBIJI_TEST_MYSQL` and `DBIJI_TEST_MARIADB`
//! (`mysql://user:password@host:port/database`). Without them every test
//! returns immediately. Each test works in its own randomly named database
//! that is dropped at the end (also when the test panics).

#![allow(dead_code, clippy::result_large_err)]

use dbiji_core::config::{SslConfig, SslMode};
use dbiji_core::{ConnectParams, DbKind, ExecOptions, QueryCursor, ResultHeader, Row, Session};
use dbiji_driver_mysql::{MariaDbDriver, MySqlDriver, MySqlSession};
use std::collections::BTreeMap;
use std::time::Duration;

/// One configured test server.
#[derive(Clone)]
pub struct Server {
    /// `mysql` or `mariadb`.
    pub name: &'static str,
    pub kind: DbKind,
    pub params: ConnectParams,
}

impl Server {
    pub fn is_mariadb(&self) -> bool {
        self.kind == DbKind::MariaDb
    }

    /// Parameters with another TLS mode.
    pub fn with_ssl(&self, mode: SslMode) -> ConnectParams {
        let mut p = self.params.clone();
        p.ssl.mode = mode;
        p
    }

    pub async fn connect(&self) -> MySqlSession {
        self.connect_with(&self.params).await
    }

    pub async fn connect_with(&self, params: &ConnectParams) -> MySqlSession {
        let res = match self.kind {
            DbKind::MariaDb => MariaDbDriver::new().connect_mysql(params).await,
            _ => MySqlDriver::new().connect_mysql(params).await,
        };
        match res {
            Ok(s) => s,
            Err(e) => panic!("[{}] connect failed: {}", self.name, e.full_text()),
        }
    }

    pub async fn try_connect(&self, params: &ConnectParams) -> dbiji_core::DbResult<MySqlSession> {
        match self.kind {
            DbKind::MariaDb => MariaDbDriver::new().connect_mysql(params).await,
            _ => MySqlDriver::new().connect_mysql(params).await,
        }
    }

    /// Parameters connected to database `db`.
    pub fn params_for(&self, db: &str) -> ConnectParams {
        let mut p = self.params.clone();
        p.database = Some(db.to_string());
        p
    }
}

/// Parse `mysql://user:password@host:port/database`.
pub fn parse_url(url: &str, kind: DbKind) -> Option<ConnectParams> {
    let rest = url
        .strip_prefix("mysql://")
        .or_else(|| url.strip_prefix("mariadb://"))?;
    let (auth, hostpart) = match rest.rfind('@') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => ("", rest),
    };
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (u.to_string(), Some(p.to_string())),
        None => (auth.to_string(), None),
    };
    let (hostport, db) = match hostpart.split_once('/') {
        Some((h, d)) => (h, Some(d.to_string()).filter(|d| !d.is_empty())),
        None => (hostpart, None),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (hostport.to_string(), 3306),
    };
    Some(ConnectParams {
        kind,
        host,
        port,
        tls_server_name: None,
        database: db,
        user: Some(user).filter(|u| !u.is_empty()),
        password: password.map(Into::into),
        file_path: None,
        ssl: SslConfig {
            mode: SslMode::Disable,
            ..Default::default()
        },
        properties: BTreeMap::new(),
        application_name: "dbiji-tests".into(),
        connect_timeout: Duration::from_secs(10),
        read_only: false,
        auto_commit: true,
    })
}

/// Configured servers (empty when no environment variable is set).
pub fn servers() -> Vec<Server> {
    let mut out = Vec::new();
    if let Ok(url) = std::env::var("DBIJI_TEST_MYSQL")
        && let Some(p) = parse_url(&url, DbKind::MySql)
    {
        out.push(Server {
            name: "mysql",
            kind: DbKind::MySql,
            params: p,
        });
    }
    if let Ok(url) = std::env::var("DBIJI_TEST_MARIADB")
        && let Some(p) = parse_url(&url, DbKind::MariaDb)
    {
        out.push(Server {
            name: "mariadb",
            kind: DbKind::MariaDb,
            params: p,
        });
    }
    out
}

/// Random identifier suffix.
pub fn suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

/// A database created for one test and dropped when the guard goes away.
pub struct TestDb {
    pub name: String,
    server: Server,
    dropped: bool,
}

impl TestDb {
    pub async fn create(server: &Server) -> TestDb {
        let name = format!("dbiji_t_{}", suffix());
        let mut s = server.connect().await;
        exec(
            &mut s,
            &format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4"),
        )
        .await;
        let _ = s.close().await;
        TestDb {
            name,
            server: server.clone(),
            dropped: false,
        }
    }

    /// Session whose current database is this one.
    pub async fn session(&self) -> MySqlSession {
        self.server
            .connect_with(&self.server.params_for(&self.name))
            .await
    }

    pub async fn drop_now(mut self) {
        self.dropped = true;
        if let Ok(mut s) = self.server.try_connect(&self.server.params).await {
            let _ = dbiji_core::driver::execute_update(
                &mut s,
                &format!("DROP DATABASE IF EXISTS `{}`", self.name),
                &[],
            )
            .await;
            let _ = s.close().await;
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        let name = self.name.clone();
        let server = self.server.clone();
        // Runs during unwinding too: use a private runtime on another thread.
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async move {
                    if let Ok(mut s) = server.try_connect(&server.params).await {
                        let _ = dbiji_core::driver::execute_update(
                            &mut s,
                            &format!("DROP DATABASE IF EXISTS `{name}`"),
                            &[],
                        )
                        .await;
                        let _ = s.close().await;
                    }
                });
            }
        })
        .join();
    }
}

/// Execute a statement (all results) and panic on error.
pub async fn exec(s: &mut MySqlSession, sql: &str) {
    if let Err(e) = dbiji_core::driver::execute_update(s, sql, &[]).await {
        panic!("statement failed: {sql}\n{}", e.full_text());
    }
}

/// First result rows of a query.
pub async fn query(s: &mut MySqlSession, sql: &str) -> Vec<Row> {
    match dbiji_core::driver::query_all(s, sql, &[]).await {
        Ok((_, rows)) => rows,
        Err(e) => panic!("query failed: {sql}\n{}", e.full_text()),
    }
}

/// Collect every result of a cursor: (header, rows) pairs.
pub async fn collect_all(
    cursor: &mut Box<dyn QueryCursor>,
    fetch: usize,
) -> dbiji_core::DbResult<Vec<(ResultHeader, Vec<Row>)>> {
    let mut out = Vec::new();
    while let Some(h) = cursor.next_result().await? {
        let mut rows = Vec::new();
        if matches!(h, ResultHeader::Rows(_)) {
            loop {
                let batch = cursor.fetch(fetch).await?;
                if batch.is_empty() {
                    break;
                }
                rows.extend(batch);
            }
        }
        out.push((h, rows));
    }
    Ok(out)
}

/// Execute and collect all results.
pub async fn run(
    s: &mut MySqlSession,
    sql: &str,
    params: &[dbiji_core::Value],
) -> dbiji_core::DbResult<Vec<(ResultHeader, Vec<Row>)>> {
    let mut c = s.execute(sql, params, &ExecOptions::default()).await?;
    let r = collect_all(&mut c, 256).await;
    let _ = c.close().await;
    r
}

/// CTE producing `n` = 0 .. 10^digits - 1 (works on MySQL 8 and MariaDB 10.2+).
pub fn numbers_sql(digits: usize, pad: usize) -> String {
    let d = "SELECT 0 AS x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 \
             UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9";
    let names: Vec<String> = (0..digits).map(|i| format!("d{i}")).collect();
    let expr = names
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{n}.x * {}", 10u64.pow(i as u32)))
        .collect::<Vec<_>>()
        .join(" + ");
    let from = names
        .iter()
        .map(|n| format!("dg {n}"))
        .collect::<Vec<_>>()
        .join(" CROSS JOIN ");
    format!("WITH dg AS ({d}) SELECT {expr} AS n, REPEAT('x', {pad}) AS pad FROM {from}")
}
