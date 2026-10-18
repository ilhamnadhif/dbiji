//! Conversion of `mysql_async` errors into [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use mysql_async::{DriverError, IoError};

/// MySQL / MariaDB error numbers the driver treats specially.
pub mod codes {
    /// ER_DBACCESS_DENIED_ERROR
    pub const DB_ACCESS_DENIED: u16 = 1044;
    /// ER_ACCESS_DENIED_ERROR
    pub const ACCESS_DENIED: u16 = 1045;
    /// ER_PARSE_ERROR
    pub const PARSE_ERROR: u16 = 1064;
    /// ER_SYNTAX_ERROR
    pub const SYNTAX_ERROR: u16 = 1149;
    /// ER_NO_SUCH_THREAD (KILL of an unknown connection id)
    pub const NO_SUCH_THREAD: u16 = 1094;
    /// ER_LOCK_DEADLOCK (the whole transaction was rolled back)
    pub const DEADLOCK: u16 = 1213;
    /// ER_NOT_SUPPORTED_AUTH_MODE
    pub const NOT_SUPPORTED_AUTH_MODE: u16 = 1251;
    /// ER_OPTION_PREVENTS_STATEMENT (`--read-only`)
    pub const OPTION_PREVENTS_STATEMENT: u16 = 1290;
    /// ER_QUERY_INTERRUPTED
    pub const QUERY_INTERRUPTED: u16 = 1317;
    /// ER_ACCESS_DENIED_NO_PASSWORD_ERROR
    pub const ACCESS_DENIED_NO_PASSWORD: u16 = 1698;
    /// ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION
    pub const READ_ONLY_TRANSACTION: u16 = 1792;
    /// ER_MUST_CHANGE_PASSWORD
    pub const MUST_CHANGE_PASSWORD: u16 = 1820;
    /// ER_READ_ONLY_MODE
    pub const READ_ONLY_MODE: u16 = 1836;
    /// ER_MUST_CHANGE_PASSWORD_LOGIN
    pub const MUST_CHANGE_PASSWORD_LOGIN: u16 = 1862;
    /// MariaDB ER_STATEMENT_TIMEOUT (max_statement_time exceeded)
    pub const MARIADB_STATEMENT_TIMEOUT: u16 = 1969;
    /// ER_QUERY_TIMEOUT (MAX_EXECUTION_TIME exceeded)
    pub const QUERY_TIMEOUT: u16 = 3024;
    /// ER_CON_COUNT_ERROR (too many connections)
    pub const TOO_MANY_CONNECTIONS: u16 = 1040;
    /// ER_SERVER_SHUTDOWN
    pub const SERVER_SHUTDOWN: u16 = 1053;
    /// ER_CONNECTION_KILLED (MariaDB 1927)
    pub const CONNECTION_KILLED: u16 = 1927;
    /// ER_CLIENT_INTERACTION_TIMEOUT (MySQL 8.0.24+)
    pub const CLIENT_INTERACTION_TIMEOUT: u16 = 4031;
}

/// Error kind for a server error number.
pub fn kind_for_code(code: u16) -> ErrorKind {
    use codes::*;
    match code {
        ACCESS_DENIED
        | DB_ACCESS_DENIED
        | ACCESS_DENIED_NO_PASSWORD
        | NOT_SUPPORTED_AUTH_MODE
        | MUST_CHANGE_PASSWORD
        | MUST_CHANGE_PASSWORD_LOGIN => ErrorKind::Auth,
        PARSE_ERROR | SYNTAX_ERROR => ErrorKind::Syntax,
        QUERY_INTERRUPTED | QUERY_TIMEOUT | MARIADB_STATEMENT_TIMEOUT => ErrorKind::Cancelled,
        OPTION_PREVENTS_STATEMENT | READ_ONLY_TRANSACTION | READ_ONLY_MODE => ErrorKind::ReadOnly,
        TOO_MANY_CONNECTIONS | SERVER_SHUTDOWN | CONNECTION_KILLED | CLIENT_INTERACTION_TIMEOUT => {
            ErrorKind::Connection
        }
        _ => ErrorKind::Query,
    }
}

/// Server error number of a `mysql_async` error, if it came from the server.
pub fn server_code(err: &mysql_async::Error) -> Option<u16> {
    match err {
        mysql_async::Error::Server(e) => Some(e.code),
        _ => None,
    }
}

