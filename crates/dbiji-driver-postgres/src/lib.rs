//! PostgreSQL and CockroachDB driver for DBiji.
//!
//! * [`PostgresDriver`] / [`CockroachDriver`] implement [`dbiji_core::Driver`];
//!   register them with [`register`].
//! * Connections use tokio-postgres over our own TCP / Unix socket stream
//!   with rustls (aws-lc-rs) for every SSL mode, so TLS is verified against
//!   the real server name even through SSH tunnels.
//! * Statements without parameters are described with `Parse`/`Describe`
//!   (column types, base table and column) and streamed with the simple
//!   query protocol in text format; statements with parameters use the
//!   extended protocol with TEXT encoded parameters. Rows are streamed with
//!   backpressure, so memory stays bounded for any result size.
//! * Metadata comes from `pg_catalog` with batched queries; DDL generation
//!   ([`PgDdl`]), the SQL dialect ([`PgDialect`]), EXPLAIN parsing, the
//!   session / lock manager, server metrics, pg_dump / pg_restore / psql
//!   command lines and a PL/pgSQL debugger (pldbgapi) complete the driver.

// `dbiji_core::DbError` (the error type of every driver API) is large by
// design (code, detail, hint, position, context); boxing it is a core change.
#![allow(clippy::result_large_err)]

mod admin;
mod catalog;
mod connect;
mod cursor;
mod ddl;
mod debugger;
mod decode;
mod dialect;
mod driver;
mod error;
mod ewkb;
mod explain;
mod keywords;
mod meta;
mod params;
mod session;
mod sqltext;
mod tabledef;
mod tls;
mod tools;
mod types;

pub use ddl::PgDdl;
pub use dialect::{PgDialect, syntax};
pub use driver::{CockroachDriver, PostgresDriver};
pub use session::PgSession;

use std::sync::Arc;

/// Register the PostgreSQL and CockroachDB drivers.
pub fn register(registry: &mut dbiji_core::registry::DriverRegistry) {
    registry.register(Arc::new(PostgresDriver::new()));
    registry.register(Arc::new(CockroachDriver::new()));
}

/// Convert hex encoded or binary EWKB (PostGIS) into EWKT text such as
/// `SRID=4326;POINT(1 2)`. Returns `None` for invalid input.
pub fn ewkb_to_ewkt(ewkb: &[u8]) -> Option<String> {
    ewkb::ewkb_to_ewkt(ewkb).ok()
}
