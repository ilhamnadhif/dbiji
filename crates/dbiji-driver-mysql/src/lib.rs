//! MySQL (5.7, 8.x) and MariaDB (10.x, 11.x) driver for DBiji, built on
//! `mysql_async`.
//!
//! * [`MySqlDriver`] ([`DbKind::MySql`]) and [`MariaDbDriver`]
//!   ([`DbKind::MariaDb`]) share one implementation. The actual product and
//!   version are detected from the server (`VERSION()` contains `MariaDB`),
//!   and catalog queries, EXPLAIN and DDL adapt to them.
//! * Statements without parameters use the text protocol, statements with
//!   parameters (`?` placeholders) the binary protocol. Results are streamed:
//!   a cursor owns the connection in a background task and reads rows only on
//!   demand (see [`cursor`]).
//! * MySQL "databases" are presented as schemas (`Capabilities::schema_label`
//!   is `"Databases"`); [`Session::list_databases`](dbiji_core::Session::list_databases)
//!   returns an empty list.
//!
//! # Connection properties
//!
//! | Property | Meaning |
//! | --- | --- |
//! | `charset`, `collation` | `SET NAMES <charset> [COLLATE <collation>]` (default `utf8mb4`); SQL text and parameters are encoded accordingly |
//! | `compress` | `true`, `false` or a zlib level `0`-`9` |
//! | `tcp_keepalive` | keepalive time in seconds (`0` disables, default 60) |
//! | `tcp_nodelay` | default `true` |
//! | `init_sql` (`init`, `initStatements`) | statements run after connecting, `;` separated |
//! | `time_zone`, `sql_mode`, `transaction_isolation` | session settings |
//! | `max_allowed_packet`, `stmt_cache_size` | client limits |
//! | `socket` | connect through a Unix socket instead of TCP |
//! | `enable_cleartext_plugin` | allow `mysql_clear_password` (LDAP/PAM, use with TLS) |
//! | `client_found_rows` | report matched instead of changed rows |
//! | `program_name` | connection attribute (default: the application name) |
//! | `client_path` | path of the `mysql` client for "execute with local client" |
//!
//! # TLS
//!
//! * `Disable`: plain TCP.
//! * `Prefer`: TLS without certificate verification; falls back to a plain
//!   connection when the server has no TLS support.
//! * `Require`: TLS without certificate verification.
//! * `VerifyCa`: chain verified against `ssl.ca_cert` plus the system and
//!   webpki roots (only `ssl.ca_cert` with `only_custom_ca`); the host name
//!   is not checked (see [`connect::VERIFY_CA_TLS_NAME`] for how this is
//!   achieved with `mysql_async` 0.37).
//! * `VerifyFull`: chain and host name verified against
//!   [`ConnectParams::tls_host`](dbiji_core::ConnectParams::tls_host), so
//!   tunnels (`host` = `127.0.0.1`) verify the real server name.
//!
//! A client certificate and key (PEM: PKCS#1, PKCS#8 or SEC1) enable mutual
//! TLS. `caching_sha2_password` works over TLS (cleartext password inside the
//! TLS channel) and over plain TCP (RSA public key retrieval).
//!
//! # Values
//!
//! Integers → `Int` (`UInt` above `i64::MAX`), `DECIMAL` → `Decimal`,
//! `FLOAT`/`DOUBLE` → `Float`, `DATE`/`DATETIME`/`TIMESTAMP` → chrono values
//! (zero or invalid dates as `Text`), `TIME` → `Time` (negative or ≥ 24 h as
//! `Text`, kind `Time`), `BIT(1)` → `Bool`, `BIT(n)` → `Text` of binary
//! digits, `JSON` → `Json`, `ENUM`/`SET` → `Text` (kind `Enum`), binary
//! strings → `Bytes`, geometry → EWKT `Text` (kind `Geometry`, e.g.
//! `SRID=4326;POINT(1 2)`).
//!
//! # Known limitations
//!
//! * `LOAD DATA LOCAL INFILE` is not supported (the server request closes the
//!   connection, as `mysql_async` has no handler installed).
//! * Statements bound with parameters must not contain `:name` text outside
//!   literals (`mysql_async` treats it as a named parameter); such statements
//!   are rejected before execution.
//! * MariaDB `JSON` columns are `LONGTEXT` on the wire and arrive as `Text`.

// `DbError` (defined by dbiji-core and fixed by the `Driver` / `Session`
// traits) is 144 bytes; boxing it is not possible from a driver crate.
#![allow(clippy::result_large_err)]

pub mod admin;
pub mod connect;
pub mod convert;
pub mod cursor;
pub mod ddl;
pub mod dialect;
pub mod error;
pub mod explain;
pub mod flavor;
pub mod geometry;
pub mod keywords;
pub mod metadata;
pub mod session;
pub mod sqltext;
pub mod tools;

