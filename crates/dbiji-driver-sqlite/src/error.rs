//! Mapping of rusqlite / SQLite errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use rusqlite::ffi;
use std::ffi::c_int;

/// Context of a failing call, used to produce precise errors.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ErrContext<'a> {
    /// Complete SQL text passed to `execute` (for error positions).
    pub sql: Option<&'a str>,
    /// Byte offset of the failing statement inside `sql`.
    pub offset: usize,
    /// Busy timeout of the session in milliseconds.
    pub busy_timeout_ms: u64,
    /// The user cancelled the running statement.
    pub cancelled: bool,
    /// The statement timeout elapsed.
    pub timed_out: bool,
}

/// Symbolic name of a (possibly extended) SQLite result code.
pub(crate) fn code_name(code: c_int) -> String {
    let extended = match code {
        ffi::SQLITE_CONSTRAINT_CHECK => Some("SQLITE_CONSTRAINT_CHECK"),
        ffi::SQLITE_CONSTRAINT_COMMITHOOK => Some("SQLITE_CONSTRAINT_COMMITHOOK"),
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => Some("SQLITE_CONSTRAINT_FOREIGNKEY"),
        ffi::SQLITE_CONSTRAINT_FUNCTION => Some("SQLITE_CONSTRAINT_FUNCTION"),
        ffi::SQLITE_CONSTRAINT_NOTNULL => Some("SQLITE_CONSTRAINT_NOTNULL"),
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => Some("SQLITE_CONSTRAINT_PRIMARYKEY"),
        ffi::SQLITE_CONSTRAINT_TRIGGER => Some("SQLITE_CONSTRAINT_TRIGGER"),
        ffi::SQLITE_CONSTRAINT_UNIQUE => Some("SQLITE_CONSTRAINT_UNIQUE"),
        ffi::SQLITE_CONSTRAINT_VTAB => Some("SQLITE_CONSTRAINT_VTAB"),
        ffi::SQLITE_CONSTRAINT_ROWID => Some("SQLITE_CONSTRAINT_ROWID"),
        ffi::SQLITE_CONSTRAINT_PINNED => Some("SQLITE_CONSTRAINT_PINNED"),
        ffi::SQLITE_CONSTRAINT_DATATYPE => Some("SQLITE_CONSTRAINT_DATATYPE"),
        ffi::SQLITE_BUSY_RECOVERY => Some("SQLITE_BUSY_RECOVERY"),
        ffi::SQLITE_BUSY_SNAPSHOT => Some("SQLITE_BUSY_SNAPSHOT"),
        ffi::SQLITE_BUSY_TIMEOUT => Some("SQLITE_BUSY_TIMEOUT"),
        ffi::SQLITE_LOCKED_SHAREDCACHE => Some("SQLITE_LOCKED_SHAREDCACHE"),
        ffi::SQLITE_LOCKED_VTAB => Some("SQLITE_LOCKED_VTAB"),
        ffi::SQLITE_READONLY_RECOVERY => Some("SQLITE_READONLY_RECOVERY"),
        ffi::SQLITE_READONLY_CANTLOCK => Some("SQLITE_READONLY_CANTLOCK"),
        ffi::SQLITE_READONLY_ROLLBACK => Some("SQLITE_READONLY_ROLLBACK"),
        ffi::SQLITE_READONLY_DBMOVED => Some("SQLITE_READONLY_DBMOVED"),
        ffi::SQLITE_READONLY_CANTINIT => Some("SQLITE_READONLY_CANTINIT"),
        ffi::SQLITE_READONLY_DIRECTORY => Some("SQLITE_READONLY_DIRECTORY"),
        ffi::SQLITE_CANTOPEN_NOTEMPDIR => Some("SQLITE_CANTOPEN_NOTEMPDIR"),
        ffi::SQLITE_CANTOPEN_ISDIR => Some("SQLITE_CANTOPEN_ISDIR"),
        ffi::SQLITE_CANTOPEN_FULLPATH => Some("SQLITE_CANTOPEN_FULLPATH"),
        ffi::SQLITE_CANTOPEN_CONVPATH => Some("SQLITE_CANTOPEN_CONVPATH"),
        ffi::SQLITE_CANTOPEN_DIRTYWAL => Some("SQLITE_CANTOPEN_DIRTYWAL"),
        ffi::SQLITE_CANTOPEN_SYMLINK => Some("SQLITE_CANTOPEN_SYMLINK"),
        ffi::SQLITE_ERROR_MISSING_COLLSEQ => Some("SQLITE_ERROR_MISSING_COLLSEQ"),
        ffi::SQLITE_ERROR_RETRY => Some("SQLITE_ERROR_RETRY"),
        ffi::SQLITE_ERROR_SNAPSHOT => Some("SQLITE_ERROR_SNAPSHOT"),
        _ => None,
    };
    if let Some(n) = extended {
        return n.to_string();
    }
    let primary = match code & 0xff {
        ffi::SQLITE_OK => "SQLITE_OK",
        ffi::SQLITE_ERROR => "SQLITE_ERROR",
        ffi::SQLITE_INTERNAL => "SQLITE_INTERNAL",
        ffi::SQLITE_PERM => "SQLITE_PERM",
        ffi::SQLITE_ABORT => "SQLITE_ABORT",
        ffi::SQLITE_BUSY => "SQLITE_BUSY",
        ffi::SQLITE_LOCKED => "SQLITE_LOCKED",
        ffi::SQLITE_NOMEM => "SQLITE_NOMEM",
        ffi::SQLITE_READONLY => "SQLITE_READONLY",
        ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT",
        ffi::SQLITE_IOERR => "SQLITE_IOERR",
        ffi::SQLITE_CORRUPT => "SQLITE_CORRUPT",
        ffi::SQLITE_NOTFOUND => "SQLITE_NOTFOUND",
        ffi::SQLITE_FULL => "SQLITE_FULL",
        ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
        ffi::SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
        ffi::SQLITE_EMPTY => "SQLITE_EMPTY",
        ffi::SQLITE_SCHEMA => "SQLITE_SCHEMA",
        ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG",
        ffi::SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
        ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH",
        ffi::SQLITE_MISUSE => "SQLITE_MISUSE",
        ffi::SQLITE_NOLFS => "SQLITE_NOLFS",
        ffi::SQLITE_AUTH => "SQLITE_AUTH",
        ffi::SQLITE_FORMAT => "SQLITE_FORMAT",
        ffi::SQLITE_RANGE => "SQLITE_RANGE",
        ffi::SQLITE_NOTADB => "SQLITE_NOTADB",
        ffi::SQLITE_NOTICE => "SQLITE_NOTICE",
        ffi::SQLITE_WARNING => "SQLITE_WARNING",
        _ => return format!("SQLITE_{code}"),
    };
    if code > 0xff && code & 0xff == ffi::SQLITE_IOERR {
        // extended I/O error codes carry the operation in the upper bits
        return format!("{primary} ({code})");
    }
    primary.to_string()
}

