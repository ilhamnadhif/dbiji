//! The MySQL / MariaDB SQL dialect ([`MySqlDialect`]).

use crate::ddl::MySqlDdl;
use crate::flavor::{Flavor, ServerProfile};
use crate::keywords;
use crate::sqltext;
use dbiji_core::admin::MaintenanceOp;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{IdentCase, SqlSyntax, ansi};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::value::{DataKind, Value};
use dbiji_core::{DbKind, Dialect};
use std::collections::BTreeMap;

const BACKTICK: &[(char, char)] = &[('`', '`')];
const BACKTICK_AND_DOUBLE: &[(char, char)] = &[('`', '`'), ('"', '"')];

/// Lexical description of MySQL / MariaDB SQL.
pub const fn mysql_syntax() -> SqlSyntax {
    SqlSyntax {
        keywords: keywords::KEYWORDS,
        functions: keywords::FUNCTIONS,
        data_types: keywords::DATA_TYPES,
        identifier_quotes: BACKTICK,
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
        plsql_blocks: false,
        extra_ident_chars: "$",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: false,
    }
}

const TABLES: &[ObjectKind] = &[ObjectKind::Table];
const TABLES_AND_VIEWS: &[ObjectKind] = &[ObjectKind::Table, ObjectKind::View];

const MAINTENANCE: &[MaintenanceOp] = &[
    MaintenanceOp {
        id: "analyze",
        label: "Analyze table",
        description: "Update key distribution statistics used by the optimizer (ANALYZE TABLE)",
        applies_to: TABLES,
        options: &[("local", "Do not write to the binary log (LOCAL)", false)],
        needs_autocommit: false,
    },
    MaintenanceOp {
        id: "optimize",
        label: "Optimize table",
        description: "Rebuild the table, defragment data and reclaim unused space (OPTIMIZE TABLE)",
        applies_to: TABLES,
        options: &[("local", "Do not write to the binary log (LOCAL)", false)],
        needs_autocommit: false,
    },
    MaintenanceOp {
        id: "check",
        label: "Check table",
        description: "Check tables and views for errors (CHECK TABLE)",
        applies_to: TABLES_AND_VIEWS,
        options: &[
            (
                "quick",
                "QUICK: do not scan rows for incorrect links",
                false,
            ),
            (
                "fast",
                "FAST: only tables that were not closed properly",
                false,
            ),
            (
                "medium",
                "MEDIUM: scan rows to verify that deleted links are valid",
                false,
            ),
            (
                "extended",
                "EXTENDED: full key lookup for all keys of each row",
                false,
            ),
            (
                "changed",
                "CHANGED: only tables changed since the last check",
                false,
            ),
            (
                "for_upgrade",
                "FOR UPGRADE: check compatibility with the current server version",
                false,
            ),
        ],
        needs_autocommit: false,
    },
    MaintenanceOp {
        id: "repair",
        label: "Repair table",
        description: "Repair a possibly corrupted table (MyISAM, ARIA, ARCHIVE, CSV) (REPAIR TABLE)",
        applies_to: TABLES,
        options: &[
            ("local", "Do not write to the binary log (LOCAL)", false),
            ("quick", "QUICK: repair only the index file", false),
            ("extended", "EXTENDED: create the index row by row", false),
            (
                "use_frm",
                "USE_FRM: recreate the index header from the table definition",
                false,
            ),
        ],
        needs_autocommit: false,
    },
    MaintenanceOp {
        id: "flush",
        label: "Flush tables",
        description: "Close open tables and flush their caches (FLUSH TABLES)",
        applies_to: TABLES,
        options: &[],
        needs_autocommit: false,
    },
];

/// MySQL / MariaDB dialect. One type serves both products; the flavor and,
/// when connected, the server version and `sql_mode` adapt version specific
/// syntax (upsert row alias, `RENAME COLUMN`, `ANSI_QUOTES`,
/// `NO_BACKSLASH_ESCAPES`).
pub struct MySqlDialect {
    profile: ServerProfile,
    syntax: SqlSyntax,
    ddl: MySqlDdl,
}

