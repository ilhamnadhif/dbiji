//! ClickHouse driver for DBiji, using the ClickHouse HTTP(S) interface.
//!
//! * Results are requested as `JSONCompactEachRowWithNamesAndTypes` and
//!   streamed line by line from the response body on a dedicated thread,
//!   with bounded memory and backpressure.
//! * Every session owns a ClickHouse HTTP session (`session_id`), so `SET`,
//!   `USE` and temporary tables persist between statements.
//! * Statements are cancelled with `KILL QUERY` using a per-statement
//!   `query_id`.
//! * ClickHouse has no transactions and no binding protocol: positional `?`
//!   parameters are substituted client side with dialect literals.
//!
//! Entry point: [`ClickHouseDriver::new`].

// `DbResult` (the core driver contract) carries a 144 byte `DbError`; boxing
// it is a dbiji-core decision, so the lint is silenced here.
#![allow(clippy::result_large_err)]

mod catalog;
mod catalogs;
mod cursor;
pub mod ddl;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod http;
pub mod json;
pub mod keywords;
pub mod params;
pub mod plan;
pub mod session;
pub mod stream;
pub mod types;

pub use ddl::ClickHouseDdl;
pub use dialect::ClickHouseDialect;
pub use driver::ClickHouseDriver;
pub use session::ClickHouseSession;
