//! Optional pre-execution syntax check with the `sqlparser` crate.
//!
//! The editor shows the result as a warning only, so this module prefers
//! silence whenever the parser is likely to be wrong: only queries and DML
//! (`SELECT`, `WITH`, `VALUES`, `INSERT`, `UPDATE`, `DELETE`, `REPLACE`) are
//! checked, and statements with client-side syntax (template variables,
//! named parameters, client commands, delimiters) or vendor constructs that
//! `sqlparser` commonly rejects are skipped.

use crate::position::offset_of_line_col_lf;
use crate::token::{TokenKind, is_word, tokenize};
use dbiji_core::{DbKind, SqlSyntax};
use sqlparser::dialect::{
    ClickHouseDialect, Dialect as ParserDialect, DuckDbDialect, GenericDialect, MsSqlDialect,
    MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::{Parser, ParserError};

/// A syntax error reported by the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxIssue {
    /// Parser message without the location suffix.
    pub message: String,
    /// Byte offset in the statement.
    pub offset: usize,
}

const CHECKED: &[&str] = &[
    "SELECT", "WITH", "VALUES", "INSERT", "UPDATE", "DELETE", "REPLACE",
];
/// Vendor syntax that `sqlparser` frequently does not support.
const SKIP_WORDS_COMMON: &[&str] = &[
    "TABLESAMPLE",
    "ORDINALITY",
    "MATCH_RECOGNIZE",
    "PIVOT",
    "UNPIVOT",
    "CONNECT",
    "MODEL",
];
const SKIP_WORDS_MYSQL: &[&str] = &[
    "OUTFILE",
    "DUMPFILE",
    "STRAIGHT_JOIN",
    "SQL_CALC_FOUND_ROWS",
    "HIGH_PRIORITY",
    "LOW_PRIORITY",
    "DELAYED",
    "PARTITION",
    "SOUNDS",
    "DISTINCTROW",
    "LOCK",
    "SQL_NO_CACHE",
    "SQL_BUFFER_RESULT",
];
const SKIP_WORDS_CLICKHOUSE: &[&str] = &[
    "FORMAT", "SETTINGS", "PREWHERE", "FINAL", "SAMPLE", "ARRAY", "GLOBAL", "ASOF",
];
const SKIP_WORDS_DUCKDB: &[&str] = &[
    "EXCLUDE",
    "COLUMNS",
    "ASOF",
    "POSITIONAL",
    "SUMMARIZE",
    "STRUCT",
    "LAMBDA",
];
const SKIP_WORDS_MSSQL: &[&str] = &["OUTPUT", "OPTION", "PIVOT", "FOR"];

/// Lexical rules approximating each product, for the pre-checks.
fn lexical(kind: DbKind) -> SqlSyntax {
    let mut s = SqlSyntax::ansi();
    match kind {
        DbKind::Postgres | DbKind::CockroachDb | DbKind::DuckDb => {
            s.dollar_quoting = true;
            s.e_strings = true;
            s.dollar_params = true;
            s.nested_block_comments = true;
            s.question_params = false;
            s.extra_ident_chars = "$";
        }
        DbKind::MySql | DbKind::MariaDb => {
            s.identifier_quotes = &[('`', '`')];
            s.double_quoted_strings = true;
            s.backslash_escapes = true;
            s.hash_comments = true;
            s.dash_comment_needs_space = true;
            s.at_variables = true;
            s.delimiter_command = true;
        }
        DbKind::Sqlite => s.identifier_quotes = &[('"', '"'), ('`', '`'), ('[', ']')],
        DbKind::SqlServer => {
            s.identifier_quotes = &[('[', ']'), ('"', '"')];
            s.at_variables = true;
            s.batch_separator = Some("GO");
            s.extra_ident_chars = "#@$";
            s.nested_block_comments = true;
        }
        DbKind::ClickHouse => {
            s.identifier_quotes = &[('`', '`'), ('"', '"')];
            s.backslash_escapes = true;
        }
        DbKind::Generic => {}
    }
    s
}

