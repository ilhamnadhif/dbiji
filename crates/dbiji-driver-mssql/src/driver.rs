//! [`SqlServerDriver`]: the DBiji driver factory for Microsoft SQL Server.

use crate::dialect::MsSqlDialect;
use crate::session::SqlServerSession;
use async_trait::async_trait;
use dbiji_core::admin::CommandSpec;
use dbiji_core::config::SslMode;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Secret, Session};
use std::path::Path;
use std::sync::Arc;

/// Microsoft SQL Server / Azure SQL driver (TDS via tiberius).
///
/// Connection properties (`ConnectParams::properties`):
/// * `instance` – named instance, resolved through SQL Browser (UDP 1434)
///   when the port is the default 1433;
/// * `application_intent` – `ReadOnly` for read-only routing in availability groups;
/// * `trust_server_certificate` – `true` skips certificate validation even for verify modes;
/// * `hostname_in_certificate` – name expected in the server certificate;
/// * `authentication` – `ActiveDirectoryAccessToken` uses the password as an Azure AD token;
/// * `packet_size`, `workstation_id`, `multi_subnet_failover`.
pub struct SqlServerDriver {
    info: DriverInfo,
    dialect: Arc<MsSqlDialect>,
}

impl SqlServerDriver {
    /// Create the driver.
    pub fn new() -> Self {
        SqlServerDriver {
            info: DriverInfo {
                kind: DbKind::SqlServer,
                name: "SQL Server",
                description: "Microsoft SQL Server and Azure SQL (TDS)",
                capabilities: Capabilities {
                    multiple_databases: true,
                    schemas: true,
                    schema_label: "Schemas",
                    switch_database: true,
                    transactions: true,
                    savepoints: true,
                    explain: true,
                    explain_analyze: true,
                    session_manager: true,
                    lock_manager: true,
                    server_metrics: true,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: true,
                    returning: false,
                    schema_object_kinds: vec![
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::Procedure,
                        ObjectKind::Function,
                        ObjectKind::Sequence,
                        ObjectKind::Synonym,
                        ObjectKind::Type,
                    ],
                    table_child_kinds: vec![
                        ObjectKind::Column,
                        ObjectKind::Index,
                        ObjectKind::PrimaryKey,
                        ObjectKind::UniqueKey,
                        ObjectKind::ForeignKey,
                        ObjectKind::CheckConstraint,
                        ObjectKind::Trigger,
                    ],
                },
            },
            dialect: Arc::new(MsSqlDialect::new()),
        }
    }
}

impl Default for SqlServerDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for SqlServerDriver {
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
        Ok(Box::new(SqlServerSession::connect(params).await?))
    }

    /// `sqlcmd -S host,port -U user -d db -i script -b`; the password is
    /// passed in `SQLCMDPASSWORD`. Backups are server side (`BACKUP
    /// DATABASE`, see the `backup_database` maintenance operation).
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let server = match params.property("instance").filter(|i| !i.is_empty()) {
            Some(inst) if params.port == 1433 => format!("{}\\{inst}", params.host),
            _ => format!("{},{}", params.host, params.port),
        };
        let mut args = vec!["-S".to_string(), server];
        match params.user.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => {
                args.push("-U".into());
                args.push(u.to_string());
            }
            None => args.push("-E".into()),
        }
        if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
            args.push("-d".into());
            args.push(db.to_string());
        }
        match params.ssl.mode {
            SslMode::Disable | SslMode::Prefer => args.push("-C".into()),
            SslMode::Require => {
                args.push("-N".into());
                args.push("-C".into());
            }
            SslMode::VerifyCa | SslMode::VerifyFull => args.push("-N".into()),
        }
        args.extend(["-I".into(), "-b".into(), "-f".into(), "65001".into(), "-i".into(), script.display().to_string()]);
        let mut env: Vec<(String, Secret)> = Vec::new();
        if let Some(p) = &params.password {
            env.push(("SQLCMDPASSWORD".into(), p.clone()));
        }
        Ok(CommandSpec { program: "sqlcmd".into(), args, env, stdin_file: None, stdout_file: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ConnectionConfig;

    #[test]
    fn client_command_never_shows_password() {
        let d = SqlServerDriver::new();
        let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "x");
        cfg.host = "db.local".into();
        cfg.port = Some(14330);
        cfg.database = Some("shop".into());
        let p = ConnectParams::from_config(&cfg, Some(Secret::new("s3cret")));
        let c = d.client_command(&p, Path::new("/tmp/run.sql")).unwrap();
        assert_eq!(c.program, "sqlcmd");
        let shown = c.display();
        assert!(shown.contains("-S db.local,14330"), "{shown}");
        assert!(shown.contains("-U sa"));
        assert!(shown.contains("-d shop"));
        assert!(shown.contains("-i /tmp/run.sql"));
        assert!(shown.contains("-b"));
        assert!(!shown.contains("s3cret"));
        assert_eq!(c.env[0].0, "SQLCMDPASSWORD");
        assert_eq!(c.env[0].1.expose(), "s3cret");
        let mut cfg2 = cfg.clone();
        cfg2.port = Some(1433);
        cfg2.properties.insert("instance".into(), "SQLEXPRESS".into());
        let p2 = ConnectParams::from_config(&cfg2, None);
        assert!(d.client_command(&p2, Path::new("a.sql")).unwrap().display().contains("db.local\\SQLEXPRESS"));
        assert!(d.backup_command(&p2, &Default::default()).is_err());
    }

    #[test]
    fn capabilities() {
        let d = SqlServerDriver::new();
        let c = &d.info().capabilities;
        assert!(c.multiple_databases && c.switch_database && c.explain_analyze && c.lock_manager);
        assert!(!c.returning);
        assert_eq!(d.kind(), DbKind::SqlServer);
    }
}
