//! Oracle Database driver for DBiji, on Oracle's pure Rust thin client
//! (`oracledb`): no Oracle Client libraries.
//!
//! Connection properties (`ConnectParams::properties`):
//! * `sid` – connect to a SID instead of the service name in the database field;
//! * `connect_descriptor` – a complete `(DESCRIPTION=...)` used as is;
//! * `role` – `SYSDBA`, `SYSOPER`, `SYSDG`, `SYSBACKUP` or `SYSKM`;
//! * `wallet_location` / `wallet_password` – an Oracle wallet (TCPS, mTLS);
//! * `time_zone` – the session time zone (`ALTER SESSION SET TIME_ZONE`).

#![allow(clippy::result_large_err)]

mod catalog;
mod convert;
mod ddl;
mod dialect;
mod error;
mod session;

pub use dialect::OracleDialect;
pub use session::OracleSession;

use async_trait::async_trait;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Dialect, Driver, Session};
use std::sync::Arc;

pub struct OracleDriver {
    info: DriverInfo,
    dialect: Arc<OracleDialect>,
}

impl OracleDriver {
    pub fn new() -> Self {
        OracleDriver {
            info: DriverInfo {
                kind: DbKind::Oracle,
                name: "Oracle",
                description: "Oracle Database 12c and later (thin client, no Oracle Client needed)",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Schemas",
                    switch_database: false,
                    transactions: true,
                    savepoints: true,
                    explain: true,
                    explain_analyze: false,
                    session_manager: true,
                    lock_manager: false,
                    server_metrics: false,
                    backup: false,
                    restore: false,
                    debugger: false,
                    cancel: true,
                    returning: false,
                    schema_object_kinds: vec![
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::MaterializedView,
                        ObjectKind::Sequence,
                        ObjectKind::Function,
                        ObjectKind::Procedure,
                        ObjectKind::Type,
                        ObjectKind::Synonym,
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
            dialect: Arc::new(OracleDialect::new()),
        }
    }
}

impl Default for OracleDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for OracleDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        if params.host.trim().is_empty()
            && params
                .property("connect_descriptor")
                .is_none_or(|d| d.trim().is_empty())
        {
            return Err(DbError::config("A host name is required"));
        }
        Ok(Box::new(OracleSession::connect(params).await?))
    }
}

/// Register the Oracle driver.
pub fn register(reg: &mut DriverRegistry) {
    reg.register(Arc::new(OracleDriver::new()));
}