fn parser_dialect(kind: DbKind) -> Box<dyn ParserDialect> {
    match kind {
        DbKind::Postgres | DbKind::CockroachDb => Box::new(PostgreSqlDialect {}),
        DbKind::MySql | DbKind::MariaDb => Box::new(MySqlDialect {}),
        DbKind::Sqlite => Box::new(SQLiteDialect {}),
        DbKind::SqlServer => Box::new(MsSqlDialect {}),
        DbKind::ClickHouse => Box::new(ClickHouseDialect {}),
        DbKind::DuckDb => Box::new(DuckDbDialect {}),
        DbKind::Generic => Box::new(GenericDialect {}),
    }
}

/// Whether `stmt` is suitable for checking with `sqlparser`.
fn checkable(stmt: &str, kind: DbKind) -> bool {
    let syn = lexical(kind);
    let toks = tokenize(stmt, &syn);
    let sig: Vec<_> = toks.iter().filter(|t| !t.is_trivia()).collect();
    let Some(first) = sig.iter().find(|t| t.kind != TokenKind::LParen) else {
        return false;
    };
    if !CHECKED.iter().any(|w| is_word(first, stmt, w)) {
        return false;
    }
    if kind != DbKind::MySql
        && kind != DbKind::MariaDb
        && kind != DbKind::Sqlite
        && is_word(first, stmt, "REPLACE")
    {
        return false;
    }
    let vendor: &[&str] = match kind {
        DbKind::MySql | DbKind::MariaDb => SKIP_WORDS_MYSQL,
        DbKind::ClickHouse => SKIP_WORDS_CLICKHOUSE,
        DbKind::DuckDb => SKIP_WORDS_DUCKDB,
        DbKind::SqlServer => SKIP_WORDS_MSSQL,
        _ => &[],
    };
    for t in &sig {
        match t.kind {
            TokenKind::ClientCommand
            | TokenKind::Delimiter
            | TokenKind::Variable
            | TokenKind::Unknown => return false,
            TokenKind::Parameter if t.text(stmt).starts_with(':') => return false,
            TokenKind::BlockComment | TokenKind::LineComment => {}
            TokenKind::Keyword | TokenKind::Identifier
                if (SKIP_WORDS_COMMON
                    .iter()
                    .chain(vendor)
                    .any(|w| is_word(t, stmt, w))
                    || t.text(stmt).starts_with('#')) =>
            {
                return false;
            }
            _ => {}
        }
    }
    true
}

