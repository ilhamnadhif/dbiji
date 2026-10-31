//! MongoDB driver for DBiji (also FerretDB, Amazon DocumentDB and Azure
//! Cosmos DB for MongoDB), on MongoDB's official Rust driver.
//!
//! The editor runs a subset of the `mongosh` language:
//! `db.<collection>.find/aggregate/countDocuments/distinct/insert…/update…/
//! delete…(…)`, `db.runCommand({…})`, `show dbs`, `use <db>`.
//!
//! Connection properties: `srv` (`true`: `mongodb+srv://`, Atlas), `hosts`
//! (`h1:27017,h2:27017`), `replicaSet`, `authSource`, `authMechanism` and any
//! other connection string option.

#![allow(clippy::result_large_err)]

mod dialect;
mod session;
mod shell;

pub use dialect::MongoDialect;
pub use session::MongoSession;

use async_trait::async_trait;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Dialect, Driver, Session};
use std::sync::Arc;

pub struct MongoDriver {
    info: DriverInfo,
    dialect: Arc<MongoDialect>,
}

impl MongoDriver {
    pub fn new() -> Self {
        MongoDriver {
            info: DriverInfo {
                kind: DbKind::MongoDb,
                name: "MongoDB",
                description: "MongoDB 4.2+, Atlas, FerretDB, Amazon DocumentDB, Azure Cosmos DB for MongoDB",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Databases",
                    switch_database: false,
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
        if params.host.trim().is_empty()
            && params.property("hosts").is_none_or(|h| h.trim().is_empty())
        {
            return Err(DbError::config("A host name is required"));
        }
        Ok(Box::new(MongoSession::connect(params).await?))
    }
}

/// Register the MongoDB driver.
pub fn register(reg: &mut DriverRegistry) {
    reg.register(Arc::new(MongoDriver::new()));
}
