//! Mapping of tokio-postgres errors into [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use std::error::Error as _;
use tokio_postgres::error::{DbError as PgDbError, ErrorPosition};

/// Error category for a SQLSTATE code.
pub(crate) fn kind_for_sqlstate(code: &str, message: &str) -> ErrorKind {
    match code {
        "42601" => ErrorKind::Syntax,
        "57014" => {
            if message.contains("statement timeout") {
                ErrorKind::Timeout
            } else {
                ErrorKind::Cancelled
            }
        }
        "55P03" if message.contains("lock timeout") => ErrorKind::Timeout,
        "25006" => ErrorKind::ReadOnly,
        // admin_shutdown, crash_shutdown, cannot_connect_now: the session is gone
        "57P01" | "57P02" | "57P03" => ErrorKind::Connection,
        c if c.starts_with("08") => ErrorKind::Connection,
        c if c.starts_with("28") => ErrorKind::Auth,
        _ => ErrorKind::Query,
    }
}

/// Convert a server error (ErrorResponse) into a [`DbError`] with all details.
pub(crate) fn from_db_error(db: &PgDbError) -> DbError {
    let code = db.code().code();
    let mut e = DbError::new(kind_for_sqlstate(code, db.message()), db.message()).with_code(code);
    if let Some(d) = db.detail() {
        e = e.with_detail(d);
    }
    if let Some(h) = db.hint() {
        e = e.with_hint(h);
    }
    let mut context = db.where_().map(str::to_string);
    match db.position() {
        Some(ErrorPosition::Original(p)) => e = e.with_position(*p as usize),
        Some(ErrorPosition::Internal { position, query }) => {
            let internal = format!("internal query: {query} (at position {position})");
            context = Some(match context {
                Some(c) => format!("{c}\n{internal}"),
                None => internal,
            });
        }
        None => {}
    }
    let mut extra = Vec::new();
    if let Some(t) = db.table() {
        match db.schema() {
            Some(s) => extra.push(format!("table {s}.{t}")),
            None => extra.push(format!("table {t}")),
        }
    }
    if let Some(c) = db.column() {
        extra.push(format!("column {c}"));
    }
    if let Some(c) = db.constraint() {
        extra.push(format!("constraint {c}"));
    }
    if !extra.is_empty() {
        let s = extra.join(", ");
        context = Some(match context {
            Some(c) => format!("{c}\n{s}"),
            None => s,
        });
    }
    if let Some(c) = context {
        e = e.with_context(c);
    }
    e
}

/// Convert any tokio-postgres error.
pub(crate) fn map_error(err: &tokio_postgres::Error) -> DbError {
    if let Some(db) = err.as_db_error() {
        return from_db_error(db);
    }
    if err.is_closed() {
        return DbError::connection("The connection to the server was closed");
    }
    let text = err.to_string();
    if text.starts_with("error performing TLS handshake") {
        let detail = err.source().map(|s| s.to_string());
        let mut e = DbError::tls(text);
        e.detail = detail;
        return e;
    }
    if let Some(io) = err.source().and_then(|s| s.downcast_ref::<std::io::Error>()) {
        let mut e = DbError::from(std::io::Error::new(io.kind(), io.to_string()));
        e.message = text;
        if e.kind == ErrorKind::Io {
            e.kind = ErrorKind::Connection;
        }
        return e;
    }
    let kind = if text.starts_with("error performing TLS handshake") {
        ErrorKind::Tls
    } else if text.starts_with("authentication error") {
        ErrorKind::Auth
    } else if text.starts_with("invalid configuration") || text.starts_with("invalid connection string") {
        if text.contains("password missing") { ErrorKind::Auth } else { ErrorKind::Config }
    } else if text.starts_with("timeout waiting for server") {
        ErrorKind::Timeout
    } else if text.starts_with("error connecting to server")
        || text.starts_with("error communicating with the server")
        || text.starts_with("unexpected message from server")
        || text.starts_with("error parsing response from server")
    {
        ErrorKind::Connection
    } else if text.starts_with("error serializing parameter") {
        ErrorKind::Query
    } else {
        ErrorKind::Internal
    };
    DbError::new(kind, text)
}

/// Shorthand used with `map_err`.
pub(crate) fn pg(err: tokio_postgres::Error) -> DbError {
    map_error(&err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstate_classes() {
        assert_eq!(kind_for_sqlstate("08006", ""), ErrorKind::Connection);
        assert_eq!(kind_for_sqlstate("28P01", ""), ErrorKind::Auth);
        assert_eq!(kind_for_sqlstate("42601", ""), ErrorKind::Syntax);
        assert_eq!(kind_for_sqlstate("57014", "canceling statement due to user request"), ErrorKind::Cancelled);
        assert_eq!(kind_for_sqlstate("57014", "canceling statement due to statement timeout"), ErrorKind::Timeout);
        assert_eq!(kind_for_sqlstate("25006", ""), ErrorKind::ReadOnly);
        assert_eq!(kind_for_sqlstate("57P01", ""), ErrorKind::Connection);
        assert_eq!(kind_for_sqlstate("23505", ""), ErrorKind::Query);
        assert_eq!(kind_for_sqlstate("42P01", ""), ErrorKind::Query);
    }
}