/// Parse `stmt` with the `sqlparser` dialect for `kind`; `None` when it
/// parses or is not checked (see the module docs).
pub fn syntax_check(stmt: &str, kind: DbKind) -> Option<SyntaxIssue> {
    if !checkable(stmt, kind) {
        return None;
    }
    let dialect = parser_dialect(kind);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Parser::parse_sql(dialect.as_ref(), stmt)
    }));
    let msg = match result {
        Ok(Ok(_)) => return None,
        Ok(Err(ParserError::ParserError(m) | ParserError::TokenizerError(m))) => m,
        Ok(Err(ParserError::RecursionLimitExceeded)) | Err(_) => return None,
    };
    let lower = msg.to_lowercase();
    if lower.contains("not supported") || lower.contains("unsupported") {
        return None;
    }
    let (message, offset) = match msg.rfind(" at Line: ") {
        Some(p) => {
            let loc = &msg[p + " at Line: ".len()..];
            let mut parts = loc.split(", Column: ");
            let line = parts.next().and_then(|l| l.trim().parse::<usize>().ok());
            let col = parts.next().and_then(|c| c.trim().parse::<usize>().ok());
            match (line, col) {
                (Some(l), Some(c)) => (msg[..p].to_string(), offset_of_line_col_lf(stmt, l, c)),
                _ => (msg.clone(), stmt.trim_end().len()),
            }
        }
        None => (msg.clone(), stmt.trim_end().len()),
    };
    Some(SyntaxIssue { message, offset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_statements_are_silent() {
        let pg = [
            "SELECT 1",
            "SELECT a, b FROM t WHERE a = $1 AND b IN (1, 2, 3) ORDER BY a DESC LIMIT 10",
            "WITH x AS (SELECT 1 AS a) SELECT * FROM x",
            "SELECT u.id, count(*) FROM users u LEFT JOIN orders o ON o.user_id = u.id GROUP BY u.id HAVING count(*) > 1",
            "INSERT INTO t (a, b) VALUES (1, 'x') ON CONFLICT (a) DO NOTHING RETURNING id",
            "UPDATE t SET a = 1 WHERE id = 2",
            "DELETE FROM t WHERE id = 1",
            "SELECT data->>'k', a::int FROM t",
            "select * from t; ",
            "VALUES (1), (2)",
        ];
        for s in pg {
            assert_eq!(syntax_check(s, DbKind::Postgres), None, "{s}");
        }
        for s in [
            "SELECT `a` FROM `t` WHERE b = ? LIMIT 5, 10",
            "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = 2",
            "REPLACE INTO t VALUES (1)",
        ] {
            assert_eq!(syntax_check(s, DbKind::MySql), None, "{s}");
        }
        assert_eq!(
            syntax_check("SELECT [a] FROM [dbo].[t] WHERE b = 1", DbKind::SqlServer),
            None
        );
        assert_eq!(syntax_check("SELECT \"a\" FROM t", DbKind::Sqlite), None);
        assert_eq!(syntax_check("SELECT 1", DbKind::Generic), None);
        assert_eq!(syntax_check("SELECT 1", DbKind::DuckDb), None);
        assert_eq!(syntax_check("SELECT 1", DbKind::ClickHouse), None);
    }

    #[test]
    fn errors_have_positions() {
        let s = "SELECT * FROM t WHERE x = = 1";
        let e = syntax_check(s, DbKind::Postgres).expect("error");
        assert_eq!(e.offset, s.find("= =").unwrap_or(0) + 2, "{e:?}");
        // sqlparser accepts keywords as table names; the error is at `=`.
        let s = "SELECT * FROM WHERE x = 1";
        let e = syntax_check(s, DbKind::Postgres).expect("error");
        assert_eq!(s.get(e.offset..e.offset + 1), Some("="), "{e:?}");
        assert!(!e.message.contains("Line:"));
        let s = "SELECT a\nFROM t\nWHERE é = = 1";
        let e = syntax_check(s, DbKind::Postgres).expect("error");
        assert_eq!(&s[e.offset..e.offset + 1], "=", "{e:?}");
        assert!(s.is_char_boundary(e.offset));
        let s = "SELECT a FROM t WHERE";
        let e = syntax_check(s, DbKind::Postgres).expect("error");
        assert!(e.offset <= s.len());
        assert!(syntax_check("INSERT INTO t VALUES (1", DbKind::MySql).is_some());
    }

    #[test]
    fn skipped_statements() {
        for s in [
            "",
            "-- comment",
            "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
            "DO $$ BEGIN END $$",
            "SELECT * FROM ${table}",
            "SELECT * FROM t WHERE a = :a",
            "@set x = 1",
            "SELEC 1",
            "BEGIN",
            "SELECT * FROM t TABLESAMPLE SYSTEM (10)",
        ] {
            assert_eq!(syntax_check(s, DbKind::Postgres), None, "{s}");
        }
        assert_eq!(
            syntax_check("SELECT * INTO OUTFILE '/tmp/x' FROM t", DbKind::MySql),
            None
        );
        assert_eq!(syntax_check("SELECT @x := 1", DbKind::MySql), None);
        assert_eq!(
            syntax_check("SELECT * FROM t FORMAT JSON", DbKind::ClickHouse),
            None
        );
        assert_eq!(syntax_check("SELECT * FROM #tmp", DbKind::SqlServer), None);
    }

    #[test]
    fn random_inputs_never_panic() {
        use crate::testutil::*;
        let mut rng = Rng::new(51);
        let kinds = [
            DbKind::Postgres,
            DbKind::MySql,
            DbKind::Sqlite,
            DbKind::SqlServer,
            DbKind::ClickHouse,
            DbKind::DuckDb,
            DbKind::Generic,
        ];
        for i in 0..300 {
            let mut src = String::from(*rng.pick(&[
                "SELECT ",
                "WITH ",
                "INSERT INTO ",
                "UPDATE ",
                "DELETE FROM ",
                "VALUES ",
            ]));
            src.push_str(&random_sql(&mut rng, 12));
            if let Some(e) = syntax_check(&src, kinds[i % kinds.len()]) {
                assert!(e.offset <= src.len() && src.is_char_boundary(e.offset));
            }
        }
    }
}
