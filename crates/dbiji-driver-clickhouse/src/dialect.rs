//! ClickHouse SQL dialect.

use crate::ddl::ClickHouseDdl;
use crate::keywords;
use crate::types::{ChType, parse_type};
use chrono::Timelike;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::value::{DataKind, Value, is_decimal_text};
use dbiji_core::DbKind;
use std::collections::BTreeMap;

/// Lexical rules of ClickHouse SQL.
pub const fn clickhouse_syntax() -> SqlSyntax {
    SqlSyntax {
        keywords: keywords::KEYWORDS,
        functions: keywords::FUNCTIONS,
        data_types: keywords::TYPES,
        identifier_quotes: &[('`', '`'), ('"', '"')],
        double_quoted_strings: false,
        backslash_escapes: true,
        e_strings: false,
        n_strings: false,
        x_strings: true,
        dollar_quoting: false,
        hash_comments: true,
        nested_block_comments: false,
        dash_comment_needs_space: false,
        question_params: true,
        dollar_params: false,
        at_variables: false,
        batch_separator: None,
        delimiter_command: false,
        compound_blocks: false,
        extra_ident_chars: "",
        ident_case: IdentCase::Preserve,
    }
}

/// ClickHouse dialect: backtick identifiers, backslash escapes, `?`
/// placeholders (substituted client side by the driver), no upsert and no
/// `RETURNING`.
pub struct ClickHouseDialect {
    syntax: SqlSyntax,
    ddl: ClickHouseDdl,
}

impl ClickHouseDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        ClickHouseDialect { syntax: clickhouse_syntax(), ddl: ClickHouseDdl::new() }
    }
}

impl Default for ClickHouseDialect {
    fn default() -> Self {
        Self::new()
    }
}

/// Does `ident` need quoting in ClickHouse?
pub fn needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else { return true };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return true;
    }
    if !ident.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return true;
    }
    keywords::is_reserved(ident)
}

/// Always quote with backticks (backslash escaping).
pub fn quote_ident(ident: &str) -> String {
    let mut s = String::with_capacity(ident.len() + 2);
    s.push('`');
    for c in ident.chars() {
        match c {
            '`' => s.push_str("\\`"),
            '\\' => s.push_str("\\\\"),
            c => s.push(c),
        }
    }
    s.push('`');
    s
}

/// Quote only when needed.
pub fn ident(name: &str) -> String {
    if needs_quoting(name) { quote_ident(name) } else { name.to_string() }
}

/// `db.name`, quoted as needed.
pub fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", ident(s), ident(name)),
        _ => ident(name),
    }
}

/// ClickHouse string literal with backslash escapes.
pub fn string_literal(s: &str) -> String {
    crate::http::sql_string(s)
}

fn fraction(nanos: u32, max_digits: usize) -> String {
    if nanos == 0 || max_digits == 0 {
        return String::new();
    }
    let digits = format!("{nanos:09}");
    let cut = &digits[..max_digits.min(9)];
    let trimmed = cut.trim_end_matches('0');
    if trimmed.is_empty() { String::new() } else { format!(".{trimmed}") }
}

impl Dialect for ClickHouseDialect {
    fn kind(&self) -> DbKind {
        DbKind::ClickHouse
    }

    fn syntax(&self) -> &SqlSyntax {
        &self.syntax
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        needs_quoting(ident)
    }

    fn force_quote_ident(&self, ident: &str) -> String {
        quote_ident(ident)
    }

    fn string_literal(&self, s: &str) -> String {
        string_literal(s)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        if b.is_empty() { "''".into() } else { format!("unhex('{}')", hex::encode_upper(b)) }
    }

