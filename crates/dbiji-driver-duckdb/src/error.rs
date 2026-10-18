//! Mapping of duckdb-rs errors to [`DbError`].
//!
//! DuckDB reports every failure with the same C API state; the kind is
//! encoded in the message prefix (`Parser Error:`, `Catalog Error:`,
//! `Interrupt Error:` ...), and parser errors carry a `LINE n:` excerpt with a
//! caret under the failing token.

use dbiji_core::{DbError, DbResult};

/// Context of a failing call.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ErrContext<'a> {
    /// Statement text (for error positions).
    pub statement: Option<&'a str>,
    /// Characters of the script before the statement.
    pub chars_before: usize,
    /// The statement timeout elapsed (the watchdog interrupted the query).
    pub timed_out: bool,
}

/// `Parser Error` → `PARSER`, the code reported in [`DbError::code`].
fn error_type(message: &str) -> Option<&str> {
    let (head, _) = message.split_once(" Error:")?;
    (!head.is_empty()
        && head.len() <= 32
        && head.chars().all(|c| c.is_ascii_alphabetic() || c == ' '))
    .then_some(head)
}

/// Position from the `LINE n: <excerpt>` / caret lines of a parser error.
fn caret_position(message: &str, statement: &str) -> Option<usize> {
    let mut lines = message.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("LINE ") else {
            continue;
        };
        let (num, excerpt) = rest.split_once(": ")?;
        let line_no: usize = num.trim().parse().ok()?;
        let caret_line = lines.next()?;
        let caret = caret_line.find('^')?;
        let prefix = line.len() - excerpt.len();
        let column = caret.checked_sub(prefix)?;
        // DuckDB shortens long lines with "..." at the start of the excerpt
        let source_line = statement.lines().nth(line_no.checked_sub(1)?)?;
        let excerpt_start = if let Some(e) = excerpt.strip_prefix("...") {
            let probe: String = e.chars().take(16).collect();
            source_line.find(&probe)?.checked_sub(3)?
        } else {
            0
        };
        let before: usize = statement
            .lines()
            .take(line_no - 1)
            .map(|l| l.chars().count() + 1)
            .sum();
        let in_line = source_line
            .get(..excerpt_start)
            .map(|s| s.chars().count())
            .unwrap_or(0)
            + column;
        return Some(before + in_line + 1);
    }
    None
}

/// Map a DuckDB message to a [`DbError`].
pub(crate) fn from_message(message: &str, ctx: &ErrContext<'_>) -> DbError {
    let kind = error_type(message).map(str::to_ascii_uppercase);
    // the first line is the message; the rest is context (LINE / caret)
    let first = message.lines().next().unwrap_or(message).trim().to_string();
    let lower = message.to_ascii_lowercase();
    let err = match kind.as_deref() {
        Some("INTERRUPT") if ctx.timed_out => {
            DbError::timeout("The statement timeout elapsed; the query was interrupted")
        }
        Some("INTERRUPT") => DbError::cancelled(),
        Some("PARSER") => DbError::syntax(first.clone()),
        Some("IO") | Some("PERMISSION") => DbError::io(first.clone()),
        _ if lower.contains("read-only mode") || lower.contains("read only mode") => {
            DbError::read_only(first.clone())
        }
        Some("INTERNAL") | Some("FATAL") => DbError::internal(first.clone()),
        Some("CONNECTION") => DbError::connection(first.clone()),
        Some("OUT OF MEMORY") => DbError::query(first.clone()),
        _ => DbError::query(first.clone()),
    };
    let mut err = match kind {
        Some(k) => err.with_code(k.replace(' ', "_")),
        None => err,
    };
    let rest: Vec<&str> = message
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if !rest.is_empty() {
        err = err.with_detail(rest.join("\n"));
    }
    if let Some(stmt) = ctx.statement
        && let Some(p) = caret_position(message, stmt)
    {
        err = err.with_position(ctx.chars_before + p);
    }
    err
}

