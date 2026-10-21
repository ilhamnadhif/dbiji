//! The SQLite SQL dialect.

use crate::ddl::SqliteDdl;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::config::DbKind;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::keywords;
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::value::{DataKind, Value};
use std::collections::BTreeMap;

/// All SQLite keywords (<https://sqlite.org/lang_keywords.html>) plus a few
/// words with special meaning (`TRUE`, `FALSE`, `ROWID`, `STRICT`, `STORED`).
pub const SQLITE_KEYWORDS: &[&str] = &[
    "ABORT",
    "ACTION",
    "ADD",
    "AFTER",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "ATTACH",
    "AUTOINCREMENT",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DO",
    "DROP",
    "EACH",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUSIVE",
    "EXISTS",
    "EXPLAIN",
    "FAIL",
    "FALSE",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GENERATED",
    "GLOB",
    "GROUP",
    "GROUPS",
    "HAVING",
    "IF",
    "IGNORE",
    "IMMEDIATE",
    "IN",
    "INDEX",
    "INDEXED",
    "INITIALLY",
    "INNER",
    "INSERT",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATCH",
    "MATERIALIZED",
    "NATURAL",
    "NO",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OTHERS",
    "OUTER",
    "OVER",
    "PARTITION",
    "PLAN",
    "PRAGMA",
    "PRECEDING",
    "PRIMARY",
    "QUERY",
    "RAISE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWID",
    "ROWS",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "STORED",
    "STRICT",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TIES",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
];

/// Built-in functions: core, date/time, aggregate, window, math, JSON and FTS.
pub const SQLITE_FUNCTIONS: &[&str] = &[
    // core
    "abs",
    "changes",
    "char",
    "coalesce",
    "concat",
    "concat_ws",
    "format",
    "glob",
    "hex",
    "if",
    "ifnull",
    "iif",
    "instr",
    "last_insert_rowid",
    "length",
    "like",
    "likelihood",
    "likely",
    "load_extension",
    "lower",
    "ltrim",
    "max",
    "min",
    "nullif",
    "octet_length",
    "printf",
    "quote",
    "random",
    "randomblob",
    "replace",
    "round",
    "rtrim",
    "sign",
    "soundex",
    "sqlite_compileoption_get",
    "sqlite_compileoption_used",
    "sqlite_offset",
    "sqlite_source_id",
    "sqlite_version",
    "substr",
    "substring",
    "total_changes",
    "trim",
    "typeof",
    "unhex",
    "unicode",
    "unistr",
    "unistr_quote",
    "unlikely",
    "upper",
    "zeroblob",
    // date and time
    "date",
    "time",
    "datetime",
    "julianday",
    "unixepoch",
    "strftime",
    "timediff",
    // aggregate
    "avg",
    "count",
    "group_concat",
    "string_agg",
    "sum",
    "total",
    // window
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    // math
    "acos",
    "acosh",
    "asin",
    "asinh",
    "atan",
    "atan2",
    "atanh",
    "ceil",
    "ceiling",
    "cos",
    "cosh",
    "degrees",
    "exp",
    "floor",
    "ln",
    "log",
    "log10",
    "log2",
    "mod",
    "pi",
    "pow",
    "power",
    "radians",
    "sin",
    "sinh",
    "sqrt",
    "tan",
    "tanh",
    "trunc",
    // JSON
    "json",
    "jsonb",
    "json_array",
    "jsonb_array",
    "json_array_length",
    "json_error_position",
    "json_extract",
    "jsonb_extract",
    "json_insert",
    "jsonb_insert",
    "json_object",
    "jsonb_object",
    "json_patch",
    "jsonb_patch",
    "json_pretty",
    "json_remove",
    "jsonb_remove",
    "json_replace",
    "jsonb_replace",
    "json_set",
    "jsonb_set",
    "json_type",
    "json_valid",
    "json_quote",
    "json_group_array",
    "jsonb_group_array",
    "json_group_object",
    "jsonb_group_object",
    "json_each",
    "json_tree",
    // full text search
    "bm25",
    "highlight",
    "snippet",
    "offsets",
    "matchinfo",
];