fn looks_like_syntax_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("syntax error") || m.contains("incomplete input") || m.contains("unrecognized token")
}

/// 1-based character position of byte `offset` inside `sql`.
fn char_position(sql: &str, offset: usize) -> Option<usize> {
    if offset > sql.len() {
        return None;
    }
    let mut end = offset;
    while end > 0 && !sql.is_char_boundary(end) {
        end -= 1;
    }
    Some(sql[..end].chars().count() + 1)
}

/// Map an SQLite primary/extended code plus message to a [`DbError`].
fn from_code(extended: c_int, msg: Option<String>, ctx: &ErrContext<'_>) -> DbError {
    let primary = extended & 0xff;
    let message = msg.unwrap_or_else(|| rusqlite::ffi::code_to_str(extended).to_string());
    let code = code_name(extended);
    let err = match primary {
        ffi::SQLITE_INTERRUPT | ffi::SQLITE_ABORT if ctx.timed_out => {
            DbError::timeout("The statement timeout elapsed; the statement was interrupted")
        }
        ffi::SQLITE_INTERRUPT => DbError::cancelled(),
        ffi::SQLITE_BUSY if ctx.timed_out => {
            DbError::timeout("The statement timeout elapsed while waiting for a database lock")
        }
        ffi::SQLITE_BUSY if ctx.cancelled => DbError::cancelled(),
        ffi::SQLITE_BUSY => DbError::query(format!(
            "The database is locked by another connection ({message}); gave up after the busy timeout of {} ms. \
             Retry later, finish the other connection's transaction, or increase the busy_timeout property.",
            ctx.busy_timeout_ms
        )),
        ffi::SQLITE_LOCKED => DbError::query(format!(
            "A table is locked ({message}); another statement of this connection may still be reading it."
        )),
        ffi::SQLITE_READONLY => DbError::read_only(format!("The database is read-only: {message}")),
        ffi::SQLITE_CANTOPEN
        | ffi::SQLITE_PERM
        | ffi::SQLITE_IOERR
        | ffi::SQLITE_FULL
        | ffi::SQLITE_NOLFS
        | ffi::SQLITE_PROTOCOL => DbError::io(message),
        ffi::SQLITE_NOTADB => {
            DbError::io(format!("The file is not an SQLite database ({message})"))
        }
        ffi::SQLITE_CORRUPT => DbError::io(format!("The database file is corrupt ({message})")),
        ffi::SQLITE_MISUSE | ffi::SQLITE_INTERNAL => DbError::internal(message),
        ffi::SQLITE_ERROR if looks_like_syntax_error(&message) => DbError::syntax(message),
        _ => DbError::query(message),
    };
    err.with_code(code)
}

