//! The DuckDB SQL dialect (PostgreSQL style lexical rules).

use crate::ddl::DuckDbDdl;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::config::DbKind;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::keywords;
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::value::{DataKind, Value};
use std::collections::BTreeMap;

/// DuckDB keywords (reserved and unreserved, as listed by `duckdb_keywords()`),
/// for highlighting and completion.
pub const DUCKDB_KEYWORDS: &[&str] = &[
    "ABORT",
    "ABSOLUTE",
    "ACCESS",
    "ACTION",
    "ADD",
    "ADMIN",
    "AFTER",
    "AGGREGATE",
    "ALL",
    "ALSO",
    "ALTER",
    "ALWAYS",
    "ANALYSE",
    "ANALYZE",
    "AND",
    "ANTI",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASOF",
    "ASSERTION",
    "ASSIGNMENT",
    "ASYMMETRIC",
    "AT",
    "ATTACH",
    "ATTRIBUTE",
    "AUTHORIZATION",
    "BACKWARD",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BIGINT",
    "BINARY",
    "BIT",
    "BOOLEAN",
    "BOTH",
    "BY",
    "CACHE",
    "CALL",
    "CALLED",
    "CASCADE",
    "CASCADED",
    "CASE",
    "CAST",
    "CATALOG",
    "CENTURY",
    "CHAIN",
    "CHAR",
    "CHARACTER",
    "CHARACTERISTICS",
    "CHECK",
    "CHECKPOINT",
    "CLASS",
    "CLOSE",
    "CLUSTER",
    "COALESCE",
    "COLLATE",
    "COLLATION",
    "COLUMN",
    "COLUMNS",
    "COMMENT",
    "COMMENTS",
    "COMMIT",
    "COMMITTED",
    "COMPRESSION",
    "CONCURRENTLY",
    "CONFIGURATION",
    "CONFLICT",
    "CONNECTION",
    "CONSTRAINT",
    "CONSTRAINTS",
    "CONTENT",
    "CONTINUE",
    "CONVERSION",
    "COPY",
    "COST",
    "CREATE",
    "CROSS",
    "CSV",
    "CUBE",
    "CURRENT",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "CURSOR",
    "CYCLE",
    "DATA",
    "DATABASE",
    "DAY",
    "DAYS",
    "DEALLOCATE",
    "DEC",
    "DECADE",
    "DECIMAL",
    "DECLARE",
    "DEFAULT",
    "DEFAULTS",
    "DEFERRABLE",
    "DEFERRED",
    "DEFINER",
    "DELETE",
    "DELIMITER",
    "DELIMITERS",
    "DEPENDS",
    "DESC",
    "DESCRIBE",
    "DETACH",
    "DICTIONARY",
    "DISABLE",
    "DISCARD",
    "DISTINCT",
    "DO",
    "DOCUMENT",
    "DOMAIN",
    "DOUBLE",
    "DROP",
    "EACH",
    "ELSE",
    "ENABLE",
    "ENCODING",
    "ENCRYPTED",
    "END",
    "ENUM",
    "ESCAPE",
    "EVENT",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUDING",
    "EXCLUSIVE",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "EXPORT",
    "EXPORT_STATE",
    "EXTENSION",
    "EXTENSIONS",
    "EXTERNAL",
    "EXTRACT",
    "FALSE",
    "FAMILY",
    "FETCH",
    "FILTER",
    "FIRST",
    "FLOAT",
    "FOLLOWING",
    "FOR",
    "FORCE",
    "FOREIGN",
    "FORWARD",
    "FREEZE",
    "FROM",
    "FULL",
    "FUNCTION",
    "FUNCTIONS",
    "GENERATED",
    "GLOB",
    "GLOBAL",
    "GRANT",
    "GRANTED",
    "GROUP",
    "GROUPING",
    "GROUPING_ID",
    "GROUPS",
    "HANDLER",
    "HAVING",
    "HEADER",
    "HOLD",
    "HOUR",
    "HOURS",
    "IDENTITY",
    "IF",
    "IGNORE",
    "ILIKE",
    "IMMEDIATE",
    "IMMUTABLE",
    "IMPLICIT",
    "IMPORT",
    "IN",
    "INCLUDE",
    "INCLUDING",
    "INCREMENT",
    "INDEX",
    "INDEXES",
    "INHERIT",
    "INHERITS",
    "INITIALLY",
    "INLINE",
    "INNER",
    "INOUT",
    "INPUT",
    "INSENSITIVE",
    "INSERT",
    "INSTALL",
    "INSTEAD",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "INVOKER",
    "IS",
    "ISNULL",
    "ISOLATION",
    "JOIN",
    "JSON",
    "KEY",
    "LABEL",
    "LANGUAGE",
    "LARGE",
    "LAST",
    "LATERAL",
    "LEADING",
    "LEAKPROOF",
    "LEFT",
    "LEVEL",
    "LIKE",
    "LIMIT",
    "LISTEN",
    "LOAD",
    "LOCAL",
    "LOCATION",
    "LOCK",
    "LOCKED",
    "LOGGED",
    "MACRO",
    "MAP",
    "MAPPING",
    "MATCH",
    "MATCH_RECOGNIZE",
    "MATERIALIZED",
    "MAXVALUE",
    "MERGE",
    "METHOD",
    "MICROSECOND",
    "MICROSECONDS",
    "MILLENNIUM",
    "MILLISECOND",
    "MILLISECONDS",
    "MINUTE",
    "MINUTES",
    "MINVALUE",
    "MODE",
    "MONTH",
    "MONTHS",
    "MOVE",
    "NAME",
    "NAMES",
    "NATIONAL",
    "NATURAL",
    "NCHAR",
    "NEW",
    "NEXT",
    "NO",
    "NONE",
    "NOT",
    "NOTHING",
    "NOTIFY",
    "NOTNULL",
    "NOWAIT",
    "NULL",
    "NULLIF",
    "NULLS",
    "NUMERIC",
    "OBJECT",
    "OF",
    "OFF",
    "OFFSET",
    "OIDS",
    "OLD",
    "ON",
    "ONLY",
    "OPERATOR",
    "OPTION",
    "OPTIONS",
    "OR",
    "ORDER",
    "ORDINALITY",
    "OTHERS",
    "OUT",
    "OUTER",
    "OVER",
    "OVERLAPS",
    "OVERLAY",
    "OVERRIDING",
    "OWNED",
    "OWNER",
    "PARALLEL",
    "PARSER",
    "PARTIAL",
    "PARTITION",
    "PARTITIONED",
    "PASSING",
    "PASSWORD",
    "PERCENT",
    "PERSISTENT",
    "PIVOT",
    "PIVOT_LONGER",
    "PIVOT_WIDER",
    "PLACING",
    "PLANS",
    "POLICY",
    "POSITION",
    "POSITIONAL",
    "PRAGMA",
    "PRECEDING",
    "PRECISION",
    "PREPARE",
    "PREPARED",
    "PRESERVE",
    "PRIMARY",
    "PRIOR",
    "PRIVILEGES",
    "PROCEDURAL",
    "PROCEDURE",
    "PROGRAM",
    "PUBLICATION",
    "QUALIFY",
    "QUARTER",
    "QUARTERS",
    "QUOTE",
    "RANGE",
    "READ",
    "REAL",
    "REASSIGN",
    "RECHECK",
    "RECURSIVE",
    "REF",
    "REFERENCES",
    "REFERENCING",
    "REFRESH",
    "REINDEX",
    "RELATIVE",
    "RELEASE",
    "RENAME",
    "REPEATABLE",
    "REPLACE",
    "REPLICA",
    "RESET",
    "RESPECT",
    "RESTART",
    "RESTRICT",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLE",
    "ROLLBACK",
    "ROLLUP",
    "ROW",
    "ROWS",
    "RULE",
    "SAMPLE",
    "SAVEPOINT",
    "SCHEMA",
    "SCHEMAS",
    "SCOPE",
    "SCROLL",
    "SEARCH",
    "SECOND",
    "SECONDS",
    "SECRET",
    "SECURITY",
    "SELECT",
    "SEMI",
    "SEQUENCE",
    "SEQUENCES",
    "SERIALIZABLE",
    "SERVER",
    "SESSION",
    "SESSION_USER",
    "SET",
    "SETOF",
    "SETS",
    "SHARE",
    "SHOW",
    "SIMILAR",
    "SIMPLE",
    "SKIP",
    "SMALLINT",
    "SNAPSHOT",
    "SOME",
    "SORTED",
    "SQL",
    "STABLE",
    "STANDALONE",
    "START",
    "STATEMENT",
    "STATISTICS",
    "STDIN",
    "STDOUT",
    "STORAGE",
    "STORED",
    "STRICT",
    "STRIP",
    "STRUCT",
    "SUBSCRIPTION",
    "SUBSTRING",
    "SUMMARIZE",
    "SYMMETRIC",
    "SYSID",
    "SYSTEM",
    "TABLE",
    "TABLES",
    "TABLESAMPLE",
    "TABLESPACE",
    "TEMP",
    "TEMPLATE",
    "TEMPORARY",
    "TEXT",
    "THEN",
    "TIES",
    "TIME",
    "TIMESTAMP",
    "TO",
    "TRAILING",
    "TRANSACTION",
    "TRANSFORM",
    "TREAT",
    "TRIGGER",
    "TRIM",
    "TRUE",
    "TRUNCATE",
    "TRUSTED",
    "TRY_CAST",
    "TYPE",
    "TYPES",
    "UNBOUNDED",
    "UNCOMMITTED",
    "UNENCRYPTED",
    "UNION",
    "UNIQUE",
    "UNKNOWN",
    "UNLISTEN",
    "UNLOGGED",
    "UNPACK",
    "UNPIVOT",
    "UNTIL",
    "UPDATE",
    "USE",
    "USER",
    "USING",
    "VACUUM",
    "VALID",
    "VALIDATE",
    "VALIDATOR",
    "VALUE",
    "VALUES",
    "VARCHAR",
    "VARIABLE",
    "VARIADIC",
    "VARYING",
    "VERBOSE",
    "VERSION",
    "VIEW",
    "VIEWS",
    "VIRTUAL",
    "VOLATILE",
    "WEEK",
    "WEEKS",
    "WHEN",
    "WHERE",
    "WHITESPACE",
    "WINDOW",
    "WITH",
    "WITHIN",
    "WITHOUT",
    "WORK",
    "WRAPPER",
    "WRITE",
    "XML",
    "YEAR",
    "YEARS",
    "YES",
    "ZONE",
];