/// Type names: SQLite storage classes, STRICT table types and the common
/// declared names SQLite maps to affinities.
pub const SQLITE_TYPES: &[&str] = &[
    "INTEGER",
    "INT",
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "BIGINT",
    "UNSIGNED BIG INT",
    "INT2",
    "INT8",
    "REAL",
    "DOUBLE",
    "DOUBLE PRECISION",
    "FLOAT",
    "NUMERIC",
    "DECIMAL",
    "BOOLEAN",
    "DATE",
    "DATETIME",
    "TIMESTAMP",
    "TIME",
    "TEXT",
    "CHARACTER",
    "VARCHAR",
    "VARYING CHARACTER",
    "NCHAR",
    "NATIVE CHARACTER",
    "NVARCHAR",
    "CLOB",
    "BLOB",
    "JSON",
    "ANY",
];

/// Lexical rules of SQLite.
pub static SQLITE_SYNTAX: SqlSyntax = SqlSyntax {
    keywords: SQLITE_KEYWORDS,
    functions: SQLITE_FUNCTIONS,
    data_types: SQLITE_TYPES,
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
    // `$name` (and therefore `$1`) are named parameters in SQLite.
    dollar_params: true,
    // `@name` parameters
    at_variables: true,
    batch_separator: None,
    delimiter_command: false,
    compound_blocks: true,
    extra_ident_chars: "$",
    ident_case: IdentCase::Preserve,
    case_sensitive_names: false,
};

/// True when `ident` must be quoted in SQLite: empty, not a plain
/// `[A-Za-z_][A-Za-z0-9_$]*` word, or a keyword.
pub(crate) fn sqlite_needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return true;
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        return true;
    }
    SQLITE_KEYWORDS
        .iter()
        .any(|k| k.eq_ignore_ascii_case(ident))
        || keywords::is_reserved(ident)
}

/// Quote an identifier for SQLite when needed.
pub(crate) fn quote_ident(ident: &str) -> String {
    if sqlite_needs_quoting(ident) {
        force_quote(ident)
    } else {
        ident.to_string()
    }
}

/// Always quote with double quotes.
pub(crate) fn force_quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `schema.name`, quoted as needed.
pub(crate) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(s), quote_ident(name)),
        _ => quote_ident(name),
    }
}

/// SQL string literal.
pub(crate) fn string_literal(s: &str) -> String {
    ansi::string_literal(s, false)
}

/// Float literal that SQLite reads back as the same REAL value.
pub(crate) fn float_literal(f: f64) -> String {
    if f.is_nan() {
        "NULL".into()
    } else if f.is_infinite() {
        if f > 0.0 {
            "9e999".into()
        } else {
            "-9e999".into()
        }
    } else {
        // Debug formatting is the shortest round-trip form and always contains
        // a '.' or an exponent, so SQLite parses it as REAL.
        format!("{f:?}")
    }
}

const MAINTENANCE_DATABASE: &[ObjectKind] = &[ObjectKind::Database, ObjectKind::Schema];
const MAINTENANCE_OBJECTS: &[ObjectKind] = &[
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::Index,
];
const MAINTENANCE_CHECK: &[ObjectKind] =
    &[ObjectKind::Database, ObjectKind::Schema, ObjectKind::Table];

/// The SQLite dialect.
pub struct SqliteDialect {
    ddl: SqliteDdl,
}

impl SqliteDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        SqliteDialect {
            ddl: SqliteDdl::new(),
        }
    }

    /// The dialect of a connection with (or without) foreign key enforcement.
    pub fn with_foreign_keys(foreign_keys: bool) -> Self {
        SqliteDialect {
            ddl: SqliteDdl::with_foreign_keys(foreign_keys),
        }
    }
}

impl Default for SqliteDialect {
    fn default() -> Self {
        Self::new()
    }
}

/// Schema (attached database) a maintenance target refers to.
fn target_schema(t: &ObjectRef) -> Option<&str> {
    match t.kind {
        ObjectKind::Schema => Some(t.schema.as_deref().unwrap_or(t.name.as_str())),
        ObjectKind::Database => None,
        _ => t.schema.as_deref(),
    }
}

fn pragma_prefix(schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("PRAGMA {}.", quote_ident(s)),
        None => "PRAGMA ".to_string(),
    }
}

