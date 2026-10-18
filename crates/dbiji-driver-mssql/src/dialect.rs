//! Transact-SQL dialect.

use crate::ddl::MsSqlDdl;
use crate::keywords;
use crate::sqltext;
use chrono::Timelike;
use dbiji_core::DbKind;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::value::{DataKind, Value, is_decimal_text};
use std::collections::BTreeMap;

/// Lexical rules of T-SQL.
pub const fn tsql_syntax() -> SqlSyntax {
    SqlSyntax {
        keywords: keywords::KEYWORDS,
        functions: keywords::FUNCTIONS,
        data_types: keywords::TYPES,
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
    }
}

/// Does `ident` need `[...]` quoting?
pub fn needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else { return true };
    if !(first == '_' || first.is_ascii_alphabetic() || first == '#' || first == '@') {
        return true;
    }
    if !ident.chars().all(|c| c == '_' || c == '$' || c == '#' || c == '@' || c.is_ascii_alphanumeric()) {
        return true;
    }
    keywords::is_reserved(ident) || dbiji_core::keywords::is_reserved(ident)
}

/// Always quote with brackets (`]` doubled).
pub fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// Quote only when needed.
pub fn ident(name: &str) -> String {
    if needs_quoting(name) { quote(name) } else { name.to_string() }
}

/// `schema.name`, quoted as needed.
pub fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", ident(s), ident(name)),
        _ => ident(name),
    }
}

/// `N'...'` literal.
pub fn nstring(s: &str) -> String {
    format!("N{}", ansi::string_literal(s, false))
}

fn fraction(nanos: u32, digits: usize) -> String {
    if nanos == 0 || digits == 0 {
        return String::new();
    }
    let all = format!("{nanos:09}");
    let cut = &all[..digits.min(9)];
    let t = cut.trim_end_matches('0');
    if t.is_empty() { String::new() } else { format!(".{t}") }
}

/// T-SQL dialect: bracket identifiers, `N'...'` strings, `@P1` placeholders,
/// `GO` batches, `OFFSET ... FETCH` / `TOP` limits and `MERGE` upserts.
pub struct MsSqlDialect {
    syntax: SqlSyntax,
    ddl: MsSqlDdl,
}

impl MsSqlDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        MsSqlDialect { syntax: tsql_syntax(), ddl: MsSqlDdl::new() }
    }
}

impl Default for MsSqlDialect {
    fn default() -> Self {
        Self::new()
    }
}

fn base_type(type_name: Option<&str>) -> String {
    type_name
        .map(|t| t.trim().to_ascii_lowercase())
        .map(|t| t.split(['(', ' ']).next().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_string())
        .unwrap_or_default()
}

