//! Small helpers turning errors of the file format crates into [`DbError`]s.

use dbiji_core::DbError;
use std::fmt::Display;
use std::path::Path;

/// I/O failure related to a file.
pub(crate) fn file_error(action: &str, path: &Path, err: impl Display) -> DbError {
    DbError::io(format!("{action} {}: {err}", path.display()))
}

/// Failure of a file format library (XLSX, Parquet, XML ...).
pub(crate) fn format_error(format: &str, err: impl Display) -> DbError {
    DbError::io(format!("{format} error: {err}"))
}

/// Malformed input file.
pub(crate) fn parse_error(format: &str, location: impl Display, err: impl Display) -> DbError {
    DbError::config(format!("Invalid {format} input at {location}: {err}"))
}

/// Adds the item name to an error message so multi-object jobs report which item failed.
pub(crate) fn with_item(err: DbError, item: &str) -> DbError {
    let context = match &err.context {
        Some(c) => format!("{item}: {c}"),
        None => item.to_string(),
    };
    err.with_context(context)
}
