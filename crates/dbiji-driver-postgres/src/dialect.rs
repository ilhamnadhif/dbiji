//! [`PgDialect`]: PostgreSQL (and CockroachDB) SQL dialect.

use crate::ddl::PgDdl;
use crate::keywords;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::{DataKind, DbKind, Value};
use std::collections::BTreeMap;

static PG_SYNTAX: SqlSyntax = SqlSyntax {
    keywords: keywords::ALL_KEYWORDS,
    functions: keywords::FUNCTIONS,
    data_types: keywords::DATA_TYPES,
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
};

/// Lexical description of PostgreSQL, usable without a dialect instance
/// (tokenizer, highlighter, statement splitter).
pub fn syntax() -> &'static SqlSyntax {
    &PG_SYNTAX
}

/// PostgreSQL dialect: quoting, literals with casts, `$n` placeholders,
/// `ON CONFLICT` upserts, maintenance operations and DDL generation.
pub struct PgDialect {
    kind: DbKind,
    ddl: PgDdl,
}

impl Default for PgDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl PgDialect {
    /// Dialect for PostgreSQL.
    pub fn new() -> Self {
        PgDialect { kind: DbKind::Postgres, ddl: PgDdl::new() }
    }

    /// Dialect for CockroachDB (PostgreSQL syntax, fewer maintenance operations).
    pub fn cockroach() -> Self {
        PgDialect { kind: DbKind::CockroachDb, ddl: PgDdl::new() }
    }

    /// The DDL generator.
    pub fn pg_ddl(&self) -> &PgDdl {
        &self.ddl
    }
}

/// Identifier needs quotes under PostgreSQL rules (lower case folding,
/// `quote_ident` keyword categories).
pub(crate) fn pg_needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    match chars.next() {
        None => return true,
        Some(c) if !(c == '_' || c.is_ascii_lowercase()) => return true,
        _ => {}
    }
    if !ident.chars().all(|c| c == '_' || c == '$' || c.is_ascii_lowercase() || c.is_ascii_digit()) {
        return true;
    }
    keywords::needs_keyword_quoting(ident)
}

/// Quote an identifier only when needed.
pub(crate) fn quote_ident(ident: &str) -> String {
    if pg_needs_quoting(ident) { format!("\"{}\"", ident.replace('"', "\"\"")) } else { ident.to_string() }
}

/// `schema.name` with quoting as needed.
pub(crate) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(s), quote_ident(name)),
        _ => quote_ident(name),
    }
}

/// Standard conforming string literal (`'it''s'`).
pub(crate) fn string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Type names whose literals need no cast.
fn is_plain_type(t: &str) -> bool {
    let base = t.trim().to_ascii_lowercase();
    let base = base.split('(').next().unwrap_or("").trim().to_string();
    matches!(
        base.as_str(),
        "" | "text"
            | "varchar"
            | "character varying"
            | "char"
            | "character"
            | "bpchar"
            | "name"
            | "citext"
            | "int2"
            | "int4"
            | "int8"
            | "smallint"
            | "integer"
            | "int"
            | "bigint"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "numeric"
            | "decimal"
            | "real"
            | "float4"
            | "float8"
            | "double precision"
            | "bool"
            | "boolean"
    )
}

/// Cast target text for a type name (quoted when it is a single mixed-case identifier).
fn cast_type(t: &str) -> String {
    let t = t.trim();
    let simple = t.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    if simple && t.chars().any(|c| c.is_ascii_uppercase()) { format!("\"{}\"", t.replace('"', "\"\"")) } else { t.to_string() }
}

impl Dialect for PgDialect {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn syntax(&self) -> &SqlSyntax {
        &PG_SYNTAX
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        pg_needs_quoting(ident)
    }

