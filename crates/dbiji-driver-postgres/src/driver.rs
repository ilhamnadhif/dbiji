//! [`PostgresDriver`] and [`CockroachDriver`]: the [`Driver`] factories.

use crate::dialect::PgDialect;
use crate::session::PgSession;
use crate::tools;
use async_trait::async_trait;
use dbiji_core::admin::{BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::{ConnectParams, DbError, DbKind, DbResult, Driver, Session};
use std::path::Path;
use std::sync::Arc;

fn capabilities(cockroach: bool) -> Capabilities {
    use ObjectKind::*;
    Capabilities {
        multiple_databases: true,
        schemas: true,
        schema_label: "Schemas",
        switch_database: false,
        transactions: true,
        savepoints: true,
        explain: true,
        explain_analyze: true,
        session_manager: true,
        lock_manager: true,
        server_metrics: true,
        backup: !cockroach,
        restore: !cockroach,
        debugger: !cockroach,
        cancel: true,
        returning: true,
        schema_object_kinds: if cockroach {
            vec![Table, View, MaterializedView, Sequence, Function, Procedure, Type]
        } else {
            vec![Table, View, MaterializedView, ForeignTable, Sequence, Function, Procedure, Aggregate, Type, Domain]
        },
        table_child_kinds: if cockroach {
            vec![Column, Index, PrimaryKey, UniqueKey, CheckConstraint, ForeignKey, Trigger]
        } else {
            vec![Column, Index, PrimaryKey, UniqueKey, CheckConstraint, ForeignKey, Trigger, Partition]
        },
    }
}

/// PostgreSQL driver (also used for PostgreSQL compatible servers such as
/// Greenplum, AlloyDB, Timescale, Supabase, Neon). A CockroachDB server is
/// detected from `version()` and the catalog queries adapt automatically.
pub struct PostgresDriver {
    info: DriverInfo,
    dialect: Arc<PgDialect>,
}

impl Default for PostgresDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PostgresDriver {
    /// Create the driver.
    pub fn new() -> Self {
        PostgresDriver {
            info: DriverInfo {
                kind: DbKind::Postgres,
                name: "PostgreSQL",
                description: "PostgreSQL 11+ via the native protocol (tokio-postgres, rustls)",
                capabilities: capabilities(false),
            },
            dialect: Arc::new(PgDialect::new()),
        }
    }
}

#[async_trait]
impl Driver for PostgresDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        Ok(Box::new(PgSession::open(params, DbKind::Postgres).await?))
    }

    fn backup_command(&self, params: &ConnectParams, opts: &BackupOptions) -> DbResult<CommandSpec> {
        tools::backup_command(params, opts)
    }

    fn restore_command(&self, params: &ConnectParams, opts: &RestoreOptions) -> DbResult<CommandSpec> {
        tools::restore_command(params, opts)
    }

    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        tools::client_command(params, script)
    }
}

/// CockroachDB driver: PostgreSQL protocol with CockroachDB specific
/// catalogs (`crdb_internal`) for sessions, locks and metrics. No debugger
/// and no pg_dump based backup (CockroachDB uses `BACKUP` / `RESTORE`).
pub struct CockroachDriver {
    info: DriverInfo,
    dialect: Arc<PgDialect>,
}

impl Default for CockroachDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl CockroachDriver {
    /// Create the driver.
    pub fn new() -> Self {
        CockroachDriver {
            info: DriverInfo { kind: DbKind::CockroachDb, name: "CockroachDB", description: "CockroachDB via the PostgreSQL protocol", capabilities: capabilities(true) },
            dialect: Arc::new(PgDialect::cockroach()),
        }
    }
}

#[async_trait]
impl Driver for CockroachDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        Ok(Box::new(PgSession::open(params, DbKind::CockroachDb).await?))
    }

    fn backup_command(&self, _params: &ConnectParams, _opts: &BackupOptions) -> DbResult<CommandSpec> {
        Err(DbError::unsupported("CockroachDB backups are made with the BACKUP statement, not with pg_dump"))
    }

    fn restore_command(&self, _params: &ConnectParams, _opts: &RestoreOptions) -> DbResult<CommandSpec> {
        Err(DbError::unsupported("CockroachDB backups are restored with the RESTORE statement"))
    }

    fn client_command(&self, params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
        tools::client_command(params, script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_info() {
        let pg = PostgresDriver::new();
        assert_eq!(pg.kind(), DbKind::Postgres);
        let c = &pg.info().capabilities;
        assert!(c.multiple_databases && c.debugger && c.backup && c.restore && c.returning && c.explain_analyze);
        assert_eq!(c.schema_object_kinds.len(), 10);
        assert!(c.table_child_kinds.contains(&ObjectKind::Partition));
        assert_eq!(pg.dialect().placeholder(1), "$1");
        let cr = CockroachDriver::new();
        assert_eq!(cr.kind(), DbKind::CockroachDb);
        assert!(!cr.info().capabilities.debugger);
        assert_eq!(cr.dialect().kind(), DbKind::CockroachDb);
        assert!(cr.backup_command(&ConnectParams::from_config(&dbiji_core::ConnectionConfig::default(), None), &Default::default()).is_err());
    }
}