/// Convert a `mysql_async` error. `sql` is the statement that failed (used to
/// compute the error position of syntax errors), if any.
pub fn map_error(err: mysql_async::Error, sql: Option<&str>) -> DbError {
    match err {
        mysql_async::Error::Server(e) => {
            let kind = kind_for_code(e.code);
            let mut out = DbError::new(kind, e.message.clone()).with_code(e.code.to_string());
            if !e.state.is_empty() {
                out = out.with_detail(format!("SQLSTATE {}", e.state));
            }
            if kind == ErrorKind::Syntax
                && let Some(pos) = sql.and_then(|s| syntax_error_position(s, &e.message))
            {
                out = out.with_position(pos);
            }
            out
        }
        mysql_async::Error::Driver(d) => map_driver_error(d),
        mysql_async::Error::Io(IoError::Io(e)) => {
            // tokio-rustls reports handshake failures (certificate verification
            // ...) as I/O errors wrapping the rustls error.
            if e.get_ref()
                .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
            {
                return DbError::tls(format!("TLS error: {e}"));
            }
            let kind = if e.kind() == std::io::ErrorKind::TimedOut {
                ErrorKind::Timeout
            } else {
                ErrorKind::Connection
            };
            DbError::new(kind, format!("Connection error: {e}"))
        }
        mysql_async::Error::Io(IoError::Tls(e)) => DbError::tls(format!("TLS error: {e}")),
        mysql_async::Error::Other(e) => DbError::internal(format!("MySQL client error: {e}")),
        mysql_async::Error::Url(e) => DbError::config(format!("Invalid connection options: {e}")),
    }
}

fn map_driver_error(d: DriverError) -> DbError {
    match d {
        DriverError::ConnectionClosed => {
            DbError::connection("The connection to the server is closed")
        }
        DriverError::NoClientSslFlagFromServer => {
            DbError::tls("TLS was requested but the server does not support TLS connections")
        }
        DriverError::UnknownAuthPlugin { name } => DbError::auth(format!(
            "The server requested the unsupported authentication plugin '{name}'"
        )),
        DriverError::MysqlOldPasswordDisabled => DbError::auth(
            "The server requested the insecure mysql_old_password authentication, which is disabled",
        ),
        DriverError::CleartextPluginDisabled => DbError::auth(
            "The server requested cleartext password authentication; enable the 'enable_cleartext_plugin' \
             connection property (only over TLS)",
        ),
        DriverError::InvalidParsecSalt => {
            DbError::auth("Invalid parsec authentication data received from the server")
        }
        DriverError::MixedParams => DbError::query(
            "The statement mixes positional (?) parameters with named (:name) parameters; \
             only '?' placeholders are supported when parameters are bound",
        ),
        DriverError::StmtParamsMismatch { required, supplied } => DbError::query(format!(
            "The statement expects {required} parameter(s) but {supplied} were supplied"
        )),
        DriverError::StmtParamsNumberExceedsLimit { supplied } => DbError::query(format!(
            "Too many parameters ({supplied}); MySQL supports at most 65535"
        )),
        DriverError::Params(e) => DbError::query(format!("Invalid statement parameters: {e}")),
        DriverError::LocalInfile(e) => DbError::query(format!(
            "LOAD DATA LOCAL INFILE is not supported by this client ({e})"
        )),
        DriverError::NoKeyFound => {
            DbError::config("No private key found in the TLS client key file")
        }
        DriverError::PacketOutOfOrder
        | DriverError::UnexpectedPacket { .. }
        | DriverError::PacketTooLarge
        | DriverError::BadCompressedPacketHeader => DbError::connection(format!(
            "Protocol error, the connection is no longer usable: {d}"
        )),
        DriverError::PoolDisconnected => DbError::connection("The connection pool was closed"),
        other => DbError::internal(format!("MySQL client error: {other}")),
    }
}

/// Is the connection unusable after this error?
pub fn is_fatal(err: &mysql_async::Error) -> bool {
    match err {
        mysql_async::Error::Server(e) => matches!(
            e.code,
            codes::SERVER_SHUTDOWN | codes::CONNECTION_KILLED | codes::CLIENT_INTERACTION_TIMEOUT
        ),
        mysql_async::Error::Driver(
            DriverError::MixedParams
            | DriverError::StmtParamsMismatch { .. }
            | DriverError::StmtParamsNumberExceedsLimit { .. }
            | DriverError::Params(_)
            | DriverError::FromValue { .. }
            | DriverError::FromRow { .. },
        ) => false,
        other => other.is_fatal(),
    }
}

