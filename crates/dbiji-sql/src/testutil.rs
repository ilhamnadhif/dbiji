//! Test helpers: `SqlSyntax` values imitating real dialects, a configurable
//! test [`Dialect`], and a deterministic pseudo random generator.

use dbiji_core::DbKind;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::keywords::{ANSI_FUNCTIONS, ANSI_KEYWORDS, ANSI_TYPES};
use std::sync::LazyLock;

fn leak(base: &'static [&'static str], extra: &[&'static str]) -> &'static [&'static str] {
    let mut v: Vec<&'static str> = base.to_vec();
    v.extend_from_slice(extra);
    Box::leak(v.into_boxed_slice())
}

static PG_KW: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_KEYWORDS,
        &[
            "RETURNING",
            "ILIKE",
            "ATOMIC",
            "LOOP",
            "CONFLICT",
            "NOTHING",
        ],
    )
});
static PG_FN: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_FUNCTIONS,
        &[
            "now",
            "string_agg",
            "array_agg",
            "jsonb_build_object",
            "generate_series",
            "unnest",
        ],
    )
});
static PG_TY: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_TYPES,
        &[
            "SERIAL",
            "BIGSERIAL",
            "JSONB",
            "UUID",
            "TIMESTAMPTZ",
            "INT4",
            "INT8",
        ],
    )
});
static MY_KW: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_KEYWORDS,
        &[
            "AUTO_INCREMENT",
            "ENGINE",
            "CHARSET",
            "DUPLICATE",
            "IGNORE",
            "STRAIGHT_JOIN",
            "REPLACE",
            "LOOP",
            "REPEAT",
            "ELSEIF",
            "LEAVE",
            "ITERATE",
            "DEFINER",
            "EVENT",
            "SCHEDULE",
            "EVERY",
            "OPTIMIZE",
            "REPAIR",
            "USE",
            "UNLOCK",
        ],
    )
});
static MY_FN: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_FUNCTIONS,
        &[
            "ifnull",
            "group_concat",
            "date_format",
            "if",
            "repeat",
            "last_insert_id",
        ],
    )
});
static MY_TY: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_TYPES,
        &[
            "TINYINT",
            "MEDIUMINT",
            "DATETIME",
            "LONGTEXT",
            "JSON",
            "ENUM",
        ],
    )
});
static LITE_KW: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_KEYWORDS,
        &[
            "PRAGMA",
            "AUTOINCREMENT",
            "VACUUM",
            "ATTACH",
            "DETACH",
            "GLOB",
        ],
    )
});
static MS_KW: LazyLock<&'static [&'static str]> = LazyLock::new(|| {
    leak(
        ANSI_KEYWORDS,
        &[
            "TOP", "TRAN", "TRY", "CATCH", "NOCOUNT", "PRINT", "EXEC", "GO", "OUTPUT",
        ],
    )
});

/// PostgreSQL-like syntax.
pub(crate) fn pg() -> SqlSyntax {
    SqlSyntax {
        keywords: *PG_KW,
        functions: *PG_FN,
        data_types: *PG_TY,
        identifier_quotes: &[('"', '"')],
        double_quoted_strings: false,
        backslash_escapes: false,
        e_strings: true,
        n_strings: true,
        x_strings: true,
        dollar_quoting: true,
        hash_comments: false,
        nested_block_comments: true,
        dash_comment_needs_space: false,
        question_params: false,
        dollar_params: true,
        at_variables: false,
        batch_separator: None,
        delimiter_command: false,
        compound_blocks: false,
        extra_ident_chars: "$",
        ident_case: IdentCase::Lower,
        case_sensitive_names: false,
        shell: None,
    }
}

/// MySQL-like syntax.
pub(crate) fn mysql() -> SqlSyntax {
    SqlSyntax {
        keywords: *MY_KW,
        functions: *MY_FN,
        data_types: *MY_TY,
        identifier_quotes: &[('`', '`')],
        double_quoted_strings: true,
        backslash_escapes: true,
        e_strings: false,
        n_strings: true,
        x_strings: true,
        dollar_quoting: false,
        hash_comments: true,
        nested_block_comments: false,
        dash_comment_needs_space: true,
        question_params: true,
        dollar_params: false,
        at_variables: true,
        batch_separator: None,
        delimiter_command: true,
        compound_blocks: true,
        extra_ident_chars: "$",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: false,
        shell: None,
    }
}

/// SQLite-like syntax.
pub(crate) fn sqlite() -> SqlSyntax {
    SqlSyntax {
        keywords: *LITE_KW,
        functions: ANSI_FUNCTIONS,
        data_types: ANSI_TYPES,
        identifier_quotes: &[('"', '"'), ('`', '`'), ('[', ']')],
        double_quoted_strings: false,
        backslash_escapes: false,
        e_strings: false,
        n_strings: false,
        x_strings: true,
        dollar_quoting: false,
        hash_comments: false,
        nested_block_comments: false,
        dash_comment_needs_space: false,
        question_params: true,
        dollar_params: false,
        at_variables: false,
        batch_separator: None,
        delimiter_command: false,
        compound_blocks: true,
        extra_ident_chars: "",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: false,
        shell: None,
    }
}

/// T-SQL-like syntax.
pub(crate) fn tsql() -> SqlSyntax {
    SqlSyntax {
        keywords: *MS_KW,
        functions: ANSI_FUNCTIONS,
        data_types: ANSI_TYPES,
        identifier_quotes: &[('[', ']'), ('"', '"')],
        double_quoted_strings: false,
        backslash_escapes: false,
        e_strings: false,
        n_strings: true,
        x_strings: false,
        dollar_quoting: false,
        hash_comments: false,
        nested_block_comments: true,
        dash_comment_needs_space: false,
        question_params: false,
        dollar_params: false,
        at_variables: true,
        batch_separator: Some("GO"),
        delimiter_command: false,
        compound_blocks: true,
        extra_ident_chars: "#@$",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: false,
        shell: None,
    }
}