/// Map a duckdb-rs error with context.
pub(crate) fn map_error_ctx(e: duckdb::Error, ctx: &ErrContext<'_>) -> DbError {
    use duckdb::Error as E;
    match e {
        E::DuckDBFailure(_, Some(msg)) => from_message(&msg, ctx),
        E::DuckDBFailure(code, None) => DbError::query(format!("DuckDB error ({code})")),
        E::InvalidParameterCount(given, expected) => DbError::query(format!(
            "The statement expects {expected} parameter(s) but {given} were supplied"
        )),
        E::InvalidParameterName(n) => DbError::query(format!("Unknown parameter name {n}")),
        E::NulError(_) => DbError::query("The SQL text contains a NUL character"),
        E::InvalidPath(p) => DbError::config(format!("Invalid database path: {}", p.display())),
        E::ToSqlConversionFailure(e) => DbError::query(format!("Cannot bind parameter: {e}")),
        E::QueryReturnedNoRows => DbError::query("The query returned no rows"),
        E::FromSqlConversionFailure(idx, ty, err) => {
            let column = if idx == usize::MAX {
                String::new()
            } else {
                format!(" {}", idx.saturating_add(1))
            };
            DbError::unsupported(format!(
                "Result column{column} ({ty}) cannot be read: {err}; cast it to VARCHAR in the query"
            ))
        }
        other => DbError::internal(other.to_string()),
    }
}

/// Map a duckdb-rs error without statement context.
pub(crate) fn map_error(e: duckdb::Error) -> DbError {
    map_error_ctx(e, &ErrContext::default())
}

/// `duckdb::Result` → [`DbResult`].
pub(crate) trait DuckResultExt<T> {
    fn db(self) -> DbResult<T>;
}

impl<T> DuckResultExt<T> for duckdb::Result<T> {
    fn db(self) -> DbResult<T> {
        self.map_err(map_error)
    }
}

/// The connection thread is gone.
pub(crate) fn worker_gone() -> DbError {
    DbError::connection("The DuckDB connection is closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ErrorKind;

    #[test]
    fn kinds() {
        let ctx = ErrContext::default();
        assert_eq!(
            from_message("Parser Error: syntax error at or near \"FORM\"", &ctx).kind,
            ErrorKind::Syntax
        );
        assert_eq!(
            from_message("Catalog Error: Table with name x does not exist!", &ctx)
                .code
                .as_deref(),
            Some("CATALOG")
        );
        assert_eq!(
            from_message("Interrupt Error: Interrupted!", &ctx).kind,
            ErrorKind::Cancelled
        );
        let t = ErrContext {
            timed_out: true,
            ..ctx
        };
        assert_eq!(
            from_message("Interrupt Error: Interrupted!", &t).kind,
            ErrorKind::Timeout
        );
        let ro = "Invalid Input Error: Cannot execute statement of type \"INSERT\" on database \"x\" which is attached in read-only mode!";
        assert_eq!(from_message(ro, &ctx).kind, ErrorKind::ReadOnly);
        assert_eq!(
            from_message("IO Error: Cannot open file", &ctx).kind,
            ErrorKind::Io
        );
        assert_eq!(
            from_message("Constraint Error: Duplicate key", &ctx).kind,
            ErrorKind::Query
        );
        assert_eq!(from_message("something odd", &ctx).code, None);
    }

    #[test]
    fn positions() {
        let msg = "Parser Error: syntax error at or near \"FORM\"\n\nLINE 1: SELECT * FORM t\n                 ^";
        let ctx = ErrContext {
            statement: Some("SELECT * FORM t"),
            chars_before: 0,
            timed_out: false,
        };
        let e = from_message(msg, &ctx);
        assert_eq!(e.position, Some(10));
        assert!(e.detail.as_deref().unwrap().starts_with("LINE 1"));
        let msg2 = "Parser Error: syntax error at or near \"x\"\n\nLINE 2: SELEC x\n        ^";
        let ctx2 = ErrContext {
            statement: Some("SELECT 1;\nSELEC x"),
            chars_before: 5,
            timed_out: false,
        };
        assert_eq!(from_message(msg2, &ctx2).position, Some(5 + 10 + 1));
    }
}
