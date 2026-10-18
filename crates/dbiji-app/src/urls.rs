//! Connection URL parsing (postgres://, mysql://, jdbc:..., sqlite:...).

use dbiji_core::config::ConnectionConfig;
use dbiji_core::{DbError, DbResult, Secret};

pub fn parse(url: &str) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    let _ = url;
    Err(DbError::unsupported("URL parsing is not available yet"))
}