/// Configurable dialect for tests.
pub(crate) struct TestDialect {
    kind: DbKind,
    syntax: SqlSyntax,
    ddl: ansi::AnsiDdl,
}

impl TestDialect {
    pub(crate) fn new(kind: DbKind, syntax: SqlSyntax) -> TestDialect {
        TestDialect {
            kind,
            ddl: ansi::AnsiDdl::new(syntax.clone()),
            syntax,
        }
    }
    pub(crate) fn pg() -> TestDialect {
        TestDialect::new(DbKind::Postgres, pg())
    }
    pub(crate) fn mysql() -> TestDialect {
        TestDialect::new(DbKind::MySql, mysql())
    }
    pub(crate) fn sqlite() -> TestDialect {
        TestDialect::new(DbKind::Sqlite, sqlite())
    }
    pub(crate) fn tsql() -> TestDialect {
        TestDialect::new(DbKind::SqlServer, tsql())
    }
}

impl Dialect for TestDialect {
    fn kind(&self) -> DbKind {
        self.kind
    }
    fn syntax(&self) -> &SqlSyntax {
        &self.syntax
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }
    fn placeholder(&self, index: usize) -> String {
        if self.kind == DbKind::Postgres {
            format!("${index}")
        } else {
            "?".into()
        }
    }
    fn upsert(
        &self,
        table: &str,
        columns: &[String],
        key_columns: &[String],
        values: &[String],
    ) -> Option<String> {
        if key_columns.is_empty() {
            return None;
        }
        match self.kind {
            DbKind::Postgres => {
                let sets: Vec<String> = columns
                    .iter()
                    .filter(|c| !key_columns.contains(c))
                    .map(|c| format!("{c} = EXCLUDED.{c}"))
                    .collect();
                Some(format!(
                    "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
                    columns.join(", "),
                    values.join(", "),
                    key_columns.join(", "),
                    sets.join(", ")
                ))
            }
            DbKind::MySql => {
                let sets: Vec<String> = columns
                    .iter()
                    .filter(|c| !key_columns.contains(c))
                    .map(|c| format!("{c} = VALUES({c})"))
                    .collect();
                Some(format!(
                    "INSERT INTO {table} ({}) VALUES ({}) ON DUPLICATE KEY UPDATE {}",
                    columns.join(", "),
                    values.join(", "),
                    sets.join(", ")
                ))
            }
            _ => None,
        }
    }
}

/// Deterministic xorshift generator for property-style tests.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Fragments that exercise tricky lexical and syntactic paths.
pub(crate) const FRAGMENTS: &[&str] = &[
    "SELECT",
    " ",
    "\n",
    "\n\n",
    "\t",
    "a",
    "b.c",
    "\"Q\"",
    "`bt`",
    "[br]",
    "'str'",
    "'",
    "\"",
    "`",
    "[",
    "]",
    "(",
    ")",
    ",",
    ";",
    ".",
    "*",
    "=",
    "<>",
    "::",
    ":=",
    ":name",
    "?",
    "$1",
    "$$",
    "$t$",
    "${v}",
    "${",
    "@x",
    "@@y",
    "@set x = 1",
    "@include f.sql",
    "@export {\"type\":\"csv\"}",
    "--c",
    "-- c\n",
    "#h\n",
    "/*",
    "*/",
    "/* b */",
    "E'\\''",
    "N'n'",
    "X'0f'",
    "1",
    "1.5",
    ".5",
    "1e9",
    "0x1F",
    "FROM",
    "WHERE",
    "JOIN",
    "ON",
    "AND",
    "OR",
    "GROUP BY",
    "ORDER BY",
    "WITH",
    "AS",
    "BEGIN",
    "END",
    "END IF",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "IF",
    "LOOP",
    "WHILE",
    "DO",
    "REPEAT",
    "CREATE PROCEDURE p()",
    "CREATE TRIGGER t",
    "DELIMITER //",
    "DELIMITER ;",
    "//",
    "GO",
    "INSERT INTO t",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "é",
    "名",
    "\u{3000}",
    "—",
    "\r\n",
    "\r",
    "\\",
    "{",
    "}",
    "->>",
    "#>",
    "@>",
    "?|",
    "LIMIT",
    "UNION",
    "(SELECT",
    "users",
    "u",
    "o",
    "orders",
    "DROP",
    "TRUNCATE",
    "ALTER",
    "EXPLAIN ANALYZE",
];

/// A random script assembled from [`FRAGMENTS`] and random characters.
pub(crate) fn random_sql(rng: &mut Rng, pieces: usize) -> String {
    let mut s = String::new();
    for _ in 0..pieces {
        if rng.below(10) == 0 {
            let c = char::from_u32(rng.below(0x3000) as u32).unwrap_or('x');
            s.push(c);
        } else {
            s.push_str(rng.pick(FRAGMENTS));
        }
        if rng.below(3) == 0 {
            s.push(' ');
        }
    }
    s
}

/// Largest char boundary `<= i`.
pub(crate) fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// All syntaxes used by tests.
pub(crate) fn all_syntaxes() -> Vec<SqlSyntax> {
    vec![pg(), mysql(), sqlite(), tsql(), SqlSyntax::ansi()]
}
