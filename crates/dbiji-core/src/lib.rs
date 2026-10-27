//! Core of DBiji: shared value model, driver interface, metadata and DDL model,
//! row storage with bounded memory, and small cross-cutting utilities
//! (secrets, localisation).
//!
//! Every database driver implements [`driver::Driver`] and [`driver::Session`];
//! the UI and all tooling crates talk to databases only through these traits.

// `DbError` carries the full server diagnostics (code, detail, hint,
// position, context) and is returned by value everywhere; boxing it would
// cost an allocation per error for no measurable gain.
#![allow(clippy::result_large_err)]

pub mod admin;
pub mod catalog;
pub mod config;
pub mod ddl;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod i18n;
pub mod keywords;
pub mod meta;
pub mod plan;
pub mod registry;
pub mod rowstore;
pub mod secret;
pub mod value;

pub use config::{ConnectionConfig, DbKind};
pub use dialect::{DatabaseEdits, DatabaseProps, Dialect, NewDatabase, SqlSyntax};
pub use driver::{CancelHandle, ConnectParams, Driver, ExecOptions, QueryCursor, ResultHeader, ServerMessage, Session, TransactionMode};
pub use error::{DbError, DbResult, ErrorKind};
pub use i18n::{tr, trf};
pub use meta::{ObjectKind, ObjectRef, SchemaRef};
pub use rowstore::{RowStore, SharedRowStore};
pub use secret::Secret;
pub use value::{ColumnMeta, ColumnSource, DataKind, Row, Value};