impl Dialect for SqliteDialect {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn syntax(&self) -> &SqlSyntax {
        &SQLITE_SYNTAX
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn catalog_folders(&self) -> Vec<dbiji_core::catalog::CatalogFolder> {
        crate::catalogs::folders()
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        sqlite_needs_quoting(ident)
    }

    fn bool_literal(&self, b: bool) -> String {
        if b { "1".into() } else { "0".into() }
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let _ = type_name;
        match value {
            Value::Float(f) => float_literal(*f),
            Value::UInt(u) if i64::try_from(*u).is_err() => string_literal(&u.to_string()),
            Value::DateTimeTz(_) | Value::Array(_) => {
                string_literal(&crate::convert::value_as_sql_text(value))
            }
            other => ansi::literal(self, other),
        }
    }

    fn upsert(
        &self,
        table: &str,
        columns: &[String],
        key_columns: &[String],
        values: &[String],
    ) -> Option<String> {
        if columns.is_empty() || columns.len() != values.len() || key_columns.is_empty() {
            return None;
        }
        let updates: Vec<String> = columns
            .iter()
            .filter(|c| !key_columns.contains(c))
            .map(|c| format!("{c} = excluded.{c}"))
            .collect();
        let action = if updates.is_empty() {
            "DO NOTHING".to_string()
        } else {
            format!("DO UPDATE SET {}", updates.join(", "))
        };
        Some(format!(
            "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) {action}",
            columns.join(", "),
            values.join(", "),
            key_columns.join(", ")
        ))
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!("DELETE FROM {}", self.qualified_ref(table))
    }

    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        vec![
            MaintenanceOp {
                id: "vacuum",
                label: "Vacuum",
                description: "Rebuild the database file, reclaiming free pages and defragmenting tables and indexes",
                applies_to: MAINTENANCE_DATABASE,
                options: &[],
                needs_autocommit: true,
            },
            MaintenanceOp {
                id: "analyze",
                label: "Analyze",
                description: "Gather statistics about tables and indexes for the query planner",
                applies_to: MAINTENANCE_OBJECTS,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "reindex",
                label: "Reindex",
                description: "Delete and recreate indexes from scratch",
                applies_to: MAINTENANCE_OBJECTS,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "integrity_check",
                label: "Integrity check",
                description: "Check the database (or a table) for corruption, constraint violations and index consistency",
                applies_to: MAINTENANCE_CHECK,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "quick_check",
                label: "Quick check",
                description: "Faster integrity check that skips index content verification",
                applies_to: MAINTENANCE_CHECK,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "optimize",
                label: "Optimize",
                description: "Run PRAGMA optimize to refresh planner statistics where needed",
                applies_to: MAINTENANCE_DATABASE,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "wal_checkpoint",
                label: "WAL checkpoint",
                description: "Copy the write-ahead log into the database file",
                applies_to: MAINTENANCE_DATABASE,
                options: &[("truncate", "Truncate the WAL file afterwards", true)],
                needs_autocommit: true,
            },
        ]
    }

    fn maintenance_sql(
        &self,
        op: &str,
        targets: &[ObjectRef],
        options: &BTreeMap<String, bool>,
    ) -> Vec<String> {
        let default_target = [ObjectRef::new(ObjectKind::Database, None, "main")];
        let targets = if targets.is_empty() {
            &default_target[..]
        } else {
            targets
        };
        let mut out: Vec<String> = Vec::new();
        for t in targets {
            let schema = target_schema(t);
            let object = matches!(t.kind, ObjectKind::Table | ObjectKind::Index);
            let sql = match op {
                "vacuum" => match schema {
                    Some(s) => format!("VACUUM {}", quote_ident(s)),
                    None => "VACUUM".to_string(),
                },
                "analyze" if object => format!("ANALYZE {}", qualified(schema, &t.name)),
                "analyze" => match schema {
                    Some(s) => format!("ANALYZE {}", quote_ident(s)),
                    None => "ANALYZE".to_string(),
                },
                "reindex" if object => format!("REINDEX {}", qualified(schema, &t.name)),
                "reindex" => "REINDEX".to_string(),
                "integrity_check" | "quick_check" => {
                    let base = format!("{}{op}", pragma_prefix(schema));
                    if t.kind == ObjectKind::Table {
                        format!("{base}({})", string_literal(&t.name))
                    } else {
                        base
                    }
                }
                "optimize" => format!("{}optimize", pragma_prefix(schema)),
                "wal_checkpoint" => {
                    let mode = if options.get("truncate").copied().unwrap_or(true) {
                        "TRUNCATE"
                    } else {
                        "PASSIVE"
                    };
                    format!("{}wal_checkpoint({mode})", pragma_prefix(schema))
                }
                _ => continue,
            };
            if !out.contains(&sql) {
                out.push(sql);
            }
        }
        out
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        crate::convert::declared_kind(type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let d = SqliteDialect::new();
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("Users"), "Users");
        assert_eq!(d.quote_ident("key"), "\"key\"");
        assert_eq!(d.quote_ident("order items"), "\"order items\"");
        assert_eq!(d.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.quote_ident("1abc"), "\"1abc\"");
        assert_eq!(d.qualified(Some("main"), "t"), "main.t");
        assert_eq!(d.force_quote_ident("x"), "\"x\"");
    }