    fn bool_literal(&self, b: bool) -> String {
        if b { "true".into() } else { "false".into() }
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let target = type_name.map(parse_type);
        let base = target.as_ref().map(|t| t.base().clone());
        match value {
            Value::Null => "NULL".into(),
            Value::Bool(b) => self.bool_literal(*b),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_nan() => "nan".into(),
            Value::Float(f) if f.is_infinite() => {
                if *f > 0.0 { "inf".into() } else { "-inf".into() }
            }
            Value::Float(f) => format!("{f}"),
            Value::Decimal(s) => {
                if is_decimal_text(s) && s.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c)) {
                    s.to_string()
                } else {
                    string_literal(s)
                }
            }
            Value::Text(s) | Value::Json(s) => string_literal(s),
            Value::Bytes(b) => self.bytes_literal(b),
            Value::Date(d) => format!("'{}'", d.format("%Y-%m-%d")),
            Value::Time(t) => format!("'{}{}'", t.format("%H:%M:%S"), fraction(t.nanosecond(), 9)),
            Value::DateTime(dt) => {
                let digits = match base {
                    Some(ChType::DateTime(_)) => 0,
                    Some(ChType::DateTime64 { precision, .. }) => precision as usize,
                    _ => 9,
                };
                format!("'{}{}'", dt.format("%Y-%m-%d %H:%M:%S"), fraction(dt.nanosecond(), digits))
            }
            Value::DateTimeTz(dt) => {
                // Convert to UTC and let ClickHouse interpret it in UTC.
                let utc = dt.naive_utc();
                match base {
                    Some(ChType::DateTime(_)) => {
                        format!("toDateTime('{}', 'UTC')", utc.format("%Y-%m-%d %H:%M:%S"))
                    }
                    _ => {
                        let precision = match base {
                            Some(ChType::DateTime64 { precision, .. }) => precision.min(9),
                            _ if utc.nanosecond() % 1000 != 0 => 9,
                            _ => 6,
                        };
                        format!(
                            "toDateTime64('{}{}', {precision}, 'UTC')",
                            utc.format("%Y-%m-%d %H:%M:%S"),
                            fraction(utc.nanosecond(), precision as usize)
                        )
                    }
                }
            }
            Value::Uuid(u) => format!("'{}'", u.hyphenated()),
            Value::Array(items) => {
                let elem_type = match &base {
                    Some(ChType::Array(inner)) => Some(type_text(inner)),
                    _ => None,
                };
                let parts: Vec<String> = items.iter().map(|v| self.literal(v, elem_type.as_deref())).collect();
                format!("[{}]", parts.join(", "))
            }
        }
    }

    fn placeholder(&self, _index: usize) -> String {
        "?".into()
    }

    fn upsert(&self, _table: &str, _columns: &[String], _key_columns: &[String], _values: &[String]) -> Option<String> {
        None
    }

    fn supports_returning(&self) -> bool {
        false
    }

    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!("TRUNCATE TABLE {}", self.qualified_ref(table))
    }

    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        vec![
            MaintenanceOp {
                id: "optimize",
                label: "Optimize",
                description: "Merge data parts of a MergeTree table (OPTIMIZE TABLE)",
                applies_to: &[ObjectKind::Table],
                options: &[("final", "FINAL (merge everything)", true), ("deduplicate", "DEDUPLICATE", false)],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "check",
                label: "Check table",
                description: "Verify the data parts of a table (CHECK TABLE)",
                applies_to: &[ObjectKind::Table],
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "materialize_ttl",
                label: "Materialize TTL",
                description: "Apply TTL rules to existing data (ALTER TABLE ... MATERIALIZE TTL)",
                applies_to: &[ObjectKind::Table],
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "flush_logs",
                label: "Flush logs",
                description: "Flush buffered system log tables (SYSTEM FLUSH LOGS)",
                applies_to: &[ObjectKind::Database],
                options: &[],
                needs_autocommit: false,
            },
        ]
    }

    fn maintenance_sql(&self, op: &str, targets: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        let opt = |k: &str, default: bool| options.get(k).copied().unwrap_or(default);
        match op {
            "optimize" => targets
                .iter()
                .map(|t| {
                    let mut s = format!("OPTIMIZE TABLE {}", self.qualified_ref(t));
                    if opt("final", true) {
                        s.push_str(" FINAL");
                    }
                    if opt("deduplicate", false) {
                        s.push_str(" DEDUPLICATE");
                    }
                    s
                })
                .collect(),
            "check" => targets.iter().map(|t| format!("CHECK TABLE {}", self.qualified_ref(t))).collect(),
            "materialize_ttl" => targets
                .iter()
                .map(|t| format!("ALTER TABLE {} MATERIALIZE TTL", self.qualified_ref(t)))
                .collect(),
            "flush_logs" => vec!["SYSTEM FLUSH LOGS".into()],
            _ => Vec::new(),
        }
    }

    /// ClickHouse has no `CURRENT_USER` form of `ALTER USER`, so the user
    /// name is required (`None` returns `None`).
    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        let who = ident(user?);
        Some(format!("ALTER USER {who} IDENTIFIED BY {}", string_literal(new_password)))
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        crate::types::kind_of(type_name)
    }
}

