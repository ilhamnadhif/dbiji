//! Redis driver for DBiji (also Valkey, KeyDB, Dragonfly, Garnet), on the
//! pure Rust `redis` crate.
//!
//! The database field is the database number (`0` … `15`). Keys are the
//! navigator's objects; the editor runs Redis commands, one per line.

#![allow(clippy::result_large_err)]

mod dialect;
mod session;

pub use dialect::RedisDialect;
pub use session::RedisSession;

use async_trait::async_trait;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Dialect, Driver, Session};
use std::sync::Arc;

pub struct RedisDriver {
    info: DriverInfo,
    dialect: Arc<RedisDialect>,
}

impl RedisDriver {
    pub fn new() -> Self {
        RedisDriver {
            info: DriverInfo {
                kind: DbKind::Redis,
                name: "Redis",
                description: "Redis 5+, Valkey, KeyDB, Dragonfly and Garnet",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Databases",
                    switch_database: false,
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
                    table_child_kinds: Vec::new(),
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
}

/// Register the Redis driver.
pub fn register(reg: &mut DriverRegistry) {
    reg.register(Arc::new(RedisDriver::new()));
}
