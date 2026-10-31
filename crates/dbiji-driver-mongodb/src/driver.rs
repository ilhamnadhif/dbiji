//! [`MongoDriver`]: the DBiji driver factory for MongoDB.

use crate::dialect::MongoDialect;
use crate::session::MongoSession;
use async_trait::async_trait;
use dbiji_core::admin::CommandSpec;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Session};
use std::path::Path;
use std::sync::Arc;

/// MongoDB driver (the official `mongodb` crate).
///
/// Connection properties (`ConnectParams::properties`):
/// * `authSource` – database the user is defined in (default: the
///   connection's database, else `admin`);
/// * `authMechanism` – `SCRAM-SHA-256`, `SCRAM-SHA-1`, `MONGODB-X509` …;
/// * `tls` – `true` forces TLS when the TLS mode is `prefer`;
/// * `replicaSet` – replica set name, for a seed list behind a proxy;
/// * `directConnection` – connect to the one host given, without discovery.
pub struct MongoDriver {
    info: DriverInfo,
    dialect: Arc<MongoDialect>,
}

impl MongoDriver {
    /// Create the driver.
    pub fn new() -> Self {
        MongoDriver {
            info: DriverInfo {
                kind: DbKind::MongoDb,
                name: "MongoDB",
                description: "MongoDB 4.2 and newer, through the shell language (official Rust driver)",
                capabilities: Capabilities {
                    // Databases are presented as schemas: one connection serves all.
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Databases",
                    switch_database: true,
                    transactions: false,
                    savepoints: false,
                    explain: true,
                    explain_analyze: true,
                    session_manager: false,
                    lock_manager: false,
                    server_metrics: false,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: false,
                    returning: false,
                    schema_object_kinds: vec![ObjectKind::Table, ObjectKind::View],
                    table_child_kinds: vec![ObjectKind::Column, ObjectKind::Index],
                },
            },
            dialect: Arc::new(MongoDialect::new()),
        }
    }
}

impl Default for MongoDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for MongoDriver {
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
        Ok(Box::new(MongoSession::connect(params).await?))
    }

    /// `mongosh --host h --port p -u user --authenticationDatabase db script`.
    /// The password is passed in `MONGOSH_PASSWORD` (`MONGOSH_PWD` for old
    /// shells), never on the command line.
    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        let mut args = vec!["--host".to_string(), params.host.clone(), "--port".to_string(), params.port.to_string()];
        if let Some(user) = params.user.as_deref().filter(|u| !u.is_empty()) {
            args.push("-u".into());
            args.push(user.to_string());
            args.push("--authenticationDatabase".into());
            args.push(crate::session::auth_source(params).unwrap_or_else(|| "admin".into()));
        }
        if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
            args.push(db.to_string());
        }
        args.push("--file".into());
        args.push(script.display().to_string());
        Ok(CommandSpec { program: "mongosh".into(), args, env: Vec::new(), stdin_file: None, stdout_file: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_match_the_language() {
        let d = MongoDriver::new();
        assert_eq!(d.info().kind, DbKind::MongoDb);
        assert_eq!(d.dialect().kind(), DbKind::MongoDb);
        assert!(!d.info().capabilities.transactions);
        assert_eq!(d.info().capabilities.schema_label, "Databases");
    }

    #[test]
    fn the_client_command_never_carries_the_password() {
        let d = MongoDriver::new();
        let mut params = dbiji_core::driver::ConnectParams {
            kind: DbKind::MongoDb,
            host: "localhost".into(),
            port: 27017,
            tls_server_name: None,
            database: Some("shop".into()),
            user: Some("root".into()),
            password: None,
            file_path: None,
            ssl: Default::default(),
            properties: Default::default(),
            application_name: "DBiji".into(),
            connect_timeout: std::time::Duration::from_secs(10),
            read_only: false,
            auto_commit: true,
        };
        params.password = Some("secret".into());
        let cmd = d.client_command(&params, Path::new("/tmp/a.js")).unwrap();
        assert_eq!(cmd.program, "mongosh");
        assert!(!cmd.args.iter().any(|a| a.contains("secret")));
        assert!(cmd.args.contains(&"/tmp/a.js".to_string()));
    }

    #[test]
    fn the_client_command_authenticates_in_the_uri_database() {
        let d = MongoDriver::new();
        let mut params = dbiji_core::driver::ConnectParams {
            kind: DbKind::MongoDb,
            host: "localhost".into(),
            port: 27017,
            tls_server_name: None,
            database: Some("shop".into()),
            user: Some("clerk".into()),
            password: None,
            file_path: None,
            ssl: Default::default(),
            properties: Default::default(),
            application_name: "DBiji".into(),
            connect_timeout: std::time::Duration::from_secs(10),
            read_only: false,
            auto_commit: true,
        };
        let auth_db = |params: &dbiji_core::driver::ConnectParams| {
            let cmd = d.client_command(params, Path::new("/tmp/a.js")).unwrap();
            let i = cmd.args.iter().position(|a| a == "--authenticationDatabase").expect("an auth database");
            cmd.args[i + 1].clone()
        };
        // The URI rule: the database in the connection string, else admin.
        assert_eq!(auth_db(&params), "shop");
        // An explicit authSource wins over the database.
        params.properties.insert("authSource".into(), "admin".into());
        assert_eq!(auth_db(&params), "admin");
        // Without either the driver's default applies.
        params.database = None;
        params.properties.clear();
        assert_eq!(auth_db(&params), "admin");
    }
}