/// Render a parsed type back to text (used for array element literals).
fn type_text(t: &ChType) -> String {
    match t {
        ChType::Nullable(i) => format!("Nullable({})", type_text(i)),
        ChType::LowCardinality(i) => format!("LowCardinality({})", type_text(i)),
        ChType::Array(i) => format!("Array({})", type_text(i)),
        ChType::DateTime(None) => "DateTime".into(),
        ChType::DateTime(Some(tz)) => format!("DateTime('{tz}')"),
        ChType::DateTime64 { precision, tz: None } => format!("DateTime64({precision})"),
        ChType::DateTime64 { precision, tz: Some(tz) } => format!("DateTime64({precision}, '{tz}')"),
        ChType::String => "String".into(),
        _ => "String".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    #[test]
    fn quoting() {
        let d = ClickHouseDialect::new();
        assert_eq!(d.quote_ident("events"), "events");
        assert_eq!(d.quote_ident("Events"), "Events");
        assert_eq!(d.quote_ident("select"), "`select`");
        assert_eq!(d.quote_ident("my col"), "`my col`");
        assert_eq!(d.quote_ident("a`b"), "`a\\`b`");
        assert_eq!(d.qualified(Some("db"), "t"), "db.t");
        assert_eq!(d.string_literal("it's \\ ok"), "'it\\'s \\\\ ok'");
    }

    #[test]
    fn literals() {
        let d = ClickHouseDialect::new();
        assert_eq!(d.literal(&Value::Bool(true), None), "true");
        assert_eq!(d.literal(&Value::Float(f64::NAN), None), "nan");
        assert_eq!(d.literal(&Value::Float(f64::NEG_INFINITY), None), "-inf");
        assert_eq!(d.literal(&Value::bytes(vec![0xabu8, 1]), None), "unhex('AB01')");
        assert_eq!(d.literal(&Value::decimal("12.50"), None), "12.50");
        let dt = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_micro_opt(3, 4, 5, 123_456).unwrap();
        assert_eq!(d.literal(&Value::DateTime(dt), None), "'2024-01-02 03:04:05.123456'");
        assert_eq!(d.literal(&Value::DateTime(dt), Some("DateTime")), "'2024-01-02 03:04:05'");
        assert_eq!(d.literal(&Value::DateTime(dt), Some("DateTime64(3)")), "'2024-01-02 03:04:05.123'");
        let tz = dt.and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap();
        assert_eq!(
            d.literal(&Value::DateTimeTz(tz), None),
            "toDateTime64('2024-01-01 20:04:05.123456', 6, 'UTC')"
        );
        assert_eq!(d.literal(&Value::DateTimeTz(tz), Some("DateTime('Asia/Jakarta')")), "toDateTime('2024-01-01 20:04:05', 'UTC')");
        let arr = Value::Array(vec![Value::Int(1), Value::Null, Value::text("x")].into_boxed_slice());
        assert_eq!(d.literal(&arr, None), "[1, NULL, 'x']");
        assert_eq!(d.literal(&Value::Date(dt.date()), None), "'2024-01-02'");
        assert_eq!(d.literal(&Value::Time(dt.time()), None), "'03:04:05.123456'");
    }

    #[test]
    fn maintenance() {
        let d = ClickHouseDialect::new();
        let t = ObjectRef::table(Some("db".into()), "t");
        let mut o = BTreeMap::new();
        o.insert("deduplicate".to_string(), true);
        assert_eq!(d.maintenance_sql("optimize", std::slice::from_ref(&t), &o), vec!["OPTIMIZE TABLE db.t FINAL DEDUPLICATE"]);
        assert_eq!(d.maintenance_sql("check", &[t], &BTreeMap::new()), vec!["CHECK TABLE db.t"]);
        assert!(d.maintenance_ops().iter().any(|o| o.id == "flush_logs"));
        assert_eq!(
            d.change_password_sql(Some("bob"), "p'w").unwrap(),
            "ALTER USER bob IDENTIFIED BY 'p\\'w'"
        );
        assert!(d.change_password_sql(None, "x").is_none());
        assert!(d.upsert("t", &[], &[], &[]).is_none());
        assert_eq!(d.type_kind("Nullable(UInt8)"), DataKind::Int);
    }
}