/// Reserved DuckDB keywords: identifiers equal to one of these must be quoted.
const DUCKDB_RESERVED: &[&str] = &[
    "ALL",
    "ANALYSE",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASYMMETRIC",
    "BOTH",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "CONSTRAINT",
    "CREATE",
    "DEFAULT",
    "DEFERRABLE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DO",
    "ELSE",
    "END",
    "EXCEPT",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INITIALLY",
    "INTERSECT",
    "INTO",
    "LATERAL",
    "LEADING",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "PIVOT",
    "PIVOT_LONGER",
    "PIVOT_WIDER",
    "PLACING",
    "PRIMARY",
    "QUALIFY",
    "REFERENCES",
    "RETURNING",
    "SELECT",
    "SHOW",
    "SOME",
    "SUMMARIZE",
    "SYMMETRIC",
    "TABLE",
    "THEN",
    "TO",
    "TRAILING",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UNPIVOT",
    "USING",
    "VARIADIC",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Common DuckDB functions (scalar, aggregate, window, list, struct, map,
/// string, date, JSON and table functions).
pub const DUCKDB_FUNCTIONS: &[&str] = &[
    "abs",
    "acos",
    "age",
    "any_value",
    "approx_count_distinct",
    "approx_quantile",
    "arg_max",
    "arg_min",
    "array_agg",
    "array_length",
    "array_value",
    "ascii",
    "asin",
    "atan",
    "atan2",
    "avg",
    "bar",
    "bit_count",
    "bit_length",
    "bool_and",
    "bool_or",
    "cardinality",
    "cbrt",
    "ceil",
    "ceiling",
    "chr",
    "coalesce",
    "concat",
    "concat_ws",
    "contains",
    "corr",
    "cos",
    "count",
    "count_star",
    "covar_pop",
    "covar_samp",
    "cume_dist",
    "current_database",
    "current_date",
    "current_schema",
    "current_schemas",
    "current_setting",
    "currval",
    "date_add",
    "date_diff",
    "date_part",
    "date_sub",
    "date_trunc",
    "datediff",
    "datepart",
    "datesub",
    "datetrunc",
    "day",
    "dayname",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "decade",
    "degrees",
    "dense_rank",
    "duckdb_columns",
    "duckdb_constraints",
    "duckdb_databases",
    "duckdb_extensions",
    "duckdb_functions",
    "duckdb_indexes",
    "duckdb_keywords",
    "duckdb_schemas",
    "duckdb_sequences",
    "duckdb_settings",
    "duckdb_tables",
    "duckdb_types",
    "duckdb_views",
    "ends_with",
    "enum_range",
    "epoch",
    "epoch_ms",
    "even",
    "exp",
    "extract",
    "factorial",
    "first",
    "first_value",
    "flatten",
    "floor",
    "format",
    "from_json",
    "gamma",
    "gcd",
    "gen_random_uuid",
    "generate_series",
    "glob",
    "greatest",
    "hash",
    "hour",
    "ifnull",
    "ilike_escape",
    "instr",
    "isfinite",
    "isinf",
    "isnan",
    "json",
    "json_array",
    "json_array_length",
    "json_contains",
    "json_extract",
    "json_extract_string",
    "json_group_array",
    "json_group_object",
    "json_keys",
    "json_object",
    "json_structure",
    "json_transform",
    "json_type",
    "json_valid",
    "kurtosis",
    "lag",
    "last",
    "last_day",
    "last_value",
    "lcase",
    "lcm",
    "lead",
    "least",
    "left",
    "length",
    "levenshtein",
    "lgamma",
    "like_escape",
    "list",
    "list_aggregate",
    "list_append",
    "list_concat",
    "list_contains",
    "list_distinct",
    "list_extract",
    "list_filter",
    "list_has",
    "list_position",
    "list_reduce",
    "list_reverse",
    "list_slice",
    "list_sort",
    "list_transform",
    "list_unique",
    "list_value",
    "ln",
    "log",
    "log10",
    "log2",
    "lower",
    "lpad",
    "ltrim",
    "make_date",
    "make_time",
    "make_timestamp",
    "map",
    "map_extract",
    "map_keys",
    "map_values",
    "max",
    "max_by",
    "md5",
    "mean",
    "median",
    "microsecond",
    "millisecond",
    "min",
    "min_by",
    "minute",
    "mode",
    "month",
    "monthname",
    "nextafter",
    "nextval",
    "now",
    "nth_value",
    "ntile",
    "nullif",
    "octet_length",
    "parquet_metadata",
    "parquet_scan",
    "parquet_schema",
    "percent_rank",
    "pi",
    "position",
    "pow",
    "power",
    "prefix",
    "printf",
    "quantile",
    "quantile_cont",
    "quantile_disc",
    "quarter",
    "radians",
    "random",
    "range",
    "rank",
    "read_csv",
    "read_csv_auto",
    "read_json",
    "read_json_auto",
    "read_parquet",
    "regexp_extract",
    "regexp_full_match",
    "regexp_matches",
    "regexp_replace",
    "regexp_split_to_array",
    "repeat",
    "replace",
    "reverse",
    "right",
    "round",
    "row",
    "row_number",
    "rpad",
    "rtrim",
    "second",
    "setseed",
    "sha256",
    "sign",
    "sin",
    "skewness",
    "split_part",
    "sqrt",
    "starts_with",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "strftime",
    "string_agg",
    "string_split",
    "strip_accents",
    "strlen",
    "strpos",
    "strptime",
    "struct_extract",
    "struct_insert",
    "struct_pack",
    "substr",
    "substring",
    "suffix",
    "sum",
    "tan",
    "time_bucket",
    "to_json",
    "to_timestamp",
    "today",
    "trim",
    "trunc",
    "try_cast",
    "try_strptime",
    "typeof",
    "ucase",
    "unicode",
    "unnest",
    "upper",
    "uuid",
    "var_pop",
    "var_samp",
    "variance",
    "version",
    "week",
    "weekday",
    "weekofyear",
    "year",
    "yearweek",
];

/// DuckDB type names.
pub const DUCKDB_TYPES: &[&str] = &[
    "BIGINT",
    "BIT",
    "BLOB",
    "BOOLEAN",
    "DATE",
    "DECIMAL",
    "DOUBLE",
    "ENUM",
    "FLOAT",
    "HUGEINT",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "LIST",
    "MAP",
    "SMALLINT",
    "STRUCT",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMP WITH TIME ZONE",
    "TIMESTAMPTZ",
    "TIMESTAMP_S",
    "TIMESTAMP_MS",
    "TIMESTAMP_NS",
    "TIMETZ",
    "TINYINT",
    "UBIGINT",
    "UHUGEINT",
    "UINTEGER",
    "UNION",
    "USMALLINT",
    "UTINYINT",
    "UUID",
    "VARCHAR",
    "VARINT",
    "BIGNUM",
    "GEOMETRY",
    "TEXT",
    "STRING",
    "REAL",
    "NUMERIC",
    "INT",
];

/// Lexical rules of DuckDB.
pub static DUCKDB_SYNTAX: SqlSyntax = SqlSyntax {
    keywords: DUCKDB_KEYWORDS,
    functions: DUCKDB_FUNCTIONS,
    data_types: DUCKDB_TYPES,
    identifier_quotes: &[('"', '"')],
    double_quoted_strings: false,
    backslash_escapes: false,
    e_strings: true,
    n_strings: false,
    x_strings: false,
    dollar_quoting: true,
    hash_comments: false,
    nested_block_comments: true,
    dash_comment_needs_space: false,
    question_params: true,
    dollar_params: true,
    at_variables: false,
    batch_separator: None,
    delimiter_command: false,
    compound_blocks: false,
    extra_ident_chars: "$",
    // identifiers are case-insensitive (even quoted ones) but keep their case
    ident_case: IdentCase::Preserve,
    case_sensitive_names: false,
};

/// True when `ident` must be quoted.
pub(crate) fn needs_quoting(ident: &str) -> bool {
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
    DUCKDB_RESERVED
        .iter()
        .any(|k| k.eq_ignore_ascii_case(ident))
        || keywords::is_reserved(ident)
}

/// Quote when needed.
pub(crate) fn quote_ident(ident: &str) -> String {
    if needs_quoting(ident) {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
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

const MAINTENANCE_DB: &[ObjectKind] = &[ObjectKind::Database, ObjectKind::Schema];
const MAINTENANCE_TABLES: &[ObjectKind] =
    &[ObjectKind::Database, ObjectKind::Schema, ObjectKind::Table];

/// The DuckDB dialect.
pub struct DuckDbDialect {
    ddl: DuckDbDdl,
}

impl DuckDbDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        DuckDbDialect {
            ddl: DuckDbDdl::new(),
        }
    }
}

impl Default for DuckDbDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for DuckDbDialect {
    fn kind(&self) -> DbKind {
        DbKind::DuckDb
    }

    fn syntax(&self) -> &SqlSyntax {
        &DUCKDB_SYNTAX
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn catalog_folders(&self) -> Vec<dbiji_core::catalog::CatalogFolder> {
        crate::catalogs::folders()
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        needs_quoting(ident)
    }

    /// `database.schema.name` for objects of another attached database.
    fn qualified_ref(&self, obj: &ObjectRef) -> String {
        match obj.database.as_deref().filter(|d| !d.is_empty()) {
            Some(db) => {
                format!(
                    "{}.{}",
                    quote_ident(db),
                    qualified(Some(obj.schema.as_deref().unwrap_or("main")), &obj.name)
                )
            }
            None => qualified(obj.schema.as_deref(), &obj.name),
        }
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len() * 4 + 10);
        s.push('\'');
        for byte in b {
            s.push_str(&format!("\\x{byte:02X}"));
        }
        s.push_str("'::BLOB");
        s
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let _ = type_name;
        match value {
            Value::Float(f) if f.is_nan() => "'NaN'::DOUBLE".into(),
            Value::Float(f) if f.is_infinite() => {
                if *f > 0.0 {
                    "'Infinity'::DOUBLE".into()
                } else {
                    "'-Infinity'::DOUBLE".into()
                }
            }
            Value::Date(d) => format!("DATE '{}'", d.format("%Y-%m-%d")),
            Value::DateTime(_) => format!("TIMESTAMP '{}'", value.to_text()),
            Value::DateTimeTz(dt) => {
                format!("TIMESTAMPTZ '{}'", dt.format("%Y-%m-%d %H:%M:%S%.f%:z"))
            }
            Value::Time(_) => format!("TIME '{}'", value.to_text()),
            Value::Uuid(u) => format!("'{}'::UUID", u.hyphenated()),
            Value::Json(s) => format!("{}::JSON", string_literal(s)),
            Value::Array(items) => {
                let parts: Vec<String> = items.iter().map(|v| self.literal(v, None)).collect();
                format!("[{}]", parts.join(", "))
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
        format!("TRUNCATE {}", self.qualified_ref(table))
    }

    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        vec![
            MaintenanceOp {
                id: "checkpoint",
                label: "Checkpoint",
                description: "Write the write-ahead log into the database file",
                applies_to: MAINTENANCE_DB,
                options: &[(
                    "force",
                    "Abort running transactions (FORCE CHECKPOINT)",
                    false,
                )],
                needs_autocommit: true,
            },
            MaintenanceOp {
                id: "analyze",
                label: "Analyze",
                description: "Recompute table statistics used by the optimizer",
                applies_to: MAINTENANCE_TABLES,
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "vacuum",
                label: "Vacuum",
                description: "Recompute statistics (VACUUM ANALYZE); DuckDB reclaims space automatically",
                applies_to: MAINTENANCE_TABLES,
                options: &[],
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
        let all = [ObjectRef::new(ObjectKind::Database, None, "")];
        let targets = if targets.is_empty() {
            &all[..]
        } else {
            targets
        };
        let mut out = Vec::new();
        for t in targets {
            let sql = match (op, t.kind) {
                ("checkpoint", ObjectKind::Database) if t.name.is_empty() => {
                    if options.get("force").copied().unwrap_or(false) {
                        "FORCE CHECKPOINT".into()
                    } else {
                        "CHECKPOINT".into()
                    }
                }
                ("checkpoint", _) => {
                    // A schema or table without its database: the current database
                    // (the schema's name is no database name).
                    let db = if t.kind == ObjectKind::Database {
                        Some(t.name.as_str())
                    } else {
                        t.database.as_deref()
                    };
                    let force = if options.get("force").copied().unwrap_or(false) {
                        "FORCE "
                    } else {
                        ""
                    };
                    match db {
                        Some(db) => format!("{force}CHECKPOINT {}", quote_ident(db)),
                        None => format!("{force}CHECKPOINT"),
                    }
                }
                ("analyze", ObjectKind::Table) => format!("ANALYZE {}", self.qualified_ref(t)),
                ("analyze", _) => "ANALYZE".into(),
                ("vacuum", ObjectKind::Table) => {
                    format!("VACUUM ANALYZE {}", self.qualified_ref(t))
                }
                ("vacuum", _) => "VACUUM ANALYZE".into(),
                _ => continue,
            };
            if !out.contains(&sql) {
                out.push(sql);
            }
        }
        out
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        crate::convert::kind_of_name(type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_literals() {
        let d = DuckDbDialect::new();
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("select"), "\"select\"");
        assert_eq!(d.quote_ident("My Table"), "\"My Table\"");
        assert_eq!(d.qualified(Some("main"), "t"), "main.t");
        assert_eq!(
            d.literal(&Value::bytes(vec![0u8, 255]), None),
            "'\\x00\\xFF'::BLOB"
        );
        assert_eq!(d.literal(&Value::Bool(true), None), "TRUE");
        let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        assert_eq!(d.literal(&Value::Date(date), None), "DATE '2024-01-02'");
        let arr = Value::Array(vec![Value::Int(1), Value::text("a")].into_boxed_slice());
        assert_eq!(d.literal(&arr, None), "[1, 'a']");
        assert_eq!(
            d.truncate_table(&ObjectRef::table(Some("main".into()), "t")),
            "TRUNCATE main.t"
        );
        assert_eq!(d.placeholder(2), "?");
    }

    #[test]
    fn upsert_and_maintenance() {
        let d = DuckDbDialect::new();
        let sql = d
            .upsert(
                "t",
                &["id".into(), "v".into()],
                &["id".into()],
                &["?".into(), "?".into()],
            )
            .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO t (id, v) VALUES (?, ?) ON CONFLICT (id) DO UPDATE SET v = excluded.v"
        );
        let opts = BTreeMap::new();
        assert_eq!(
            d.maintenance_sql("checkpoint", &[], &opts),
            vec!["CHECKPOINT"]
        );
        let t = ObjectRef::table(Some("main".into()), "t");
        assert_eq!(
            d.maintenance_sql("analyze", std::slice::from_ref(&t), &opts),
            vec!["ANALYZE main.t"]
        );
        assert_eq!(
            d.maintenance_sql("vacuum", &[t], &opts),
            vec!["VACUUM ANALYZE main.t"]
        );
    }
}
