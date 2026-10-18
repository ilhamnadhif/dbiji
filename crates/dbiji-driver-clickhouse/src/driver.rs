//! [`ClickHouseDriver`]: the DBiji driver factory for ClickHouse.

use crate::dialect::ClickHouseDialect;
use crate::session::ClickHouseSession;
use async_trait::async_trait;
use dbiji_core::admin::CommandSpec;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Secret, Session};
use std::path::Path;
use std::sync::Arc;

/// ClickHouse driver using the HTTP(S) interface (default ports 8123 / 8443).
///
/// Connection properties (`ConnectParams::properties`):
/// * `path` – URL path when ClickHouse sits behind a reverse proxy;
/// * `session_timeout` – HTTP session timeout in seconds (default 3600, lowered
///   automatically to the server's `max_session_timeout`);
/// * `compression` – `true` to request gzip compressed responses;
/// * `setting.<name>` – any ClickHouse setting sent with every query.
pub struct ClickHouseDriver {
    info: DriverInfo,
    dialect: Arc<ClickHouseDialect>,
}

impl ClickHouseDriver {
    /// Create the driver.
    pub fn new() -> Self {
        ClickHouseDriver {
            info: DriverInfo {
                kind: DbKind::ClickHouse,
                name: "ClickHouse",
                description: "ClickHouse over the HTTP interface",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Databases",
                    switch_database: true,
                    transactions: false,
                    savepoints: false,
                    explain: true,
                    explain_analyze: false,
                    session_manager: true,
                    lock_manager: false,
                    server_metrics: true,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: true,
                    returning: false,
                    schema_object_kinds: vec![ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView, ObjectKind::Type, ObjectKind::Function],
                    table_child_kinds: vec![ObjectKind::Column, ObjectKind::Index, ObjectKind::PrimaryKey, ObjectKind::CheckConstraint],
                },
            },
            dialect: Arc::new(ClickHouseDialect::new()),
        }
    }
}

impl Default for ClickHouseDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for ClickHouseDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        if params.host.trim().is_empty() {
            return Err(DbError::config("A host name is required"));
        }
        Ok(Box::new(ClickHouseSession::connect(params).await?))
    }

    /// `clickhouse client` with the script on stdin (the native client uses
    /// the TCP protocol, port 9000/9440, configurable with the `native_port`
    /// property).
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let native_port = params.property("native_port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(if params.ssl.mode.uses_tls() && params.port == 8443 { 9440 } else { 9000 });
        let mut args = vec!["client".to_string(), "--host".into(), params.host.clone(), "--port".into(), native_port.to_string(), "--multiquery".into()];
        if let Some(u) = &params.user {
            args.push("--user".into());
            args.push(u.clone());
        }
        if let Some(db) = &params.database {
            args.push("--database".into());
            args.push(db.clone());
        }
        if params.ssl.mode.uses_tls() && native_port == 9440 {
            args.push("--secure".into());
        }
        let mut env: Vec<(String, Secret)> = Vec::new();
        if let Some(p) = &params.password {
            env.push(("CLICKHOUSE_PASSWORD".into(), p.clone()));
        }
        Ok(CommandSpec { program: "clickhouse".into(), args, env, stdin_file: Some(script.to_path_buf()), stdout_file: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ConnectionConfig;

    #[test]
    fn info_and_client_command() {
        let d = ClickHouseDriver::new();
        assert_eq!(d.kind(), DbKind::ClickHouse);
        assert!(!d.info().capabilities.transactions);
        let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "x");
        cfg.host = "ch.local".into();
        let p = ConnectParams::from_config(&cfg, Some(Secret::new("pw")));
        let c = d.client_command(&p, Path::new("/tmp/a.sql")).unwrap();
        assert_eq!(c.program, "clickhouse");
        assert!(c.args.contains(&"ch.local".to_string()));
        assert!(!c.display().contains("pw"));
        assert_eq!(c.env[0].0, "CLICKHOUSE_PASSWORD");
    }
}