impl Dialect for MsSqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::SqlServer
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
        quote(ident)
    }

    fn string_literal(&self, s: &str) -> String {
        nstring(s)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        format!("0x{}", hex::encode_upper(b))
    }

    fn bool_literal(&self, b: bool) -> String {
        if b { "1".into() } else { "0".into() }
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let base = base_type(type_name);
        match value {
            Value::Null => "NULL".into(),
            Value::Bool(b) => self.bool_literal(*b),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => {
                let s = format!("{f:e}");
                // integral values print without an exponent
                if f.fract() == 0.0 && f.abs() < 1e15 { format!("{f}") } else { s }
            }
            Value::Float(f) => format!("CAST({} AS float)", nstring(&f.to_string())),
            Value::Decimal(s) => {
                if is_decimal_text(s) && s.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c)) {
                    s.to_string()
                } else {
                    nstring(s)
                }
            }
            Value::Text(s) | Value::Json(s) => {
                if matches!(base.as_str(), "varchar" | "char" | "text") {
                    ansi::string_literal(s, false)
                } else {
                    nstring(s)
                }
            }
            Value::Bytes(b) => self.bytes_literal(b),
            Value::Uuid(u) => format!("'{}'", u.hyphenated()),
            Value::Date(d) => format!("'{}'", d.format("%Y%m%d")),
            Value::Time(t) => format!("'{}{}'", t.format("%H:%M:%S"), fraction(t.nanosecond(), 7)),
            Value::DateTime(dt) => {
                let digits = match base.as_str() {
                    "datetime" => 3,
                    "smalldatetime" => 0,
                    _ => 7,
                };
                let text = format!("{}{}", dt.format("%Y-%m-%dT%H:%M:%S"), fraction(dt.nanosecond(), digits));
                if base.is_empty() { format!("CAST('{text}' AS datetime2)") } else { format!("'{text}'") }
            }
            Value::DateTimeTz(dt) => {
                let local = dt.naive_local();
                let off = dt.offset().local_minus_utc();
                let sign = if off < 0 { '-' } else { '+' };
                let off = off.abs();
                let text = format!(
                    "{}{}{sign}{:02}:{:02}",
                    local.format("%Y-%m-%d %H:%M:%S"),
                    fraction(local.nanosecond(), 7),
                    off / 3600,
                    (off % 3600) / 60
                );
                if base.is_empty() || base == "datetimeoffset" {
                    format!("CAST('{text}' AS datetimeoffset)")
                } else {
                    format!("'{text}'")
                }
            }
            Value::Array(_) => nstring(&value.to_text()),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        format!("@P{index}")
    }

    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let inner = ansi::strip_trailing_semicolon(sql);
        let shape = sqltext::select_shape(inner);
        if !shape.is_select {
            return inner.to_string();
        }
        let simple = !shape.has_top && !shape.has_offset && !shape.has_trailer && !shape.has_into;
        if simple && shape.order_by.is_some() {
            return format!("{inner}\nOFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY");
        }
        if simple && offset == 0 && !shape.has_set_op
            && let Some(at) = shape.top_insert_at {
                return format!("{} TOP ({limit}){}", &inner[..at], &inner[at..]);
            }
        if shape.has_into {
            return inner.to_string();
        }
        format!(
            "SELECT * FROM ({inner}) dbiji_q ORDER BY (SELECT NULL) OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        )
    }

    fn count_query(&self, sql: &str) -> String {
        let inner = ansi::strip_trailing_semicolon(sql);
        let shape = sqltext::select_shape(inner);
        // ORDER BY is not allowed in a derived table without TOP/OFFSET
        let body = match shape.order_by {
            Some(pos) if !shape.has_top && !shape.has_offset && !shape.has_trailer => inner[..pos].trim_end(),
            _ => inner,
        };
        format!("SELECT COUNT_BIG(*) FROM ({body}) dbiji_cnt")
    }

    fn select_table(
        &self,
        table: &ObjectRef,
        columns: Option<&[String]>,
        where_clause: Option<&str>,
        order_by: Option<&str>,
        limit: Option<u64>,
    ) -> String {
        let cols = match columns {
            Some(c) if !c.is_empty() => c.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", "),
            _ => "*".to_string(),
        };
        let top = limit.map(|l| format!("TOP ({l}) ")).unwrap_or_default();
        let mut sql = format!("SELECT {top}{cols} FROM {}", self.qualified_ref(table));
        if let Some(w) = where_clause.filter(|w| !w.trim().is_empty()) {
            sql.push_str(" WHERE ");
            sql.push_str(w);
        }
        if let Some(o) = order_by.filter(|o| !o.trim().is_empty()) {
            sql.push_str(" ORDER BY ");
            sql.push_str(o);
        }
        sql
    }

    fn upsert(&self, table: &str, columns: &[String], key_columns: &[String], values: &[String]) -> Option<String> {
        if columns.is_empty() || key_columns.is_empty() || columns.len() != values.len() {
            return None;
        }
        let on = key_columns.iter().map(|k| format!("t.{k} = s.{k}")).collect::<Vec<_>>().join(" AND ");
        let updates: Vec<String> = columns
            .iter()
            .filter(|c| !key_columns.contains(c))
            .map(|c| format!("t.{c} = s.{c}"))
            .collect();
        let mut sql = format!(
            "MERGE INTO {table} WITH (HOLDLOCK) AS t\nUSING (VALUES ({})) AS s ({})\nON {on}",
            values.join(", "),
            columns.join(", ")
        );
        if !updates.is_empty() {
            sql.push_str(&format!("\nWHEN MATCHED THEN UPDATE SET {}", updates.join(", ")));
        }
        sql.push_str(&format!(
            "\nWHEN NOT MATCHED THEN INSERT ({}) VALUES ({});",
            columns.join(", "),
            columns.iter().map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", ")
        ));
        Some(sql)
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
                id: "update_statistics",
                label: "Update statistics",
                description: "Refresh the optimizer statistics of a table (UPDATE STATISTICS)",
                applies_to: &[ObjectKind::Table, ObjectKind::View],
                options: &[("fullscan", "Full scan", false)],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "rebuild_indexes",
                label: "Rebuild indexes",
                description: "Rebuild all indexes of a table (ALTER INDEX ALL ... REBUILD)",
                applies_to: &[ObjectKind::Table, ObjectKind::Index],
                options: &[("online", "Online (Enterprise edition)", false)],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "reorganize_indexes",
                label: "Reorganize indexes",
                description: "Defragment all indexes of a table (ALTER INDEX ALL ... REORGANIZE)",
                applies_to: &[ObjectKind::Table, ObjectKind::Index],
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "check_table",
                label: "Check table",
                description: "Check the integrity of a table (DBCC CHECKTABLE)",
                applies_to: &[ObjectKind::Table],
                options: &[("no_infomsgs", "Suppress informational messages", true)],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "check_database",
                label: "Check database",
                description: "Check the integrity of a database (DBCC CHECKDB)",
                applies_to: &[ObjectKind::Database],
                options: &[("no_infomsgs", "Suppress informational messages", true), ("physical_only", "Physical only", false)],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "update_all_statistics",
                label: "Update all statistics",
                description: "Update statistics of every table in the database (sp_updatestats)",
                applies_to: &[ObjectKind::Database],
                options: &[],
                needs_autocommit: false,
            },
            MaintenanceOp {
                id: "shrink_database",
                label: "Shrink database",
                description: "Release unused space of the database files (DBCC SHRINKDATABASE)",
                applies_to: &[ObjectKind::Database],
                options: &[],
                needs_autocommit: true,
            },
            MaintenanceOp {
                id: "backup_database",
                label: "Back up database",
                description: "Server side full backup into the default backup directory (BACKUP DATABASE ... TO DISK)",
                applies_to: &[ObjectKind::Database],
                options: &[("copy_only", "Copy-only backup", true), ("compression", "Compress", false), ("checksum", "Verify checksums", true)],
                needs_autocommit: true,
            },
        ]
    }

    fn maintenance_sql(&self, op: &str, targets: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        let opt = |k: &str, d: bool| options.get(k).copied().unwrap_or(d);
        let table_of = |t: &ObjectRef| -> String {
            if t.kind == ObjectKind::Index {
                t.parent_table().map(|p| self.qualified_ref(&p)).unwrap_or_else(|| self.qualified_ref(t))
            } else {
                self.qualified_ref(t)
            }
        };
        match op {
            "update_statistics" => targets
                .iter()
                .map(|t| {
                    let mut s = format!("UPDATE STATISTICS {}", self.qualified_ref(t));
                    if opt("fullscan", false) {
                        s.push_str(" WITH FULLSCAN");
                    }
                    s
                })
                .collect(),
            "rebuild_indexes" => targets
                .iter()
                .map(|t| {
                    let which = if t.kind == ObjectKind::Index { quote(&t.name) } else { "ALL".into() };
                    let mut s = format!("ALTER INDEX {which} ON {} REBUILD", table_of(t));
                    if opt("online", false) {
                        s.push_str(" WITH (ONLINE = ON)");
                    }
                    s
                })
                .collect(),
            "reorganize_indexes" => targets
                .iter()
                .map(|t| {
                    let which = if t.kind == ObjectKind::Index { quote(&t.name) } else { "ALL".into() };
                    format!("ALTER INDEX {which} ON {} REORGANIZE", table_of(t))
                })
                .collect(),
            "check_table" => targets
                .iter()
                .map(|t| {
                    let mut s = format!("DBCC CHECKTABLE ({})", nstring(&self.qualified_ref(t)));
                    if opt("no_infomsgs", true) {
                        s.push_str(" WITH NO_INFOMSGS");
                    }
                    s
                })
                .collect(),
            "check_database" => targets
                .iter()
                .map(|t| {
                    let mut with = Vec::new();
                    if opt("no_infomsgs", true) {
                        with.push("NO_INFOMSGS");
                    }
                    if opt("physical_only", false) {
                        with.push("PHYSICAL_ONLY");
                    }
                    let mut s = format!("DBCC CHECKDB ({})", quote(&t.name));
                    if !with.is_empty() {
                        s.push_str(&format!(" WITH {}", with.join(", ")));
                    }
                    s
                })
                .collect(),
            "update_all_statistics" => {
                targets.iter().map(|t| format!("EXEC {}.sys.sp_updatestats", quote(&t.name))).collect()
            }
            "shrink_database" => targets.iter().map(|t| format!("DBCC SHRINKDATABASE ({})", quote(&t.name))).collect(),
            "backup_database" => targets
                .iter()
                .map(|t| {
                    let file = format!("{}.bak", t.name.replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_"));
                    let mut with = vec!["INIT".to_string(), format!("NAME = {}", nstring(&format!("{} full backup", t.name)))];
                    if opt("copy_only", true) {
                        with.push("COPY_ONLY".into());
                    }
                    if opt("compression", false) {
                        with.push("COMPRESSION".into());
                    }
                    if opt("checksum", true) {
                        with.push("CHECKSUM".into());
                    }
                    format!("BACKUP DATABASE {} TO DISK = {} WITH {}", quote(&t.name), nstring(&file), with.join(", "))
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `ALTER LOGIN ... WITH PASSWORD`. Logins without `ALTER ANY LOGIN`
    /// must also give `OLD_PASSWORD`, which this interface does not carry:
    /// for them the statement fails with a permission error.
    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        Some(match user {
            Some(u) => format!("ALTER LOGIN {} WITH PASSWORD = {}", quote(u), nstring(new_password)),
            None => format!(
                "DECLARE @dbiji_sql nvarchar(max) = N'ALTER LOGIN ' + QUOTENAME(SUSER_SNAME()) + N' WITH PASSWORD = ' + {}\nEXEC (@dbiji_sql)",
                nstring(&nstring(new_password))
            ),
        })
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        crate::types::kind_of_type_name(type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    #[test]
    fn quoting() {
        let d = MsSqlDialect::new();
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("Order"), "[Order]");
        assert_eq!(d.quote_ident("my table"), "[my table]");
        assert_eq!(d.quote_ident("a]b"), "[a]]b]");
        assert_eq!(d.quote_ident("#tmp"), "#tmp");
        assert_eq!(d.quote_ident("1abc"), "[1abc]");
        assert_eq!(d.quote_ident("percent"), "[percent]");
        assert_eq!(d.qualified(Some("dbo"), "user"), "dbo.[user]");
        assert_eq!(d.string_literal("it's"), "N'it''s'");
        assert_eq!(d.placeholder(3), "@P3");
    }

    #[test]
    fn literals() {
        let d = MsSqlDialect::new();
        assert_eq!(d.literal(&Value::Bool(true), None), "1");
        assert_eq!(d.literal(&Value::bytes(vec![0xab, 0x01]), None), "0xAB01");
        assert_eq!(d.literal(&Value::text("x"), Some("varchar(10)")), "'x'");
        assert_eq!(d.literal(&Value::text("x"), Some("nvarchar(10)")), "N'x'");
        assert_eq!(d.literal(&Value::Float(2.0), None), "2");
        assert_eq!(d.literal(&Value::Float(0.1), None), "1e-1");
        let dt = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_nano_opt(3, 4, 5, 3_333_333).unwrap();
        assert_eq!(d.literal(&Value::DateTime(dt), Some("datetime")), "'2024-01-02T03:04:05.003'");
        assert_eq!(d.literal(&Value::DateTime(dt), Some("datetime2(7)")), "'2024-01-02T03:04:05.0033333'");
        assert_eq!(d.literal(&Value::DateTime(dt), None), "CAST('2024-01-02T03:04:05.0033333' AS datetime2)");
        assert_eq!(d.literal(&Value::Date(dt.date()), None), "'20240102'");
        let tz = dt.and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap();
        assert_eq!(d.literal(&Value::DateTimeTz(tz), None), "CAST('2024-01-02 03:04:05.0033333+07:00' AS datetimeoffset)");
        assert_eq!(d.literal(&Value::Null, None), "NULL");
    }

    #[test]
    fn limits() {
        let d = MsSqlDialect::new();
        assert_eq!(d.limit_query("SELECT a FROM t ORDER BY a;", 10, 20), "SELECT a FROM t ORDER BY a\nOFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY");
        assert_eq!(d.limit_query("SELECT DISTINCT a FROM t", 5, 0), "SELECT DISTINCT TOP (5) a FROM t");
        assert_eq!(
            d.limit_query("SELECT a FROM t", 5, 10),
            "SELECT * FROM (SELECT a FROM t) dbiji_q ORDER BY (SELECT NULL) OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY"
        );
        assert_eq!(d.limit_query("SELECT TOP 3 a FROM t ORDER BY a", 5, 0).matches("dbiji_q").count(), 1);
        assert_eq!(d.limit_query("EXEC sp_who", 5, 0), "EXEC sp_who");
        assert_eq!(d.count_query("SELECT a FROM t ORDER BY a"), "SELECT COUNT_BIG(*) FROM (SELECT a FROM t) dbiji_cnt");
        let t = ObjectRef::table(Some("dbo".into()), "users");
        assert_eq!(d.select_table(&t, None, Some("id > 1"), Some("id"), Some(200)), "SELECT TOP (200) * FROM dbo.users WHERE id > 1 ORDER BY id");
    }

    #[test]
    fn upsert_and_maintenance() {
        let d = MsSqlDialect::new();
        let sql = d
            .upsert("dbo.t", &["id".into(), "name".into()], &["id".into()], &["@P1".into(), "@P2".into()])
            .unwrap();
        assert!(sql.starts_with("MERGE INTO dbo.t WITH (HOLDLOCK) AS t"), "{sql}");
        assert!(sql.contains("ON t.id = s.id"));
        assert!(sql.contains("WHEN MATCHED THEN UPDATE SET t.name = s.name"));
        assert!(sql.ends_with("VALUES (s.id, s.name);"));
        assert!(d.upsert("t", &[], &[], &[]).is_none());
        let t = ObjectRef::table(Some("dbo".into()), "t");
        let mut o = BTreeMap::new();
        o.insert("fullscan".to_string(), true);
        assert_eq!(d.maintenance_sql("update_statistics", std::slice::from_ref(&t), &o), vec!["UPDATE STATISTICS dbo.t WITH FULLSCAN"]);
        assert_eq!(d.maintenance_sql("rebuild_indexes", std::slice::from_ref(&t), &BTreeMap::new()), vec!["ALTER INDEX ALL ON dbo.t REBUILD"]);
        assert_eq!(d.maintenance_sql("check_table", &[t], &BTreeMap::new()), vec!["DBCC CHECKTABLE (N'dbo.t') WITH NO_INFOMSGS"]);
        let db = ObjectRef::new(ObjectKind::Database, None, "shop");
        assert_eq!(d.maintenance_sql("check_database", std::slice::from_ref(&db), &BTreeMap::new()), vec!["DBCC CHECKDB ([shop]) WITH NO_INFOMSGS"]);
        let b = d.maintenance_sql("backup_database", &[db], &BTreeMap::new());
        assert!(b[0].starts_with("BACKUP DATABASE [shop] TO DISK = N'shop.bak' WITH INIT"), "{}", b[0]);
        assert!(b[0].contains("COPY_ONLY"));
        assert_eq!(d.change_password_sql(Some("bob"), "p'w").unwrap(), "ALTER LOGIN [bob] WITH PASSWORD = N'p''w'");
        assert!(d.change_password_sql(None, "x").unwrap().contains("SUSER_SNAME()"));
    }
}