    #[test]
    fn literals() {
        let d = SqliteDialect::new();
        assert_eq!(d.literal(&Value::Bool(true), None), "1");
        assert_eq!(d.literal(&Value::Float(1.0), None), "1.0");
        assert_eq!(d.literal(&Value::Float(f64::INFINITY), None), "9e999");
        assert_eq!(d.literal(&Value::Float(f64::NAN), None), "NULL");
        assert_eq!(d.literal(&Value::Float(1e300), None), "1e300");
        assert_eq!(d.literal(&Value::bytes(vec![0xabu8]), None), "X'AB'");
        assert_eq!(d.literal(&Value::text("it's"), None), "'it''s'");
        assert_eq!(
            d.literal(&Value::UInt(u64::MAX), None),
            format!("'{}'", u64::MAX)
        );
        let arr = Value::Array(vec![Value::Int(1)].into_boxed_slice());
        assert_eq!(d.literal(&arr, None), "'[1]'");
    }

    #[test]
    fn upsert_and_truncate() {
        let d = SqliteDialect::new();
        let sql = d
            .upsert(
                "main.t",
                &["id".into(), "name".into()],
                &["id".into()],
                &["?".into(), "?".into()],
            )
            .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO main.t (id, name) VALUES (?, ?) ON CONFLICT (id) DO UPDATE SET name = excluded.name"
        );
        let only_keys = d
            .upsert("t", &["id".into()], &["id".into()], &["1".into()])
            .unwrap();
        assert!(only_keys.ends_with("DO NOTHING"));
        assert_eq!(
            d.truncate_table(&ObjectRef::table(Some("main".into()), "t")),
            "DELETE FROM main.t"
        );
        assert!(d.supports_returning());
    }

    #[test]
    fn maintenance() {
        let d = SqliteDialect::new();
        assert_eq!(d.maintenance_ops().len(), 7);
        let t = ObjectRef::table(Some("main".into()), "t");
        let opts = BTreeMap::new();
        assert_eq!(d.maintenance_sql("vacuum", &[], &opts), vec!["VACUUM"]);
        assert_eq!(
            d.maintenance_sql("analyze", std::slice::from_ref(&t), &opts),
            vec!["ANALYZE main.t"]
        );
        assert_eq!(
            d.maintenance_sql("reindex", std::slice::from_ref(&t), &opts),
            vec!["REINDEX main.t"]
        );
        assert_eq!(
            d.maintenance_sql("integrity_check", std::slice::from_ref(&t), &opts),
            vec!["PRAGMA main.integrity_check('t')"]
        );
        assert_eq!(
            d.maintenance_sql("quick_check", &[], &opts),
            vec!["PRAGMA quick_check"]
        );
        assert_eq!(
            d.maintenance_sql("optimize", &[], &opts),
            vec!["PRAGMA optimize"]
        );
        let schema = ObjectRef::new(ObjectKind::Schema, None, "aux");
        assert_eq!(
            d.maintenance_sql("wal_checkpoint", &[schema], &opts),
            vec!["PRAGMA aux.wal_checkpoint(TRUNCATE)"]
        );
    }
}
