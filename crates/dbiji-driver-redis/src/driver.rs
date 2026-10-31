//! [`RedisDriver`]: the DBiji driver factory for Redis.

use crate::dialect::RedisDialect;
use crate::session::RedisSession;
use async_trait::async_trait;
use dbiji_core::admin::CommandSpec;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Session};
use std::path::Path;
use std::sync::Arc;

/// Redis driver (`redis` crate, RESP3 with a fallback to RESP2).
///
/// Connection properties (`ConnectParams::properties`):
/// * `tls` – `true` forces TLS when the TLS mode is `prefer`;
/// * `protocol` – `resp2` for servers before 6.0;
/// * `client_path` – path of `redis-cli` for "execute with local client".
pub struct RedisDriver {
    info: DriverInfo,
    dialect: Arc<RedisDialect>,
}

impl RedisDriver {
    /// Create the driver.
    pub fn new() -> Self {
        RedisDriver {
            info: DriverInfo {
                kind: DbKind::Redis,
                name: "Redis",
                description: "Redis 6.0 and newer (also Valkey, KeyDB and Dragonfly)",
                capabilities: Capabilities {
                    // Databases are presented as schemas: one connection serves all.
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Databases",
                    switch_database: true,
                    transactions: false,
                    savepoints: false,
                    explain: false,
                    explain_analyze: false,
                    session_manager: true,
                    lock_manager: false,
                    server_metrics: true,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: false,
                    returning: false,
                    schema_object_kinds: vec![ObjectKind::Table],
                    table_child_kinds: vec![ObjectKind::Column],
                },
            },
            dialect: Arc::new(RedisDialect::new()),
        }
    }
}

impl Default for RedisDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for RedisDriver {
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
        Ok(Box::new(RedisSession::connect(params).await?))
    }

    /// `redis-cli -h host -p port -n db --user user --pass …`, the password in
    /// `REDISCLI_AUTH` so it never shows on the command line.
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let mut args = vec!["-h".to_string(), params.host.clone(), "-p".to_string(), params.port.to_string()];
        if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
            args.push("-n".into());
            args.push(db.to_string());
        }
        if let Some(user) = params.user.as_deref().filter(|u| !u.is_empty()) {
            args.push("--user".into());
            args.push(user.to_string());
            args.push("--no-auth-warning".into());
        }
        args.push("-f".into());
        args.push(script.display().to_string());
        let env = params.password.as_ref().filter(|p| !p.is_empty()).map(|p| vec![("REDISCLI_AUTH".to_string(), p.clone())]).unwrap_or_default();
        Ok(CommandSpec { program: params.property("client_path").unwrap_or("redis-cli").to_string(), args, env, stdin_file: None, stdout_file: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_match_the_language() {
        let d = RedisDriver::new();
        assert_eq!(d.info().kind, DbKind::Redis);
        assert_eq!(d.dialect().kind(), DbKind::Redis);
        assert!(!d.info().capabilities.transactions);
        assert!(!d.info().capabilities.explain);
        assert_eq!(d.info().capabilities.schema_label, "Databases");
    }
}
