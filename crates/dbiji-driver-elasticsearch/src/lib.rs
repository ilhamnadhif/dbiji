//! Elasticsearch and OpenSearch driver for DBiji, over HTTP(S) with the
//! pure Rust `ureq` client: SQL (the SQL endpoint, paged with its cursor)
//! and REST requests such as `GET /index/_search` followed by a JSON body.
//!
//! Connection properties: `https` (`true` without TLS settings), `api_key`
//! (instead of user and password), `path` (behind a reverse proxy).

#![allow(clippy::result_large_err)]

mod dialect;
mod session;

pub use dialect::SearchDialect;
pub use session::SearchSession;

use async_trait::async_trait;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Dialect, Driver, Session};
use std::sync::Arc;

pub struct SearchDriver {
    info: DriverInfo,
    dialect: Arc<SearchDialect>,
}

impl SearchDriver {
    pub fn new() -> Self {
        SearchDriver {
            info: DriverInfo {
                kind: DbKind::Elasticsearch,
                name: "Elasticsearch",
                description: "Elasticsearch 7.10+ and OpenSearch 1+ (SQL and REST)",
                capabilities: Capabilities {
                    multiple_databases: false,
                    schemas: true,
                    schema_label: "Clusters",
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
                    table_child_kinds: vec![ObjectKind::Column],
                },
            },
            dialect: Arc::new(SearchDialect::new()),
        }
    }
}

impl Default for SearchDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Driver for SearchDriver {
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
        Ok(Box::new(SearchSession::connect(params).await?))
    }
}

/// Register the Elasticsearch / OpenSearch driver.
pub fn register(reg: &mut DriverRegistry) {
    reg.register(Arc::new(SearchDriver::new()));
}