/// Column types whose values are written as literals (see [`Dialect::inline_value`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    Bit,
    Geometry,
}

fn special_type(type_name: &str) -> Option<Special> {
    let base = type_name.trim().to_ascii_lowercase();
    let base = base.split(['(', ' ']).next().unwrap_or("");
    match base {
        "bit" => Some(Special::Bit),
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" | "geomcollection" => Some(Special::Geometry),
        _ => None,
    }
}

impl MySqlDialect {
    /// `ST_GeomFromText('POINT(1 2)', 4326)` for `SRID=4326;POINT(1 2)` (or plain WKT).
    fn geometry_literal(&self, text: &str) -> String {
        let (srid, wkt) = match text
            .trim()
            .strip_prefix("SRID=")
            .and_then(|r| r.split_once(';'))
        {
            Some((n, wkt)) => (n.trim().parse::<u32>().ok(), wkt),
            None => (None, text.trim()),
        };
        match srid {
            // DBiji shows the stored x/y order; MySQL 8 reads geographic WKT as latitude-longitude by default.
            Some(n) if self.profile.is_mysql() && self.profile.at_least(8, 0, 12) => {
                format!(
                    "ST_GeomFromText({}, {n}, 'axis-order=long-lat')",
                    self.string_literal(wkt)
                )
            }
            Some(n) => format!("ST_GeomFromText({}, {n})", self.string_literal(wkt)),
            None => format!("ST_GeomFromText({})", self.string_literal(wkt)),
        }
    }

    /// Dialect of a flavor without server specific information.
    pub fn new(flavor: Flavor) -> Self {
        Self::for_server(ServerProfile::unknown(flavor))
    }

    /// Dialect matching a connected server (version and `sql_mode`).
    pub fn for_server(profile: ServerProfile) -> Self {
        let mut syntax = mysql_syntax();
        if profile.ansi_quotes() {
            syntax.identifier_quotes = BACKTICK_AND_DOUBLE;
            syntax.double_quoted_strings = false;
        }
        if profile.no_backslash_escapes() {
            syntax.backslash_escapes = false;
        }
        MySqlDialect {
            ddl: MySqlDdl::new(profile.clone()),
            syntax,
            profile,
        }
    }

    /// Flavor of this dialect.
    pub fn flavor(&self) -> Flavor {
        self.profile.flavor
    }

    /// Server profile (version may be unknown).
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    /// The DDL generator (concrete type, with event helpers).
    pub fn mysql_ddl(&self) -> &MySqlDdl {
        &self.ddl
    }

    fn user_spec(&self, user: &str) -> String {
        let u = user.trim();
        let strip = |s: &str| {
            s.trim()
                .trim_matches(|c| c == '\'' || c == '`' || c == '"')
                .to_string()
        };
        match u.rfind('@') {
            Some(at) if at > 0 => {
                format!(
                    "{}@{}",
                    self.string_literal(&strip(&u[..at])),
                    self.string_literal(&strip(&u[at + 1..]))
                )
            }
            _ => self.string_literal(&strip(u)),
        }
    }
}

impl Dialect for MySqlDialect {
    fn kind(&self) -> DbKind {
        self.profile.flavor.kind()
    }

    fn syntax(&self) -> &SqlSyntax {
        &self.syntax
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn catalog_folders(&self) -> Vec<dbiji_core::catalog::CatalogFolder> {
        crate::catalogs::folders()
    }

    fn force_quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        let mut chars = ident.chars();
        let Some(first) = chars.next() else {
            return true;
        };
        if !(first.is_ascii_alphabetic() || first == '_') {
            return true;
        }
        if !ident
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        {
            return true;
        }
        keywords::is_reserved(ident)
    }

    fn string_literal(&self, s: &str) -> String {
        if !self.syntax.backslash_escapes {
            return ansi::string_literal(s, false);
        }
        crate::ddl::string_literal(s)
    }

