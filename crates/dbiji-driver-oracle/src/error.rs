//! Mapping of `oracledb` errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use oracledb::ErrorKind as OraKind;

/// ORA codes of a lost or refused connection.
const CONNECTION_CODES: &[usize] = &[
    28, 1012, 1033, 1034, 1089, 1090, 1092, 3113, 3114, 3135, 12170, 12514, 12505, 12520, 12521,
    12528, 12537, 12541, 12543, 12545,
];

/// ORA codes of a failed log-on (wrong password, locked or expired account).
const AUTH_CODES: &[usize] = &[1004, 1005, 1017, 1045, 28000, 28001, 28009, 28040];

pub(crate) fn map(e: oracledb::Error) -> DbError {
    match e.kind() {
        OraKind::DbError(db) => {
            let code = db.code();
            let kind = if code == 1013 {
                ErrorKind::Cancelled
            } else if AUTH_CODES.contains(&code) {
                ErrorKind::Auth
            } else if CONNECTION_CODES.contains(&code) {
                ErrorKind::Connection
            } else if (900..=999).contains(&code) && code != 942 || code == 1756 || code == 6550 {
                // ORA-009xx: invalid SQL, identifier, missing keyword ... (not "does not exist");
                // ORA-01756 unterminated string; ORA-06550 PL/SQL compilation error.
                ErrorKind::Syntax
            } else {
                ErrorKind::Query
            };
            let message = first_line(db.message());
            let mut err = DbError::new(kind, message).with_code(format!("ORA-{code:05}"));
            let detail = db
                .message()
                .lines()
                .skip(1)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !detail.is_empty() {
                err = err.with_detail(detail);
            }
            if db.offset() > 0 {
                err = err.with_position(db.offset() + 1);
            }
            err
        }
        OraKind::CallTimeoutExceeded => DbError::timeout(e.to_string()),
        OraKind::DeadConnection | OraKind::NotConnected => DbError::connection(e.to_string()),
        OraKind::ListenerRefusedConnection(..)
        | OraKind::InvalidServiceName(..)
        | OraKind::InvalidSid(..)
        | OraKind::InvalidConnectString(..)
        | OraKind::InvalidRedirect(..) => DbError::connection(e.to_string()),
        OraKind::NoCredentials => DbError::auth(e.to_string()),
        _ => {
            let text = e.to_string();
            // Socket and TLS failures come through as I/O errors.
            let lower = text.to_ascii_lowercase();
            if lower.contains("connection")
                && (lower.contains("refused")
                    || lower.contains("reset")
                    || lower.contains("closed")
                    || lower.contains("timed out"))
                || lower.contains("broken pipe")
            {
                DbError::connection(text)
            } else if lower.contains("certificate") || lower.contains("tls") {
                DbError::tls(text)
            } else {
                DbError::query(text)
            }
        }
    }
}

/// The first line of a server message (Oracle appends "Help: https://..." lines).
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or(msg).trim().to_string()
}

pub(crate) trait OraResultExt<T> {
    fn db(self) -> dbiji_core::DbResult<T>;
}

impl<T> OraResultExt<T> for Result<T, oracledb::Error> {
    fn db(self) -> dbiji_core::DbResult<T> {
        self.map_err(map)
    }
}