/// Map a rusqlite error with context.
pub(crate) fn map_error_ctx(e: rusqlite::Error, ctx: &ErrContext<'_>) -> DbError {
    use rusqlite::Error as E;
    match e {
        E::SqliteFailure(err, msg) => from_code(err.extended_code, msg, ctx),
        E::SqlInputError {
            error,
            msg,
            sql: _,
            offset,
        } => {
            let mut err = from_code(error.extended_code, Some(msg), ctx);
            if err.kind == ErrorKind::Query && looks_like_syntax_error(&err.message) {
                err.kind = ErrorKind::Syntax;
            }
            if let (Some(sql), Ok(off)) = (ctx.sql, usize::try_from(offset))
                && let Some(pos) = char_position(sql, ctx.offset + off)
            {
                err = err.with_position(pos);
            }
            err
        }
        E::InvalidParameterCount(given, expected) => DbError::query(format!(
            "The statement expects {expected} parameter(s) but {given} were supplied"
        )),
        E::InvalidParameterName(n) => DbError::query(format!("Unknown parameter name {n}")),
        E::MultipleStatement => DbError::query("Only one statement can be prepared at a time"),
        E::NulError(_) => DbError::query("The SQL text contains a NUL character"),
        E::Utf8Error(..) => DbError::query("Invalid UTF-8 in a text value"),
        E::InvalidPath(p) => DbError::config(format!("Invalid database path: {}", p.display())),
        E::ExecuteReturnedResults => DbError::internal("The statement unexpectedly returned rows"),
        E::QueryReturnedNoRows => DbError::query("The query returned no rows"),
        E::QueryReturnedMoreThanOneRow => DbError::query("The query returned more than one row"),
        E::ToSqlConversionFailure(e) => DbError::query(format!("Cannot bind parameter: {e}")),
        E::UserFunctionError(e) => DbError::query(e.to_string()),
        E::SqliteSingleThreadedMode => {
            DbError::internal("SQLite was compiled in single-threaded mode")
        }
        E::UnwindingPanic => DbError::internal("A user defined SQL function panicked"),
        other => DbError::internal(other.to_string()),
    }
}

/// Map a rusqlite error without statement context.
pub(crate) fn map_error(e: rusqlite::Error) -> DbError {
    map_error_ctx(e, &ErrContext::default())
}

/// Extension to map `rusqlite::Result` into [`dbiji_core::DbResult`].
pub(crate) trait SqliteResultExt<T> {
    fn db(self) -> dbiji_core::DbResult<T>;
}

impl<T> SqliteResultExt<T> for rusqlite::Result<T> {
    fn db(self) -> dbiji_core::DbResult<T> {
        self.map_err(map_error)
    }
}

/// Error returned when the connection thread is gone.
pub(crate) fn worker_gone() -> DbError {
    DbError::connection("The SQLite connection is closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(code_name(ffi::SQLITE_BUSY), "SQLITE_BUSY");
        assert_eq!(
            code_name(ffi::SQLITE_CONSTRAINT_UNIQUE),
            "SQLITE_CONSTRAINT_UNIQUE"
        );
        assert_eq!(code_name(ffi::SQLITE_READONLY), "SQLITE_READONLY");
    }

    #[test]
    fn kinds() {
        let ctx = ErrContext {
            busy_timeout_ms: 250,
            ..Default::default()
        };
        let e = from_code(ffi::SQLITE_BUSY, Some("database is locked".into()), &ctx);
        assert_eq!(e.kind, ErrorKind::Query);
        assert!(e.message.contains("250 ms"), "{}", e.message);
        assert_eq!(
            from_code(ffi::SQLITE_INTERRUPT, None, &ctx).kind,
            ErrorKind::Cancelled
        );
        let t = ErrContext {
            timed_out: true,
            ..ctx
        };
        assert_eq!(
            from_code(ffi::SQLITE_INTERRUPT, None, &t).kind,
            ErrorKind::Timeout
        );
        assert_eq!(
            from_code(ffi::SQLITE_READONLY, None, &ctx).kind,
            ErrorKind::ReadOnly
        );
        assert_eq!(
            from_code(
                ffi::SQLITE_ERROR,
                Some("near \"x\": syntax error".into()),
                &ctx
            )
            .kind,
            ErrorKind::Syntax
        );
        assert_eq!(
            from_code(ffi::SQLITE_ERROR, Some("no such table: t".into()), &ctx).kind,
            ErrorKind::Query
        );
        assert_eq!(
            from_code(ffi::SQLITE_CANTOPEN, None, &ctx).kind,
            ErrorKind::Io
        );
    }

    #[test]
    fn positions() {
        assert_eq!(char_position("héllo", 3), Some(3));
        assert_eq!(char_position("abc", 0), Some(1));
        assert_eq!(char_position("abc", 10), None);
    }
}