    fn inline_value(&self, type_name: &str) -> bool {
        special_type(type_name).is_some()
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        // BIT digits and geometry text (as DBiji shows them) would be stored as plain strings.
        match (type_name.and_then(special_type), value) {
            (Some(Special::Bit), Value::Text(s))
                if !s.is_empty() && s.chars().all(|c| c == '0' || c == '1') =>
            {
                return format!("b'{s}'");
            }
            (Some(Special::Bit), Value::Bool(b)) => {
                return if *b { "b'1'".into() } else { "b'0'".into() };
            }
            (Some(Special::Geometry), Value::Text(s)) => return self.geometry_literal(s),
            _ => {}
        }
        match value {
            // With its offset where MySQL converts it (8.0.19+), else converted from UTC
            // into the session's time zone by the server (as parameters are).
            Value::DateTimeTz(dt) if self.profile.is_mysql() && self.profile.at_least(8, 0, 19) => {
                self.string_literal(&dt.format("%Y-%m-%d %H:%M:%S%.6f%:z").to_string())
            }
            Value::DateTimeTz(dt) => {
                let utc = self.string_literal(&Value::DateTime(dt.naive_utc()).to_text());
                let offset = format!(
                    "TIMESTAMPADD(SECOND, TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()), {utc})"
                );
                // CONVERT_TZ leaves times outside the TIMESTAMP range unchanged.
                if crate::session::in_timestamp_range(&dt.naive_utc()) {
                    format!("COALESCE(CONVERT_TZ({utc}, '+00:00', @@session.time_zone), {offset})")
                } else {
                    offset
                }
            }
            Value::Array(_) => self.string_literal(&value.to_json().to_string()),
            Value::Json(s) => self.string_literal(s),
            other => ansi::literal(self, other),
        }
    }

    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let inner = ansi::strip_trailing_semicolon(sql);
        let lead = sqltext::leading_keyword(inner);
        let appendable = matches!(
            lead.as_deref(),
            Some("SELECT" | "WITH" | "TABLE" | "VALUES")
        ) && sqltext::is_read_only_statement(inner)
            && !["LIMIT", "FOR", "LOCK", "INTO", "PROCEDURE"]
                .iter()
                .any(|w| sqltext::has_top_level_word(inner, w));
        let clause = if offset > 0 {
            format!("LIMIT {limit} OFFSET {offset}")
        } else {
            format!("LIMIT {limit}")
        };
        if appendable {
            // A newline terminates a trailing `--` / `#` comment.
            format!("{inner}\n{clause}")
        } else {
            format!("SELECT * FROM ({inner}) dbiji_q {clause}")
        }
    }

    fn upsert(
        &self,
        table: &str,
        columns: &[String],
        key_columns: &[String],
        values: &[String],
    ) -> Option<String> {
        if columns.is_empty() || columns.len() != values.len() {
            return None;
        }
        let non_keys: Vec<&String> = columns
            .iter()
            .filter(|c| !key_columns.contains(c))
            .collect();
        let cols = columns.join(", ");
        let vals = values.join(", ");
        if self.profile.has_upsert_row_alias() {
            let sets = if non_keys.is_empty() {
                let k = key_columns.first().unwrap_or(&columns[0]);
                format!("{k} = {k}")
            } else {
                non_keys
                    .iter()
                    .map(|c| format!("{c} = new.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Some(format!(
                "INSERT INTO {table} ({cols}) VALUES ({vals}) AS new ON DUPLICATE KEY UPDATE {sets}"
            ))
        } else {
            let sets = if non_keys.is_empty() {
                let k = key_columns.first().unwrap_or(&columns[0]);
                format!("{k} = {k}")
            } else {
                non_keys
                    .iter()
                    .map(|c| format!("{c} = VALUES({c})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Some(format!(
                "INSERT INTO {table} ({cols}) VALUES ({vals}) ON DUPLICATE KEY UPDATE {sets}"
            ))
        }
    }

    fn supports_returning(&self) -> bool {
        self.profile.has_returning()
    }

    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        MAINTENANCE.to_vec()
    }

    fn truncate_options(&self) -> &'static [(&'static str, &'static str, bool)] {
        &[(
            "no_fk_checks",
            "Ignore foreign keys while truncating (FOREIGN_KEY_CHECKS = 0)",
            false,
        )]
    }

    fn truncate_sql(&self, tables: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        let mut out: Vec<String> = tables.iter().map(|t| self.truncate_table(t)).collect();
        if !out.is_empty() && options.get("no_fk_checks").copied().unwrap_or(false) {
            out.insert(0, "SET FOREIGN_KEY_CHECKS = 0".to_string());
            out.push("SET FOREIGN_KEY_CHECKS = 1".to_string());
        }
        out
    }

    fn maintenance_sql(
        &self,
        op: &str,
        targets: &[ObjectRef],
        options: &BTreeMap<String, bool>,
    ) -> Vec<String> {
        let on = |k: &str| options.get(k).copied().unwrap_or(false);
        let list = targets
            .iter()
            .map(|t| self.qualified(t.schema.as_deref(), &t.name))
            .collect::<Vec<_>>()
            .join(", ");
        if list.is_empty() && op != "flush" {
            return Vec::new();
        }
        let local = if on("local") {
            "NO_WRITE_TO_BINLOG "
        } else {
            ""
        };
        let sql = match op {
            "analyze" => format!("ANALYZE {local}TABLE {list}"),
            "optimize" => format!("OPTIMIZE {local}TABLE {list}"),
            "check" => {
                let mut s = format!("CHECK TABLE {list}");
                for (k, kw) in [
                    ("for_upgrade", "FOR UPGRADE"),
                    ("quick", "QUICK"),
                    ("fast", "FAST"),
                    ("medium", "MEDIUM"),
                    ("extended", "EXTENDED"),
                    ("changed", "CHANGED"),
                ] {
                    if on(k) {
                        s.push(' ');
                        s.push_str(kw);
                    }
                }
                s
            }
            "repair" => {
                let mut s = format!("REPAIR {local}TABLE {list}");
                for (k, kw) in [
                    ("quick", "QUICK"),
                    ("extended", "EXTENDED"),
                    ("use_frm", "USE_FRM"),
                ] {
                    if on(k) {
                        s.push(' ');
                        s.push_str(kw);
                    }
                }
                s
            }
            "flush" => {
                if list.is_empty() {
                    "FLUSH TABLES".to_string()
                } else {
                    format!("FLUSH TABLES {list}")
                }
            }
            _ => return Vec::new(),
        };
        vec![sql]
    }

    /// MySQL: `ALTER USER CURRENT_USER() IDENTIFIED BY '...'`. MariaDB needs
    /// the CREATE USER privilege for `ALTER USER`, so its own password is
    /// changed with `SET PASSWORD = PASSWORD('...')`.
    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        let user = user.map(str::trim).filter(|u| !u.is_empty());
        let pw = self.string_literal(new_password);
        if self.profile.is_mariadb() {
            return Some(match user {
                Some(u) => format!("SET PASSWORD FOR {} = PASSWORD({pw})", self.user_spec(u)),
                None => format!("SET PASSWORD = PASSWORD({pw})"),
            });
        }
        let who = match user {
            Some(u) => self.user_spec(u),
            None => "CURRENT_USER()".to_string(),
        };
        Some(format!("ALTER USER {who} IDENTIFIED BY {pw}"))
    }

    fn type_kind(&self, type_name: &str) -> DataKind {
        type_kind(type_name)
    }
}

