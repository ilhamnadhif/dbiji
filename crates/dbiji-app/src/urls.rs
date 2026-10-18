//! Connection URL parsing (postgres://, mysql://, jdbc:..., sqlite:...).

use dbiji_core::config::ConnectionConfig;
use dbiji_core::{DbResult, Secret};

/// Parse a connection URL into a configuration and an optional password.
pub fn parse(url: &str) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    dbiji_config::parse_connection_url(url)
}

/// Format a configuration as a URL (without the password).
pub fn format(cfg: &ConnectionConfig) -> String {
    dbiji_config::to_url(cfg)
}
