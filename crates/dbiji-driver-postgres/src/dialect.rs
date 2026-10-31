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
    plsql_blocks: false,
    newline_ends_statement: false,
    extra_ident_chars: "$",
    ident_case: IdentCase::Lower,
    case_sensitive_names: false,
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

/// Built-in (`pg_catalog`) types: a cast to them works whatever the `search_path`.
/// A cast to a user type (enum, domain, extension type) is left out: unqualified it
/// fails when its schema is not on the path, and the untyped literal takes the
/// column's type anyway (`col = 'happy'`, `SET col = 'happy'`, `VALUES ('happy')`).
fn is_builtin_type(t: &str) -> bool {
    let base = t.trim().to_ascii_lowercase();
    let base = base.strip_prefix("pg_catalog.").unwrap_or(&base);
    let base = base.trim_end_matches("[]").split('(').next().unwrap_or("").trim();
    // Exact names: a user enum `region` or `price_range` is no built-in type.
    let base = base.strip_suffix(" with time zone").or_else(|| base.strip_suffix(" without time zone")).unwrap_or(base).trim();
    is_plain_type(base)
        || matches!(
            base,
            "int4range"
                | "int8range"
                | "numrange"
                | "tsrange"
                | "tstzrange"
                | "daterange"
                | "int4multirange"
                | "int8multirange"
                | "nummultirange"
                | "tsmultirange"
                | "tstzmultirange"
                | "datemultirange"
                | "time"
                | "timetz"
                | "timestamp"
                | "timestamptz"
                | "bit"
                | "bit varying"
                | "regclass"
                | "regtype"
                | "regproc"
                | "regprocedure"
                | "regoper"
                | "regoperator"
                | "regconfig"
                | "regdictionary"
                | "regnamespace"
                | "regrole"
                | "regcollation"
                | "interval"
                | "json"
                | "jsonb"
                | "uuid"
                | "date"
                | "bytea"
                | "inet"
                | "cidr"
                | "macaddr"
                | "macaddr8"
                | "money"
                | "varbit"
                | "xml"
                | "point"
                | "line"
                | "lseg"
                | "box"
                | "path"
                | "polygon"
                | "circle"
                | "tsvector"
                | "tsquery"
                | "oid"
                | "pg_lsn"
                | "txid_snapshot"
                | "pg_snapshot"
                | "float4"
                | "float8"
                | "double precision"
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

    fn catalog_folders(&self) -> Vec<dbiji_core::catalog::CatalogFolder> {
        crate::catalogs::folders(self.kind == DbKind::CockroachDb)
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
        let typed = type_name.filter(|t| !is_plain_type(t) && is_builtin_type(t));
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

    // The new row comes back over the binary protocol: timestamptz in UTC, money
    // without its symbol, tsvector / point / ltree / vector as bytes. As text they
    // read like the rows of a query.
    fn returning_column(&self, column: &str, type_name: &str) -> String {
        let q = self.quote_ident(column);
        match crate::types::kind_from_type_name(type_name) {
            DataKind::DateTimeTz | DataKind::Decimal | DataKind::Other => format!("{q}::text AS {q}"),
            _ => q,
        }
    }

    fn returned_value(&self, type_name: &str, value: Value) -> Value {
        let conv = match crate::types::kind_from_type_name(type_name) {
            DataKind::DateTimeTz => crate::types::Conv::TimestampTz,
            DataKind::Decimal if !type_name.eq_ignore_ascii_case("money") => crate::types::Conv::Numeric,
            _ => return value,
        };
        match value.as_str() {
            Some(s) => crate::decode::text_value(&conv, s),
            None => value,
        }
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn truncate_options(&self) -> &'static [(&'static str, &'static str, bool)] {
        const CASCADE: (&str, &str, bool) = ("cascade", "Cascade: also truncate the tables that reference it by a foreign key (CASCADE)", false);
        if self.kind == DbKind::CockroachDb {
            return &[CASCADE];
        }
        &[
            ("only", "Only the table itself, not its child tables (ONLY)", false),
            ("restart_identity", "Restart identity: the sequences of its columns start over (RESTART IDENTITY)", false),
            CASCADE,
        ]
    }

    /// One statement for all the tables: tables referencing each other are emptied together.
    fn truncate_sql(&self, tables: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        if tables.is_empty() {
            return Vec::new();
        }
        let on = |k: &str| options.get(k).copied().unwrap_or(false);
        let only = on("only") && self.kind != DbKind::CockroachDb;
        let names: Vec<String> = tables.iter().map(|t| format!("{}{}", if only { "ONLY " } else { "" }, self.qualified_ref(t))).collect();
        let mut sql = format!("TRUNCATE TABLE {}", names.join(", "));
        if on("restart_identity") && self.kind != DbKind::CockroachDb {
            sql.push_str(" RESTART IDENTITY");
        }
        if on("cascade") {
            sql.push_str(" CASCADE");
        }
        vec![sql]
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
            for (id, label, description) in [
                ("disable_triggers", "Disable triggers", "Stop the triggers of the table (ALTER TABLE … DISABLE TRIGGER)"),
                ("enable_triggers", "Enable triggers", "Turn the triggers of the table on again"),
            ] {
                ops.push(MaintenanceOp {
                    id,
                    label,
                    description,
                    applies_to: &[ObjectKind::Table, ObjectKind::Partition, ObjectKind::Trigger],
                    options: &[("user_only", "Only user triggers (keep foreign key triggers)", true)],
                    needs_autocommit: false,
                });
            }
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
                "disable_triggers" | "enable_triggers" => {
                    let verb = if op == "disable_triggers" { "DISABLE" } else { "ENABLE" };
                    match (&t.kind, &t.table) {
                        (ObjectKind::Trigger, Some(table)) => {
                            out.push(format!("ALTER TABLE {} {verb} TRIGGER {}", qualified(t.schema.as_deref(), table), quote_ident(&t.name)));
                        }
                        _ => {
                            let which = if on("user_only") { "USER" } else { "ALL" };
                            out.push(format!("ALTER TABLE {} {verb} TRIGGER {which}", qualified(t.schema.as_deref(), &t.name)));
                        }
                    }
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
    fn truncate_with_options() {
        let d = PgDialect::new();
        let t = [ObjectRef::table(Some("public".into()), "orders"), ObjectRef::table(Some("public".into()), "Lines")];
        let opts = |on: &[&str]| on.iter().map(|k| (k.to_string(), true)).collect::<BTreeMap<_, _>>();
        assert_eq!(d.truncate_sql(&t, &opts(&[])), ["TRUNCATE TABLE public.orders, public.\"Lines\""]);
        assert_eq!(d.truncate_sql(&t, &opts(&["only", "restart_identity", "cascade"])), ["TRUNCATE TABLE ONLY public.orders, ONLY public.\"Lines\" RESTART IDENTITY CASCADE"]);
        let c = PgDialect::cockroach();
        assert_eq!(c.truncate_options().len(), 1);
        assert_eq!(c.truncate_sql(&t[..1], &opts(&["only", "restart_identity", "cascade"])), ["TRUNCATE TABLE public.orders CASCADE"]);
    }

    #[test]
    fn returned_rows_read_like_query_rows() {
        let d = PgDialect::new();
        assert_eq!(d.returning_column("id", "int4"), "id");
        assert_eq!(d.returning_column("at", "timestamptz"), "at::text AS at");
        assert_eq!(d.returning_column("doc", "tsvector"), "doc::text AS doc");
        match d.returned_value("timestamptz", Value::text("2026-10-21 05:12:31.9+07")) {
            Value::DateTimeTz(t) => assert_eq!(t.offset().local_minus_utc(), 7 * 3600),
            v => panic!("{v:?}"),
        }
        assert_eq!(d.returned_value("money", Value::text("$1,234.50")), Value::text("$1,234.50"));
        assert_eq!(d.returned_value("numeric", Value::text("12.50")), Value::decimal("12.50"));
        assert_eq!(d.returned_value("int4", Value::Int(3)), Value::Int(3));
    }

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
        // user types (enum, domain, extension): no cast, the column's type applies
        assert_eq!(d.literal(&Value::text("happy"), Some("Mood")), "'happy'");
        for user in ["region", "price_range", "timezone_kind", "bitflag", "registration_status"] {
            assert!(!is_builtin_type(user), "{user}");
        }
        for builtin in ["tstzrange", "timestamp(3) with time zone", "time without time zone", "regclass", "bit varying(8)", "int4[]"] {
            assert!(is_builtin_type(builtin), "{builtin}");
        }
        assert_eq!(d.literal(&Value::text("SRID=4326;POINT(1 2)"), Some("geometry")), "'SRID=4326;POINT(1 2)'");
        assert_eq!(d.literal(&Value::text("10.0.0.1"), Some("inet")), "'10.0.0.1'::inet");
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

    #[test]
    fn trigger_maintenance() {
        let d = PgDialect::new();
        assert!(d.maintenance_ops().iter().any(|o| o.id == "disable_triggers"));
        let t = ObjectRef::table(Some("app".into()), "orders");
        let mut o = BTreeMap::new();
        o.insert("user_only".to_string(), true);
        assert_eq!(d.maintenance_sql("disable_triggers", std::slice::from_ref(&t), &o), vec!["ALTER TABLE app.orders DISABLE TRIGGER USER"]);
        o.insert("user_only".to_string(), false);
        assert_eq!(d.maintenance_sql("enable_triggers", std::slice::from_ref(&t), &o), vec!["ALTER TABLE app.orders ENABLE TRIGGER ALL"]);
        let trig = ObjectRef::child(ObjectKind::Trigger, &t, "audit");
        assert_eq!(d.maintenance_sql("disable_triggers", &[trig], &o), vec!["ALTER TABLE app.orders DISABLE TRIGGER audit"]);
    }
}