/// Logical kind of a MySQL / MariaDB type name.
pub fn type_kind(type_name: &str) -> DataKind {
    let l = type_name.trim().to_ascii_lowercase();
    let base = l.split(['(', ' ']).next().unwrap_or("");
    match base {
        "bool" | "boolean" => DataKind::Bool,
        "tinyint" if l.starts_with("tinyint(1)") => DataKind::Bool,
        "bit" => {
            if l == "bit" || l.starts_with("bit(1)") {
                DataKind::Bool
            } else {
                DataKind::Bit
            }
        }
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" | "int1"
        | "int2" | "int3" | "int4" | "int8" | "middleint" | "serial" => DataKind::Int,
        "float" | "double" | "real" | "float4" | "float8" => DataKind::Float,
        "decimal" | "dec" | "numeric" | "fixed" => DataKind::Decimal,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "nchar"
        | "nvarchar" | "national" | "long" | "character" | "varcharacter" => DataKind::Text,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            DataKind::Binary
        }
        "enum" | "set" => DataKind::Enum,
        "json" => DataKind::Json,
        "date" => DataKind::Date,
        "time" => DataKind::Time,
        "datetime" | "timestamp" => DataKind::DateTime,
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" | "geomcollection" => DataKind::Geometry,
        "uuid" => DataKind::Uuid,
        "inet4" | "inet6" => DataKind::Network,
        "vector" => DataKind::Array,
        _ => DataKind::guess_from_type_name(type_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn my80() -> MySqlDialect {
        MySqlDialect::for_server(ServerProfile::new(Flavor::MySql, (8, 0, 36)))
    }

    #[test]
    fn truncate_without_foreign_key_checks() {
        let d = my80();
        let t = [ObjectRef::table(Some("shop".into()), "order")];
        assert_eq!(
            d.truncate_sql(&t, &BTreeMap::new()),
            ["TRUNCATE TABLE shop.`order`"]
        );
        let off = BTreeMap::from([("no_fk_checks".to_string(), true)]);
        assert_eq!(
            d.truncate_sql(&t, &off),
            [
                "SET FOREIGN_KEY_CHECKS = 0",
                "TRUNCATE TABLE shop.`order`",
                "SET FOREIGN_KEY_CHECKS = 1"
            ]
        );
    }

    #[test]
    fn quoting() {
        let d = MySqlDialect::new(Flavor::MySql);
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("Users"), "Users");
        assert_eq!(d.quote_ident("order"), "`order`");
        assert_eq!(d.quote_ident("key"), "`key`");
        assert_eq!(d.quote_ident("my table"), "`my table`");
        assert_eq!(d.quote_ident("1abc"), "`1abc`");
        assert_eq!(d.quote_ident("a`b"), "`a``b`");
        assert_eq!(d.qualified(Some("shop"), "order"), "shop.`order`");
        assert_eq!(d.kind(), DbKind::MySql);
        assert_eq!(MySqlDialect::new(Flavor::MariaDb).kind(), DbKind::MariaDb);
    }

    #[test]
    fn literals() {
        let d = MySqlDialect::new(Flavor::MySql);
        assert_eq!(
            d.string_literal("it's a \\ path\0"),
            "'it''s a \\\\ path\\0'"
        );
        assert_eq!(d.literal(&Value::bytes(vec![0xABu8, 1]), None), "X'AB01'");
        assert_eq!(d.literal(&Value::Bool(true), None), "TRUE");
        assert_eq!(d.literal(&Value::Int(-3), None), "-3");
        assert_eq!(d.literal(&Value::decimal("1.50"), None), "1.50");
        assert_eq!(d.literal(&Value::Null, None), "NULL");
        let arr = Value::Array(vec![Value::Int(1), Value::Int(2)].into_boxed_slice());
        assert_eq!(d.literal(&arr, None), "'[1,2]'");
        let dt = chrono::DateTime::parse_from_rfc3339("2024-01-02T10:00:00+07:00").unwrap();
        assert_eq!(
            d.literal(&Value::DateTimeTz(dt), None),
            "COALESCE(CONVERT_TZ('2024-01-02 03:00:00', '+00:00', @@session.time_zone), TIMESTAMPADD(SECOND, TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()), '2024-01-02 03:00:00'))"
        );
        let old = chrono::DateTime::parse_from_rfc3339("1965-03-01T00:00:00+00:00").unwrap();
        assert_eq!(
            d.literal(&Value::DateTimeTz(old), None),
            "TIMESTAMPADD(SECOND, TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()), '1965-03-01 00:00:00')"
        );
        assert_eq!(d.placeholder(3), "?");
        let mut p = ServerProfile::new(Flavor::MySql, (8, 0, 36));
        p.sql_mode = "NO_BACKSLASH_ESCAPES,ANSI_QUOTES".into();
        let nb = MySqlDialect::for_server(p);
        assert_eq!(nb.string_literal("a\\b'c"), "'a\\b''c'");
        assert!(!nb.syntax().double_quoted_strings);
        assert_eq!(nb.syntax().identifier_quotes.len(), 2);
    }

    #[test]
    fn bit_and_geometry_literals() {
        let d = MySqlDialect::for_server(ServerProfile::new(Flavor::MySql, (8, 0, 36)));
        assert!(
            d.inline_value("bit(8)") && d.inline_value("POINT") && !d.inline_value("varchar(10)")
        );
        assert_eq!(
            d.literal(&Value::text("00000101"), Some("bit(8)")),
            "b'00000101'"
        );
        assert_eq!(
            d.literal(&Value::text("SRID=4326;POINT(1 2)"), Some("geometry")),
            "ST_GeomFromText('POINT(1 2)', 4326, 'axis-order=long-lat')"
        );
        assert_eq!(
            d.literal(&Value::text("POINT(1 2)"), Some("point")),
            "ST_GeomFromText('POINT(1 2)')"
        );
        let maria = MySqlDialect::for_server(ServerProfile::new(Flavor::MariaDb, (11, 4, 0)));
        assert_eq!(
            maria.literal(&Value::text("SRID=4326;POINT(1 2)"), Some("geometry")),
            "ST_GeomFromText('POINT(1 2)', 4326)"
        );
        assert_eq!(d.literal(&Value::text("x"), Some("varchar(5)")), "'x'");
    }

    #[test]
    fn syntax_flags() {
        let s = mysql_syntax();
        assert!(
            s.hash_comments
                && s.dash_comment_needs_space
                && s.backslash_escapes
                && s.double_quoted_strings
        );
        assert!(
            s.question_params
                && !s.dollar_params
                && s.at_variables
                && s.delimiter_command
                && s.compound_blocks
        );
        assert!(s.x_strings && s.n_strings);
        assert_eq!(s.ident_case, IdentCase::Preserve);
        assert!(
            s.is_keyword("select") && s.is_function("json_extract") && s.is_data_type("mediumtext")
        );
    }

    #[test]
    fn limits() {
        let d = my80();
        assert_eq!(
            d.limit_query("select * from t;", 10, 0),
            "select * from t\nLIMIT 10"
        );
        assert_eq!(
            d.limit_query("select * from t -- c", 10, 20),
            "select * from t -- c\nLIMIT 10 OFFSET 20"
        );
        assert_eq!(
            d.limit_query("select * from t limit 5", 10, 0),
            "SELECT * FROM (select * from t limit 5) dbiji_q LIMIT 10"
        );
        assert_eq!(
            d.limit_query("select * from t for update", 1, 0),
            "SELECT * FROM (select * from t for update) dbiji_q LIMIT 1"
        );
        assert_eq!(
            d.limit_query("show tables", 1, 0),
            "SELECT * FROM (show tables) dbiji_q LIMIT 1"
        );
        assert_eq!(
            d.limit_query("select a from t where x in (select y from u limit 1)", 5, 0),
            "select a from t where x in (select y from u limit 1)\nLIMIT 5"
        );
        assert_eq!(
            d.count_query("select 1"),
            "SELECT COUNT(*) FROM (select 1) dbiji_cnt"
        );
    }

    #[test]
    fn upserts() {
        let cols = vec!["`id`".to_string(), "`name`".to_string()];
        let keys = vec!["`id`".to_string()];
        let vals = vec!["?".to_string(), "?".to_string()];
        assert_eq!(
            my80().upsert("`t`", &cols, &keys, &vals).unwrap(),
            "INSERT INTO `t` (`id`, `name`) VALUES (?, ?) AS new ON DUPLICATE KEY UPDATE `name` = new.`name`"
        );
        let maria = MySqlDialect::for_server(ServerProfile::new(Flavor::MariaDb, (11, 4, 0)));
        assert_eq!(
            maria.upsert("`t`", &cols, &keys, &vals).unwrap(),
            "INSERT INTO `t` (`id`, `name`) VALUES (?, ?) ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"
        );
        assert_eq!(
            maria
                .upsert("`t`", &keys, &keys, &["1".to_string()])
                .unwrap(),
            "INSERT INTO `t` (`id`) VALUES (1) ON DUPLICATE KEY UPDATE `id` = `id`"
        );
        assert!(maria.supports_returning() && !my80().supports_returning());
    }

    #[test]
    fn maintenance() {
        let d = my80();
        let targets = vec![
            ObjectRef::table(Some("db".into()), "a"),
            ObjectRef::table(Some("db".into()), "order"),
        ];
        let mut opts = BTreeMap::new();
        assert_eq!(
            d.maintenance_sql("analyze", &targets, &opts),
            vec!["ANALYZE TABLE db.a, db.`order`"]
        );
        opts.insert("local".to_string(), true);
        assert_eq!(
            d.maintenance_sql("optimize", &targets, &opts),
            vec!["OPTIMIZE NO_WRITE_TO_BINLOG TABLE db.a, db.`order`"]
        );
        let mut o2 = BTreeMap::new();
        o2.insert("quick".to_string(), true);
        o2.insert("changed".to_string(), true);
        assert_eq!(
            d.maintenance_sql("check", &targets[..1], &o2),
            vec!["CHECK TABLE db.a QUICK CHANGED"]
        );
        let mut o3 = BTreeMap::new();
        o3.insert("use_frm".to_string(), true);
        assert_eq!(
            d.maintenance_sql("repair", &targets[..1], &o3),
            vec!["REPAIR TABLE db.a USE_FRM"]
        );
        assert_eq!(
            d.maintenance_sql("flush", &[], &BTreeMap::new()),
            vec!["FLUSH TABLES"]
        );
        assert!(
            d.maintenance_sql("vacuum", &targets, &BTreeMap::new())
                .is_empty()
        );
        assert_eq!(d.maintenance_ops().len(), 5);
    }

    #[test]
    fn passwords() {
        let d = my80();
        assert_eq!(
            d.change_password_sql(None, "p'w\\d").unwrap(),
            "ALTER USER CURRENT_USER() IDENTIFIED BY 'p''w\\\\d'"
        );
        assert_eq!(
            d.change_password_sql(Some("bob@localhost"), "x").unwrap(),
            "ALTER USER 'bob'@'localhost' IDENTIFIED BY 'x'"
        );
        assert_eq!(
            d.change_password_sql(Some("'bob'@'%'"), "x").unwrap(),
            "ALTER USER 'bob'@'%' IDENTIFIED BY 'x'"
        );
        assert_eq!(
            d.change_password_sql(Some("alice"), "x").unwrap(),
            "ALTER USER 'alice' IDENTIFIED BY 'x'"
        );
        let m = MySqlDialect::new(Flavor::MariaDb);
        assert_eq!(
            m.change_password_sql(None, "x'y").unwrap(),
            "SET PASSWORD = PASSWORD('x''y')"
        );
        assert_eq!(
            m.change_password_sql(Some("bob@%"), "x").unwrap(),
            "SET PASSWORD FOR 'bob'@'%' = PASSWORD('x')"
        );
    }

    #[test]
    fn kinds() {
        assert_eq!(type_kind("tinyint(1)"), DataKind::Bool);
        assert_eq!(type_kind("tinyint(4)"), DataKind::Int);
        assert_eq!(type_kind("int unsigned"), DataKind::Int);
        assert_eq!(type_kind("bit(1)"), DataKind::Bool);
        assert_eq!(type_kind("bit(8)"), DataKind::Bit);
        assert_eq!(type_kind("decimal(10,2)"), DataKind::Decimal);
        assert_eq!(type_kind("enum('a','b')"), DataKind::Enum);
        assert_eq!(type_kind("longblob"), DataKind::Binary);
        assert_eq!(type_kind("datetime(6)"), DataKind::DateTime);
        assert_eq!(type_kind("multipolygon"), DataKind::Geometry);
        assert_eq!(type_kind("json"), DataKind::Json);
        assert_eq!(type_kind("year"), DataKind::Int);
    }
}