/// 1-based character position of a syntax error inside `sql`, computed from
/// the `... near '<text>' at line <n>` part of MySQL / MariaDB messages.
pub fn syntax_error_position(sql: &str, message: &str) -> Option<usize> {
    let near = message.find("near '")? + "near '".len();
    let tail = &message[near..];
    let at_line = tail.rfind("' at line ")?;
    let snippet = &tail[..at_line];
    let line: usize = tail[at_line + "' at line ".len()..]
        .trim()
        .trim_end_matches('.')
        .parse()
        .ok()?;
    // Byte offset where the reported line starts.
    let mut line_start = 0usize;
    if line > 1 {
        let mut seen = 1usize;
        for (i, c) in sql.char_indices() {
            if c == '\n' {
                seen += 1;
                if seen == line {
                    line_start = i + 1;
                    break;
                }
            }
        }
        if seen < line {
            return None;
        }
    }
    let byte_pos = if snippet.is_empty() {
        sql.trim_end().len()
    } else {
        // The server truncates the snippet (about 80 characters); search a prefix.
        let probe: String = snippet.chars().take(40).collect();
        sql[line_start..]
            .find(probe.as_str())
            .map(|p| line_start + p)
            .or_else(|| sql.find(probe.as_str()))?
    };
    Some(sql[..byte_pos].chars().count() + 1)
}

/// Shorthand: map an error without statement context.
pub fn err(e: mysql_async::Error) -> DbError {
    map_error(e, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::ServerError;

    fn server(code: u16, state: &str, msg: &str) -> mysql_async::Error {
        mysql_async::Error::Server(ServerError {
            code,
            message: msg.into(),
            state: state.into(),
        })
    }

    #[test]
    fn kinds() {
        assert_eq!(
            map_error(server(1045, "28000", "Access denied"), None).kind,
            ErrorKind::Auth
        );
        assert_eq!(
            map_error(server(1317, "70100", "interrupted"), None).kind,
            ErrorKind::Cancelled
        );
        assert_eq!(
            map_error(server(3024, "HY000", "timeout"), None).kind,
            ErrorKind::Cancelled
        );
        assert_eq!(
            map_error(server(1290, "HY000", "read only"), None).kind,
            ErrorKind::ReadOnly
        );
        assert_eq!(
            map_error(server(1792, "25006", "read only tx"), None).kind,
            ErrorKind::ReadOnly
        );
        assert_eq!(
            map_error(server(1146, "42S02", "no table"), None).kind,
            ErrorKind::Query
        );
        let e = map_error(server(1146, "42S02", "Table 'x.y' doesn't exist"), None);
        assert_eq!(e.code.as_deref(), Some("1146"));
        assert_eq!(e.detail.as_deref(), Some("SQLSTATE 42S02"));
        assert_eq!(
            map_error(
                mysql_async::Error::Driver(DriverError::ConnectionClosed),
                None
            )
            .kind,
            ErrorKind::Connection
        );
        assert_eq!(
            map_error(
                mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer),
                None
            )
            .kind,
            ErrorKind::Tls
        );
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(
            map_error(mysql_async::Error::Io(IoError::Io(io)), None).kind,
            ErrorKind::Connection
        );
    }

    #[test]
    fn syntax_position() {
        let sql = "SELECT * FORM t";
        let msg = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'FORM t' at line 1";
        assert_eq!(syntax_error_position(sql, msg), Some(10));
        let e = map_error(server(1064, "42000", msg), Some(sql));
        assert_eq!(e.kind, ErrorKind::Syntax);
        assert_eq!(e.position, Some(10));

        let sql2 = "SELECT 1;\nSELECT * FORM t";
        assert_eq!(
            syntax_error_position(sql2, &msg.replace("line 1", "line 2")),
            Some(20)
        );
        // Empty snippet: error at the end of the statement.
        let sql3 = "SELECT * FROM";
        let msg3 = "You have an error in your SQL syntax; check the manual ... near '' at line 1";
        assert_eq!(syntax_error_position(sql3, msg3), Some(14));
        assert_eq!(syntax_error_position("x", "Unknown column"), None);
    }

    #[test]
    fn fatality() {
        assert!(!is_fatal(&server(1064, "42000", "x")));
        assert!(is_fatal(&mysql_async::Error::Driver(
            DriverError::ConnectionClosed
        )));
        assert!(!is_fatal(&mysql_async::Error::Driver(
            DriverError::MixedParams
        )));
    }
}