    fn string_literal(&self, s: &str) -> String {
        string_literal(s)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        format!("'\\x{}'::bytea", hex::encode(b))
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let typed = type_name.filter(|t| !is_plain_type(t));
        let cast = |lit: String, default: Option<&str>| -> String {
            match typed.or(default) {
                Some(t) => format!("{lit}::{}", cast_type(t)),
                None => lit,
            }
        };
        match value {
            Value::Null => "NULL".into(),
            Value::Bool(b) => {
                if *b {
                    "TRUE".into()
                } else {
                    "FALSE".into()
                }
            }
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f}"),
            Value::Float(f) => cast(string_literal(&crate::decode::pg_float_text(*f)), Some("float8")),
            Value::Decimal(s) => {
                if dbiji_core::value::is_decimal_text(s) && s.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c)) {
                    s.to_string()
                } else {
                    cast(string_literal(s), Some("numeric"))
                }
            }
            Value::Bytes(b) => cast(format!("'\\x{}'", hex::encode(b)), Some("bytea")),
            Value::Uuid(u) => cast(string_literal(&u.hyphenated().to_string()), Some("uuid")),
            Value::Date(d) => cast(string_literal(&crate::decode::date_text(*d)), Some("date")),
            Value::Time(t) => cast(string_literal(&crate::decode::time_text(*t)), Some("time")),
            Value::DateTime(dt) => cast(string_literal(&crate::decode::timestamp_text(*dt)), Some("timestamp")),
            Value::DateTimeTz(_) => {
                let text = crate::params::param_text(value, &postgres_types::Type::TIMESTAMPTZ).map(|c| c.into_owned()).unwrap_or_default();
                cast(string_literal(&text), Some("timestamptz"))
            }
            Value::Json(s) => cast(string_literal(s), None),
            Value::Array(items) => {
                let lit = crate::params::array_literal(items, &postgres_types::Type::TEXT);
                cast(string_literal(&lit), None)
            }
            Value::Text(s) => cast(string_literal(s), None),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }

    fn upsert(&self, table: &str, columns: &[String], key_columns: &[String], values: &[String]) -> Option<String> {
        if key_columns.is_empty() || columns.is_empty() || columns.len() != values.len() {
            return None;
        }
        let updates: Vec<String> = columns.iter().filter(|c| !key_columns.contains(c)).map(|c| format!("{c} = EXCLUDED.{c}")).collect();
        let action = if updates.is_empty() { "DO NOTHING".to_string() } else { format!("DO UPDATE SET {}", updates.join(", ")) };
        Some(format!("INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) {action}", columns.join(", "), values.join(", "), key_columns.join(", ")))
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        let mut ops = Vec::new();
        if self.kind == DbKind::Postgres {
            ops.push(MaintenanceOp {
                id: "vacuum",
                label: "Vacuum",
                description: "Reclaim storage occupied by dead tuples and optionally update statistics",
                applies_to: &[ObjectKind::Table, ObjectKind::MaterializedView, ObjectKind::Partition, ObjectKind::Database],
                options: &[
                    ("full", "Full (rewrite the table, exclusive lock)", false),
                    ("freeze", "Freeze", false),
                    ("analyze", "Analyze", true),
                    ("verbose", "Verbose", false),
                    ("skip_locked", "Skip locked relations", false),
                    ("disable_page_skipping", "Disable page skipping", false),
                ],
                needs_autocommit: true,
            });
        }
        ops.push(MaintenanceOp {
            id: "analyze",
            label: "Analyze",
            description: "Collect statistics for the query planner",
            applies_to: &[ObjectKind::Table, ObjectKind::MaterializedView, ObjectKind::Partition, ObjectKind::Database],
            options: &[("verbose", "Verbose", false), ("skip_locked", "Skip locked relations", false)],
            needs_autocommit: false,
        });
        if self.kind == DbKind::Postgres {
            ops.push(MaintenanceOp {
                id: "reindex",
                label: "Reindex",
                description: "Rebuild indexes",
                applies_to: &[ObjectKind::Table, ObjectKind::Index, ObjectKind::MaterializedView, ObjectKind::Partition, ObjectKind::Schema, ObjectKind::Database],
                options: &[("concurrently", "Concurrently (PostgreSQL 12+)", false), ("verbose", "Verbose", false)],
                needs_autocommit: true,
            });
            ops.push(MaintenanceOp {
                id: "cluster",
                label: "Cluster",
                description: "Physically reorder the table according to its clustered index",
                applies_to: &[ObjectKind::Table, ObjectKind::MaterializedView],
                options: &[("verbose", "Verbose", false)],
                needs_autocommit: false,
            });
            ops.push(MaintenanceOp {
                id: "refresh",
                label: "Refresh materialized view",
                description: "Replace the contents of a materialized view",
                applies_to: &[ObjectKind::MaterializedView],
                options: &[("concurrently", "Concurrently (requires a unique index)", false), ("with_no_data", "With no data", false)],
                needs_autocommit: false,
            });
        }
        ops
    }

    fn maintenance_sql(&self, op: &str, targets: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        let on = |k: &str| options.get(k).copied().unwrap_or(false);
        let target_sql = |t: &ObjectRef| -> Option<String> {
            match t.kind {
                ObjectKind::Database | ObjectKind::Schema => None,
                _ => Some(qualified(t.schema.as_deref(), &t.name)),
            }
        };
        let mut out = Vec::new();
        for t in targets {
            match op {
                "vacuum" => {
                    let mut opts = Vec::new();
                    for (k, sql) in [
                        ("full", "FULL"),
                        ("freeze", "FREEZE"),
                        ("verbose", "VERBOSE"),
                        ("analyze", "ANALYZE"),
                        ("skip_locked", "SKIP_LOCKED"),
                        ("disable_page_skipping", "DISABLE_PAGE_SKIPPING"),
                    ] {
                        if on(k) {
                            opts.push(sql);
                        }
                    }
                    let mut s = "VACUUM".to_string();
                    if !opts.is_empty() {
                        s.push_str(&format!(" ({})", opts.join(", ")));
                    }
                    if let Some(name) = target_sql(t) {
                        s.push(' ');
                        s.push_str(&name);
                    }
                    out.push(s);
                }
                "analyze" => {
                    let mut opts = Vec::new();
                    if on("verbose") {
                        opts.push("VERBOSE");
                    }
                    if on("skip_locked") {
                        opts.push("SKIP_LOCKED");
                    }
                    let mut s = "ANALYZE".to_string();
                    if !opts.is_empty() {
                        s.push_str(&format!(" ({})", opts.join(", ")));
                    }
                    if let Some(name) = target_sql(t) {
                        s.push(' ');
                        s.push_str(&name);
                    }
                    out.push(s);
                }
                "reindex" => {
                    let mut s = "REINDEX".to_string();
                    if on("verbose") {
                        s.push_str(" (VERBOSE)");
                    }
                    let (what, name) = match t.kind {
                        ObjectKind::Index => ("INDEX", qualified(t.schema.as_deref(), &t.name)),
                        ObjectKind::Schema => ("SCHEMA", quote_ident(&t.name)),
                        ObjectKind::Database => ("DATABASE", quote_ident(&t.name)),
                        _ => ("TABLE", qualified(t.schema.as_deref(), &t.name)),
                    };
                    s.push(' ');
                    s.push_str(what);
                    if on("concurrently") {
                        s.push_str(" CONCURRENTLY");
                    }
                    s.push(' ');
                    s.push_str(&name);
                    out.push(s);
                }
                "cluster" => {
                    let mut s = "CLUSTER".to_string();
                    if on("verbose") {
                        s.push_str(" VERBOSE");
                    }
                    if let Some(name) = target_sql(t) {
                        s.push(' ');
                        s.push_str(&name);
                    }
                    out.push(s);
                }
                "refresh" => {
                    let mut s = "REFRESH MATERIALIZED VIEW".to_string();
                    if on("concurrently") && !on("with_no_data") {
                        s.push_str(" CONCURRENTLY");
                    }
                    s.push(' ');
                    s.push_str(&qualified(t.schema.as_deref(), &t.name));
                    if on("with_no_data") {
                        s.push_str(" WITH NO DATA");
                    }
                    out.push(s);
                }
                _ => {}
            }
        }
        out
    }

    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        let role = match user {
            Some(u) => quote_ident(u),
            None => "CURRENT_USER".to_string(),
        };
        if self.kind == DbKind::CockroachDb {
            return Some(format!("ALTER USER {role} WITH PASSWORD {}", string_literal(new_password)));
        }
        // Send a SCRAM-SHA-256 verifier instead of the clear text password so the
        // password never appears in server logs or pg_stat_activity.
        let verifier = postgres_protocol::password::scram_sha_256(new_password.as_bytes());
        Some(format!("ALTER ROLE {role} PASSWORD {}", string_literal(&verifier)))
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        crate::types::kind_from_type_name(type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn quoting() {
        let d = PgDialect::new();
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("Users"), "\"Users\"");
        assert_eq!(d.quote_ident("user"), "\"user\"");
        assert_eq!(d.quote_ident("my table"), "\"my table\"");
        assert_eq!(d.quote_ident("a$b"), "a$b");
        assert_eq!(d.quote_ident("1a"), "\"1a\"");
        assert_eq!(d.quote_ident("name"), "name");
        assert_eq!(d.quote_ident("é"), "\"é\"");
        assert_eq!(d.qualified(Some("public"), "Order"), "public.\"Order\"");
        assert_eq!(d.placeholder(3), "$3");
        assert!(syntax().dollar_quoting);
    }

    #[test]
    fn literals_with_casts() {
        let d = PgDialect::new();
        assert_eq!(d.literal(&Value::json("{\"a\":1}"), Some("jsonb")), "'{\"a\":1}'::jsonb");
        assert_eq!(d.literal(&Value::json("{}"), None), "'{}'");
        assert_eq!(d.literal(&Value::bytes(vec![1u8, 0xab]), None), "'\\x01ab'::bytea");
        assert_eq!(d.literal(&Value::text("1 day"), Some("interval")), "'1 day'::interval");
        assert_eq!(d.literal(&Value::text("x"), Some("varchar(10)")), "'x'");
        assert_eq!(d.literal(&Value::text("happy"), Some("Mood")), "'happy'::\"Mood\"");
        assert_eq!(d.literal(&Value::text("SRID=4326;POINT(1 2)"), Some("geometry")), "'SRID=4326;POINT(1 2)'::geometry");
        assert_eq!(d.literal(&Value::text("{1,2}"), Some("int4[]")), "'{1,2}'::int4[]");
        let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        assert_eq!(d.literal(&Value::Date(date), None), "'2024-02-29'::date");
        assert_eq!(d.literal(&Value::Float(f64::NAN), None), "'NaN'::float8");
        assert_eq!(d.literal(&Value::Int(-5), Some("int4")), "-5");
        assert_eq!(d.literal(&Value::text("it's"), None), "'it''s'");
        let arr = Value::Array(vec![Value::Int(1), Value::Null].into_boxed_slice());
        assert_eq!(d.literal(&arr, Some("integer[]")), "'{1,NULL}'::integer[]");
    }

    #[test]
    fn upsert_sql() {
        let d = PgDialect::new();
        let s = d.upsert("public.t", &["id".into(), "name".into()], &["id".into()], &["$1".into(), "$2".into()]).unwrap();
        assert_eq!(s, "INSERT INTO public.t (id, name) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name");
        let s = d.upsert("t", &["id".into()], &["id".into()], &["1".into()]).unwrap();
        assert!(s.ends_with("ON CONFLICT (id) DO NOTHING"));
        assert!(d.upsert("t", &["a".into()], &[], &["1".into()]).is_none());
    }

    #[test]
    fn maintenance() {
        let d = PgDialect::new();
        let t = ObjectRef::table(Some("s".into()), "T");
        let mut o = BTreeMap::new();
        o.insert("full".to_string(), true);
        o.insert("analyze".to_string(), true);
        assert_eq!(d.maintenance_sql("vacuum", std::slice::from_ref(&t), &o), vec!["VACUUM (FULL, ANALYZE) s.\"T\""]);
        let mut o = BTreeMap::new();
        o.insert("concurrently".to_string(), true);
        assert_eq!(d.maintenance_sql("reindex", std::slice::from_ref(&t), &o), vec!["REINDEX TABLE CONCURRENTLY s.\"T\""]);
        let db = ObjectRef::new(ObjectKind::Database, None, "app");
        assert_eq!(d.maintenance_sql("vacuum", std::slice::from_ref(&db), &BTreeMap::new()), vec!["VACUUM"]);
        assert_eq!(d.maintenance_sql("reindex", &[db], &BTreeMap::new()), vec!["REINDEX DATABASE app"]);
        let mv = ObjectRef::new(ObjectKind::MaterializedView, Some("s".into()), "mv");
        assert_eq!(d.maintenance_sql("refresh", &[mv], &o), vec!["REFRESH MATERIALIZED VIEW CONCURRENTLY s.mv"]);
        assert!(d.maintenance_ops().iter().any(|op| op.id == "vacuum" && op.needs_autocommit));
        assert!(PgDialect::cockroach().maintenance_ops().iter().all(|op| op.id != "vacuum"));
    }

    #[test]
    fn password_change_uses_scram_verifier() {
        let d = PgDialect::new();
        let s = d.change_password_sql(None, "s3cret").unwrap();
        assert!(s.starts_with("ALTER ROLE CURRENT_USER PASSWORD 'SCRAM-SHA-256$4096:"), "{s}");
        assert!(!s.contains("s3cret"));
        let s = d.change_password_sql(Some("App"), "x").unwrap();
        assert!(s.starts_with("ALTER ROLE \"App\" PASSWORD 'SCRAM-SHA-256$"));
    }

    #[test]
    fn type_kinds() {
        let d = PgDialect::new();
        assert_eq!(d.type_kind("bit(1)"), DataKind::Bit);
        assert_eq!(d.type_kind("timestamp with time zone"), DataKind::DateTimeTz);
        assert_eq!(d.type_kind("_text"), DataKind::Array);
    }
}
