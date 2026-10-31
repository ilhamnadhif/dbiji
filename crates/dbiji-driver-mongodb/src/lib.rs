//! MongoDB driver for DBiji.
//!
//! Statements are written in the MongoDB shell language — `db.users.find({…})`,
//! `db.orders.aggregate([…])`, `db.t.insertOne({…})`, `show collections`,
//! `use shop` — and parsed by [`shell`] into server commands. The driver uses
//! the official `mongodb` crate (its own connection pool, TLS through rustls).
//!
//! * Results are documents: one column per top-level field (the union of the
//!   fields of the first documents), nested values as JSON text.
//! * `find` and `aggregate` stream through a cursor; the columns come from a
//!   sample of the first documents.
//! * Databases are shown as schemas (as MySQL does), collections as tables,
//!   indexes and the fields sampled from the documents as their children.
//! * There are no SQL transactions: every statement commits on its own.
//!
//! Entry point: [`MongoDriver::new`].

// `DbResult` (the core driver contract) carries a 144 byte `DbError`; boxing
// it is a dbiji-core decision, so the lint is silenced here.
#![allow(clippy::result_large_err)]

mod ddl;
pub mod dialect;
mod driver;
pub mod error;
pub mod js;
mod session;
pub mod shell;
pub mod types;

pub use ddl::MongoDdl;
pub use dialect::MongoDialect;
pub use driver::MongoDriver;
pub use session::MongoSession;
