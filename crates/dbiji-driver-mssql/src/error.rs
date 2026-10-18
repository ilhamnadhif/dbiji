//! Mapping of `tiberius` errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use tiberius::error::{Error as TdsError, TokenError};

/// Error kind for a SQL Server error number and severity.
pub fn kind_for(number: u32, class: u8) -> ErrorKind {
    match number {
        // login failures
        18452 | 18456 | 18470 | 18486 | 18487 | 18488 | 4060 | 4064 => ErrorKind::Auth,
        // syntax / parse errors
        102 | 103 | 105 | 107 | 109 | 110 | 111 | 112 | 113 | 117 | 141 | 142 | 153 | 154 | 156 | 170 | 179 | 319 | 1018 | 1038 | 1039 => ErrorKind::Syntax,
        // lock request time out
        1222 => ErrorKind::Timeout,
        // cancelled by the client (attention)
        3617 => ErrorKind::Cancelled,
        // read-only database / replica
        3906 | 3908 | 3910 | 3929 => ErrorKind::ReadOnly,
        _ if class >= 20 => ErrorKind::Connection,
        _ => ErrorKind::Query,
    }
}

/// Convert a server error token. `sql` is the statement text, used to
/// translate the reported line number into a character position.
pub fn token_error(e: &TokenError, sql: Option<&str>) -> DbError {
    let number = e.code();
    let mut err = DbError::new(kind_for(number, e.class()), e.message().to_string()).with_code(number.to_string());
    err = err.with_detail(format!("Msg {}, Level {}, State {}", number, e.class(), e.state()));
    let line = e.line();
    let procedure = e.procedure();
    if !procedure.is_empty() {
        err = err.with_context(format!("Procedure {procedure}, Line {line}"));
    } else if line > 0 {
        err = err.with_context(format!("Line {line}"));
        if let Some(pos) = sql.and_then(|s| crate::sqltext::line_start_position(s, line)) {
            err = err.with_position(pos);
        }
    }
    match number {
        1205 => err.with_hint("The transaction was chosen as a deadlock victim and rolled back; retry it"),
        18456 => err.with_hint("Check the login name, password and that SQL Server authentication is enabled"),
        4060 => err.with_hint("The login has no access to the requested database"),
        2627 | 2601 => err.with_hint("A row with the same key already exists"),
        547 => err.with_hint("The statement conflicts with a foreign key or check constraint"),
        _ => err,
    }
}

/// Convert any tiberius error.
pub fn map_error(e: TdsError, sql: Option<&str>) -> DbError {
    match e {
        TdsError::Server(t) => token_error(&t, sql),
        TdsError::Io { kind, message } => {
            let k = match kind {
                std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
                _ => ErrorKind::Connection,
            };
            DbError::new(k, format!("Network error: {message}"))
        }
        TdsError::Tls(m) => DbError::tls(format!("TLS error: {m}")),
        TdsError::Protocol(m) => {
            if m.contains("encryption") {
                DbError::tls(format!("Encryption negotiation failed: {m}"))
            } else {
                DbError::connection(format!("Protocol error: {m}"))
            }
        }
        TdsError::Routing { host, port } => DbError::connection(format!("The server redirected the connection to {host}:{port}")),
        TdsError::Conversion(m) => DbError::internal(format!("Value conversion error: {m}")),
        TdsError::Encoding(m) => DbError::internal(format!("Encoding error: {m}")),
        TdsError::Utf8 => DbError::internal("Invalid UTF-8 data received"),
        TdsError::Utf16 => DbError::internal("Invalid UTF-16 data received"),
        TdsError::ParseInt(e) => DbError::internal(format!("Invalid number received: {e}")),
        TdsError::BulkInput(m) => DbError::internal(format!("Bulk input error: {m}")),
        #[allow(unreachable_patterns)]
        other => DbError::connection(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_for(18456, 14), ErrorKind::Auth);
        assert_eq!(kind_for(102, 15), ErrorKind::Syntax);
        assert_eq!(kind_for(208, 16), ErrorKind::Query);
        assert_eq!(kind_for(1222, 16), ErrorKind::Timeout);
        assert_eq!(kind_for(9001, 21), ErrorKind::Connection);
        assert_eq!(kind_for(3906, 16), ErrorKind::ReadOnly);
    }

    #[test]
    fn io_and_protocol() {
        let e = map_error(TdsError::Io { kind: std::io::ErrorKind::ConnectionReset, message: "reset".into() }, None);
        assert_eq!(e.kind, ErrorKind::Connection);
        assert!(e.is_connection_lost());
        let e = map_error(TdsError::Protocol("Server does not allow the requested encryption level.".into()), None);
        assert_eq!(e.kind, ErrorKind::Tls);
        let e = map_error(TdsError::Io { kind: std::io::ErrorKind::TimedOut, message: "t".into() }, None);
        assert_eq!(e.kind, ErrorKind::Timeout);
    }
}
