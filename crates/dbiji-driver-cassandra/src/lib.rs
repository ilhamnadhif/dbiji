//! Apache Cassandra and ScyllaDB driver for DBiji (CQL native protocol, on
//! the pure Rust `scylla` driver).
//!
//! Connection properties (`ConnectParams::properties`):
//! * `hosts` – more contact points, `host:port,host:port`;
//! * `local_datacenter` – the data center to prefer;
//! * `direct_nodes` – `true` to connect to every node at the address it
//!   announces (by default, with a single contact point, all traffic goes
//!   through it: nodes behind Docker, NAT or an SSH tunnel are unreachable).
//!
//! The database field is the keyspace to use. Cassandra has no
//! transactions: every statement is applied at once.

#![allow(clippy::result_large_err)]

mod dialect;
mod session;

pub use dialect::CqlDialect;
pub use session::CassandraSession;

use async_trait::async_trait;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Dialect, Driver, Session};
use std::sync::Arc;

pub struct CassandraDriver {
    info: DriverInfo,
    dialect: Arc<CqlDialect>,
}

impl CassandraDriver {
    pub fn new() -> Self {
        CassandraDriver {
            info: DriverInfo {
                kind: DbKind::Cassandra,
                name: "Cassandra",
                description: "Apache Cassandra 3+, ScyllaDB, Amazon Keyspaces and DataStax Astra (CQL)",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Keyspaces",
                    switch_database: false,
                    transactions: false,
                    savepoints: false,
                    explain: false,
                    explain_analyze: false,
                    session_manager: false,
                    lock_manager: false,
                    server_metrics: false,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: false,
                    returning: false,
                    schema_object_kinds: vec![
                        ObjectKind::Table,
                        ObjectKind::MaterializedView,
                        ObjectKind::Type,
                        ObjectKind::Function,
                    ],
                    table_child_kinds: vec![ObjectKind::Column, ObjectKind::Index],
                },
            },
            dialect: Arc::new(CqlDialect::new()),
        }
    }
}

impl Default for CassandraDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for CassandraDriver {
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
        Ok(Box::new(CassandraSession::connect(params).await?))
    }
}

/// Register the Cassandra driver.
pub fn register(reg: &mut DriverRegistry) {
    reg.register(Arc::new(CassandraDriver::new()));
}
