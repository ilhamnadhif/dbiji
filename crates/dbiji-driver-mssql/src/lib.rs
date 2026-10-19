//! Microsoft SQL Server driver for DBiji (TDS protocol through `tiberius`).
//!
//! * Statements run in a tokio task that owns the connection, streaming
//!   rows through a bounded channel to a `'static` cursor; the connection
//!   returns to the session when the cursor finishes, is closed or dropped.
//! * Cancellation sends a TDS attention signal; if the connection cannot be
//!   resynchronised it is replaced transparently (see `cursor`).
//! * Plain statements run as SQL batches (so `USE`, `SET` and `#temp` tables
//!   persist); parameterised statements and plain DML go through
//!   `sp_executesql`, which also reports per-statement row counts.
//! * Metadata comes from the catalog views of any database on the server.
//!
//! Entry point: [`SqlServerDriver::new`].

// `DbResult` (the core driver contract) carries a 144 byte `DbError`; boxing
// it is a dbiji-core decision, so the lint is silenced here.
#![allow(clippy::result_large_err)]

mod admin;
mod catalog;
mod catalogs;
pub mod connect;
mod cursor;
pub mod ddl;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod keywords;
pub mod plan;
pub mod session;
pub mod sqltext;
pub mod types;

pub use ddl::MsSqlDdl;
pub use dialect::MsSqlDialect;
pub use driver::SqlServerDriver;
pub use session::{SqlServerCancelHandle, SqlServerSession};