use async_trait::async_trait;
use dbiji_core::admin::{BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::driver::{Capabilities, DriverInfo};
use dbiji_core::meta::ObjectKind;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{ConnectParams, DbKind, DbResult, Dialect, Driver, Session};
use std::path::Path;
use std::sync::Arc;

pub use crate::cursor::MySqlCursor;
pub use crate::ddl::MySqlDdl;
pub use crate::dialect::MySqlDialect;
pub use crate::flavor::{Flavor, ServerProfile};
pub use crate::session::{MySqlCancelHandle, MySqlSession};

/// Capabilities of the MySQL / MariaDB drivers.
pub fn capabilities(flavor: Flavor) -> Capabilities {
    let mut schema_object_kinds = vec![
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::Event,
    ];
    if flavor == Flavor::MariaDb {
        schema_object_kinds.push(ObjectKind::Sequence);
    }
    Capabilities {
        multiple_databases: false,
        schemas: true,
        schema_label: "Databases",
        switch_database: true,
        transactions: true,
        savepoints: true,
        explain: true,
        explain_analyze: true,
        session_manager: true,
        lock_manager: true,
        server_metrics: true,
        backup: true,
        restore: true,
        debugger: false,
        cancel: true,
        returning: false,
        schema_object_kinds,
        table_child_kinds: vec![
            ObjectKind::Column,
            ObjectKind::Index,
            ObjectKind::PrimaryKey,
            ObjectKind::UniqueKey,
            ObjectKind::CheckConstraint,
            ObjectKind::ForeignKey,
            ObjectKind::Trigger,
            ObjectKind::Partition,
        ],
    }
}

/// Shared implementation of both drivers.
struct DriverCore {
    flavor: Flavor,
    info: DriverInfo,
    dialect: Arc<MySqlDialect>,
}

impl DriverCore {
    fn new(flavor: Flavor) -> Self {
        let (name, description) = match flavor {
            Flavor::MySql => (
                "MySQL",
                "MySQL 5.7 and 8.x (also Percona Server and other MySQL compatible servers)",
            ),
            Flavor::MariaDb => ("MariaDB", "MariaDB 10.x and 11.x"),
        };
        DriverCore {
            flavor,
            info: DriverInfo {
                kind: flavor.kind(),
                name,
                description,
                capabilities: capabilities(flavor),
            },
            dialect: Arc::new(MySqlDialect::new(flavor)),
        }
    }

    async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
        Ok(Box::new(MySqlSession::connect(params, self.flavor).await?))
    }
}

macro_rules! impl_driver {
    ($ty:ident, $flavor:expr, $doc:literal) => {
        #[doc = $doc]
        pub struct $ty {
            core: DriverCore,
        }

        impl $ty {
            /// Create the driver.
            pub fn new() -> Self {
                $ty {
                    core: DriverCore::new($flavor),
                }
            }

            /// Open a session and return the concrete session type.
            pub async fn connect_mysql(&self, params: &ConnectParams) -> DbResult<MySqlSession> {
                MySqlSession::connect(params, self.core.flavor).await
            }

            /// The concrete dialect (no server specific information).
            pub fn mysql_dialect(&self) -> Arc<MySqlDialect> {
                self.core.dialect.clone()
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                Self::new()
            }
        }

        #[async_trait]
        impl Driver for $ty {
            fn info(&self) -> &DriverInfo {
                &self.core.info
            }

            fn dialect(&self) -> Arc<dyn Dialect> {
                self.core.dialect.clone()
            }

            async fn connect(&self, params: &ConnectParams) -> DbResult<Box<dyn Session>> {
                self.core.connect(params).await
            }

            fn backup_command(
                &self,
                params: &ConnectParams,
                opts: &BackupOptions,
            ) -> DbResult<CommandSpec> {
                tools::backup_command(self.core.flavor, params, opts)
            }

            fn restore_command(
                &self,
                params: &ConnectParams,
                opts: &RestoreOptions,
            ) -> DbResult<CommandSpec> {
                tools::restore_command(self.core.flavor, params, opts)
            }

            fn client_command(
                &self,
                params: &ConnectParams,
                script: &Path,
            ) -> DbResult<CommandSpec> {
                tools::client_command(self.core.flavor, params, script)
            }
        }
    };
}

impl_driver!(
    MySqlDriver,
    Flavor::MySql,
    "Driver for MySQL servers ([`DbKind::MySql`])."
);
impl_driver!(
    MariaDbDriver,
    Flavor::MariaDb,
    "Driver for MariaDB servers ([`DbKind::MariaDb`])."
);

/// Register both drivers.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(MySqlDriver::new()));
    registry.register(Arc::new(MariaDbDriver::new()));
}

/// Driver for a kind handled by this crate.
pub fn driver_for(kind: DbKind) -> Option<Arc<dyn Driver>> {
    match kind {
        DbKind::MySql => Some(Arc::new(MySqlDriver::new())),
        DbKind::MariaDb => Some(Arc::new(MariaDbDriver::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_info() {
        let my = MySqlDriver::new();
        assert_eq!(my.kind(), DbKind::MySql);
        let caps = &my.info().capabilities;
        assert!(!caps.multiple_databases && caps.schemas && caps.switch_database);
        assert_eq!(caps.schema_label, "Databases");
        assert!(
            caps.savepoints
                && caps.explain_analyze
                && caps.lock_manager
                && caps.backup
                && caps.restore
                && caps.cancel
        );
        assert!(!caps.schema_object_kinds.contains(&ObjectKind::Sequence));
        let ma = MariaDbDriver::new();
        assert_eq!(ma.kind(), DbKind::MariaDb);
        assert!(
            ma.info()
                .capabilities
                .schema_object_kinds
                .contains(&ObjectKind::Sequence)
        );
        assert_eq!(ma.dialect().kind(), DbKind::MariaDb);
        assert_eq!(ma.info().capabilities.table_child_kinds.len(), 8);
        let mut reg = DriverRegistry::new();
        register(&mut reg);
        assert_eq!(reg.kinds(), vec![DbKind::MySql, DbKind::MariaDb]);
        assert!(driver_for(DbKind::Postgres).is_none());
    }
}
