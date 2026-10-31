//! Redis driver for DBiji.
//!
//! Statements are Redis commands, one per line (`GET user:1`,
//! `HGETALL session:9`, `SCAN 0 MATCH order:* COUNT 100`), split and
//! classified by the shell rules of `dbiji-sql`. The driver runs them through
//! the `redis` crate (RESP3) and shapes the reply into a table:
//!
//! * a scalar reply is one row with a `value` column;
//! * a list is one `value` column, a map is `field` / `value`;
//! * `SCAN` and its variants show the cursor;
//! * `ZRANGE … WITHSCORES` and `HGETALL` are pairs.
//!
//! Redis databases are presented as schemas (`Capabilities::schema_label` is
//! `"Databases"`); a key is a table and its fields are its columns.
//!
//! Entry point: [`RedisDriver::new`].

// `DbResult` (the core driver contract) carries a 144 byte `DbError`; boxing
// it is a dbiji-core decision, so the lint is silenced here.
#![allow(clippy::result_large_err)]

pub mod command;
pub mod ddl;
pub mod dialect;
mod driver;
pub mod error;
mod session;
pub mod types;

pub use ddl::RedisDdl;
pub use dialect::RedisDialect;
pub use driver::RedisDriver;
pub use session::RedisSession;
