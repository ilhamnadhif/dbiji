//! CQL dialect and DDL: lower-case identifiers, no subqueries (row limits
//! and counts are rewritten instead of wrapped), keyspaces as schemas.

use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::meta::ObjectRef;
use dbiji_core::value::{DataKind, Value};

/// Reserved CQL keywords (they must be quoted as names).
const RESERVED: &[&str] = &[
    "ADD",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "COLUMNFAMILY",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DROP",
    "ENTRIES",
    "EXECUTE",
    "FROM",
    "FULL",
    "GRANT",
    "IF",
    "IN",
    "INDEX",
    "INFINITY",
    "INSERT",
    "INTO",
    "KEYSPACE",
    "LIMIT",
    "MODIFY",
    "NAN",
    "NORECURSIVE",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "RENAME",
    "REPLACE",
    "REVOKE",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TO",
    "TOKEN",
    "TRUNCATE",
    "UNLOGGED",
    "UPDATE",
    "USE",
    "USING",
    "VIEW",
    "WHERE",
    "WITH",
];

const KEYWORDS: &[&str] = &[
    "ADD",
    "AGGREGATE",
    "ALL",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "AS",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "CALLED",
    "CLUSTERING",
    "COMPACT",
    "CONTAINS",
    "COUNTER",
    "CREATE",
    "CUSTOM",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ENTRIES",
    "EXISTS",
    "FILTERING",
    "FROM",
    "FROZEN",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "IF",
    "IN",
    "INDEX",
    "INSERT",
    "INTO",
    "IS",
    "JSON",
    "KEY",
    "KEYS",
    "KEYSPACE",
    "KEYSPACES",
    "LANGUAGE",
    "LIKE",
    "LIMIT",
    "LIST",
    "LOGGED",
    "MATERIALIZED",
    "MODIFY",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PARTITION",
    "PER",
    "PERMISSIONS",
    "PRIMARY",
    "RENAME",
    "REPLACE",
    "RETURNS",
    "REVOKE",
    "ROLE",
    "SCHEMA",
    "SELECT",
    "SET",
    "STATIC",
    "TABLE",
    "TO",
    "TOKEN",
    "TRIGGER",
    "TRUNCATE",
    "TTL",
    "TYPE",
    "UNLOGGED",
    "UNSET",
    "UPDATE",
    "USE",
    "USER",
    "USING",
    "VALUES",
    "VIEW",
    "WHERE",
    "WITH",
    "WRITETIME",
];

const FUNCTIONS: &[&str] = &[
    "avg",
    "blobasbigint",
    "bigintasblob",
    "cast",
    "count",
    "currentdate",
    "currenttime",
    "currenttimestamp",
    "currenttimeuuid",
    "dateof",
    "fromjson",
    "max",
    "maxtimeuuid",
    "min",
    "mintimeuuid",
    "now",
    "sum",
    "toDate",
    "toJson",
    "toTimestamp",
    "toUnixTimestamp",
    "token",
    "ttl",
    "uuid",
    "writetime",
];

const TYPES: &[&str] = &[
    "ascii",
    "bigint",
    "blob",
    "boolean",
    "counter",
    "date",
    "decimal",
    "double",
    "duration",
    "float",
    "frozen",
    "inet",
    "int",
    "list",
    "map",
    "set",
    "smallint",
    "text",
    "time",
    "timestamp",
    "timeuuid",
    "tinyint",
    "tuple",
    "uuid",
    "varchar",
    "varint",
    "vector",
];

pub const SYNTAX: SqlSyntax = SqlSyntax {
    keywords: KEYWORDS,
    functions: FUNCTIONS,
    data_types: TYPES,
    identifier_quotes: &[('"', '"')],
    double_quoted_strings: false,
    backslash_escapes: false,
    e_strings: false,
    n_strings: false,
    x_strings: false,
    dollar_quoting: true,
    hash_comments: false,
    nested_block_comments: false,
    dash_comment_needs_space: false,
    question_params: true,
    dollar_params: false,
    at_variables: false,
    batch_separator: None,
    delimiter_command: false,
    compound_blocks: false,
    plsql_blocks: false,
    newline_ends_statement: false,
    extra_ident_chars: "",
    ident_case: IdentCase::Lower,
    case_sensitive_names: false,
};

/// Unquoted names are folded to lower case.
pub(crate) fn needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    if !(first.is_ascii_lowercase() || first == '_')
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return true;
    }
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(ident))
}

pub(crate) fn quote(ident: &str) -> String {
    if needs_quoting(ident) {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
}

pub(crate) fn qualified(keyspace: Option<&str>, name: &str) -> String {
    match keyspace {
        Some(k) if !k.is_empty() => format!("{}.{}", quote(k), quote(name)),
        _ => quote(name),
    }
}

/// Byte offset of the top-level `FROM` of a `SELECT` (outside strings,
/// quotes and parentheses).
fn top_level_from(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if depth == 0
                && c.eq_ignore_ascii_case(&b'f')
                && sql[i..].len() >= 4
                && sql[i..i + 4].eq_ignore_ascii_case("from") =>
            {
                let before = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                let after = b
                    .get(i + 4)
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
                if before && after {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `sql` without a trailing `LIMIT n` (top level, at the end).
fn strip_limit(sql: &str) -> &str {
    let upper = sql.to_ascii_uppercase();
    match upper.rfind(" LIMIT ") {
        Some(i) if upper[i + 7..].trim().chars().all(|c| c.is_ascii_digit()) => sql[..i].trim_end(),
        _ => sql,
    }
}

pub struct CqlDialect {
    ddl: CqlDdl,
}

impl CqlDialect {
    pub fn new() -> Self {
        CqlDialect { ddl: CqlDdl }
    }
}

impl Default for CqlDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for CqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::Cassandra
    }

    fn syntax(&self) -> &SqlSyntax {
        &SYNTAX
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        needs_quoting(ident)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        format!("0x{}", hex::encode(b))
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let _ = type_name;
        match value {
            Value::Uuid(u) => u.to_string(),
            Value::DateTime(t) => format!("'{}'", t.format("%Y-%m-%d %H:%M:%S%.3f")),
            Value::DateTimeTz(t) => format!("'{}'", t.format("%Y-%m-%d %H:%M:%S%.3f%z")),
            _ => ansi::literal(self, value),
        }
    }

    /// No subqueries: the limit is appended (or replaces the query's own).
    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let inner = strip_limit(ansi::strip_trailing_semicolon(sql).trim());
        format!("{inner} LIMIT {}", limit.saturating_add(offset))
    }

    fn count_query(&self, sql: &str) -> String {
        self.aggregate_query(sql, "COUNT(*)")
    }

    fn aggregate_query(&self, sql: &str, select_list: &str) -> String {
        let s = strip_limit(ansi::strip_trailing_semicolon(sql).trim());
        match top_level_from(s) {
            Some(i) => format!("SELECT {select_list} {}", &s[i..]),
            None => format!("SELECT {select_list} FROM ({s})"),
        }
    }

    /// Filters on other than key columns need ALLOW FILTERING.
    fn select_table(
        &self,
        table: &ObjectRef,
        columns: Option<&[String]>,
        where_clause: Option<&str>,
        order_by: Option<&str>,
        limit: Option<u64>,
    ) -> String {
        let cols = match columns {
            Some(c) if !c.is_empty() => c.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "),
            _ => "*".to_string(),
        };
        let mut sql = format!(
            "SELECT {cols} FROM {}",
            qualified(table.schema.as_deref(), &table.name)
        );
        let filtered = where_clause.is_some_and(|w| !w.trim().is_empty());
        if let Some(w) = where_clause.filter(|w| !w.trim().is_empty()) {
            sql.push_str(" WHERE ");
            sql.push_str(w);
        }
        if let Some(o) = order_by.filter(|o| !o.trim().is_empty()) {
            sql.push_str(" ORDER BY ");
            sql.push_str(o);
        }
        if let Some(l) = limit {
            sql.push_str(&format!(" LIMIT {l}"));
        }
        if filtered {
            sql.push_str(" ALLOW FILTERING");
        }
        sql
    }

    /// INSERT is an upsert in CQL.
    fn upsert(
        &self,
        table: &str,
        columns: &[String],
        key_columns: &[String],
        values: &[String],
    ) -> Option<String> {
        let _ = key_columns;
        Some(format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            columns.join(", "),
            values.join(", ")
        ))
    }

    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!(
            "TRUNCATE {}",
            qualified(table.schema.as_deref(), &table.name)
        )
    }

    fn create_schema_sql(&self, name: &str, owner: Option<&str>) -> Option<String> {
        let _ = owner;
        (!name.trim().is_empty()).then(|| self.ddl.create_schema(name.trim()))
    }
}

pub struct CqlDdl;

fn list(cols: &[String]) -> String {
    cols.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `PRIMARY KEY ((a, b), c)`: the `partition_key` option names the partition
/// columns (else the first key column is the partition key).
fn primary_key(t: &TableDef) -> Option<String> {
    let pk = t.primary_key.as_ref().filter(|k| !k.columns.is_empty())?;
    let partition: Vec<String> = match t.options.get("partition_key") {
        Some(p) => p
            .split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        None => vec![pk.columns[0].clone()],
    };
    let clustering: Vec<String> = pk
        .columns
        .iter()
        .filter(|c| !partition.contains(c))
        .cloned()
        .collect();
    let part = if partition.len() == 1 {
        quote(&partition[0])
    } else {
        format!("({})", list(&partition))
    };
    Some(if clustering.is_empty() {
        format!("PRIMARY KEY ({part})")
    } else {
        format!("PRIMARY KEY ({part}, {})", list(&clustering))
    })
}

impl DdlGenerator for CqlDdl {
    fn create_table(&self, t: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{} {}{}",
                    quote(&c.name),
                    c.data_type,
                    if t.options
                        .get("static_columns")
                        .is_some_and(|s| s.split(',').any(|x| x.trim() == c.name))
                    {
                        " STATIC"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        if let Some(pk) = primary_key(t) {
            lines.push(pk);
        }
        let mut with = Vec::new();
        if let Some(order) = t.options.get("clustering_order").filter(|o| !o.is_empty()) {
            with.push(format!("CLUSTERING ORDER BY ({order})"));
        }
        if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
            with.push(format!("comment = {}", lit(c)));
        }
        let mut sql = format!(
            "CREATE TABLE {} (\n    {}\n)",
            qualified(t.schema.as_deref(), &t.name),
            lines.join(",\n    ")
        );
        if !with.is_empty() {
            sql.push_str(&format!(" WITH {}", with.join("\n    AND ")));
        }
        let mut out = vec![sql];
        for idx in &t.indexes {
            out.push(self.create_index(t, idx));
        }
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DROP TABLE {}", qualified(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!(
            "-- Cassandra cannot rename tables ({} to {})",
            qualified(schema, old_name),
            quote(new_name)
        )
    }

    fn alter_table(
        &self,
        from: &TableDef,
        to: &TableDef,
        renamed: &[(String, String)],
    ) -> Vec<String> {
        let t = qualified(to.schema.as_deref(), &to.name);
        let (pairs, dropped, added) = ansi::match_columns(from, to, renamed);
        let mut out = Vec::new();
        for c in dropped {
            out.push(self.drop_column(to, &c.name));
        }
        for (fc, tc) in pairs {
            if fc.name != tc.name {
                // Only primary key columns can be renamed.
                out.push(format!(
                    "ALTER TABLE {t} RENAME {} TO {}",
                    quote(&fc.name),
                    quote(&tc.name)
                ));
            }
        }
        for c in added {
            out.extend(self.add_column(to, c));
        }
        if from.comment != to.comment {
            out.push(format!(
                "ALTER TABLE {t} WITH comment = {}",
                lit(to.comment.as_deref().unwrap_or(""))
            ));
        }
        for i in &from.indexes {
            if !to.indexes.iter().any(|x| ansi::same_index(x, i)) {
                out.push(self.drop_index(to, i));
            }
        }
        for i in &to.indexes {
            if !from.indexes.iter().any(|x| ansi::same_index(x, i)) {
                out.push(self.create_index(to, i));
            }
        }
        out
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        vec![format!(
            "ALTER TABLE {} ADD {} {}",
            qualified(table.schema.as_deref(), &table.name),
            quote(&column.name),
            column.data_type
        )]
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!(
            "ALTER TABLE {} DROP {}",
            qualified(table.schema.as_deref(), &table.name),
            quote(column)
        )
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        let target = index
            .columns
            .first()
            .map(|c| {
                if c.is_expression {
                    c.name.clone()
                } else {
                    quote(&c.name)
                }
            })
            .unwrap_or_default();
        format!(
            "CREATE INDEX {} ON {} ({target})",
            quote(&index.name),
            qualified(table.schema.as_deref(), &table.name)
        )
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!(
            "DROP INDEX {}",
            qualified(table.schema.as_deref(), &index.name)
        )
    }

    fn add_primary_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        "-- The primary key of a Cassandra table cannot change".into()
    }
    fn drop_primary_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        "-- The primary key of a Cassandra table cannot change".into()
    }
    fn add_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        "-- Cassandra has no unique keys".into()
    }
    fn drop_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        "-- Cassandra has no unique keys".into()
    }
    fn add_check(&self, _table: &TableDef, _check: &CheckDef) -> String {
        "-- Cassandra has no check constraints".into()
    }
    fn drop_check(&self, _table: &TableDef, _check: &CheckDef) -> String {
        "-- Cassandra has no check constraints".into()
    }
    fn add_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        "-- Cassandra has no foreign keys".into()
    }
    fn drop_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        "-- Cassandra has no foreign keys".into()
    }
    fn create_trigger(&self, _table: &TableDef, trigger: &TriggerDef) -> Vec<String> {
        vec![trigger.definition.clone()]
    }
    fn drop_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> String {
        format!(
            "DROP TRIGGER {} ON {}",
            quote(&trigger.name),
            qualified(table.schema.as_deref(), &table.name)
        )
    }

    fn create_view(&self, view: &ViewDef, _or_replace: bool) -> Vec<String> {
        vec![format!(
            "CREATE MATERIALIZED VIEW {} AS\n{}",
            qualified(view.schema.as_deref(), &view.name),
            ansi::strip_trailing_semicolon(&view.definition)
        )]
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!(
            "DROP MATERIALIZED VIEW {}",
            qualified(view.schema.as_deref(), &view.name)
        )
    }

    fn create_sequence(&self, _seq: &SequenceDef) -> String {
        "-- Cassandra has no sequences".into()
    }
    fn alter_sequence(&self, _from: &SequenceDef, _to: &SequenceDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_sequence(&self, _seq: &SequenceDef) -> String {
        "-- Cassandra has no sequences".into()
    }

    fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
        vec![routine.definition.clone()]
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        format!(
            "DROP FUNCTION {}",
            qualified(routine.schema.as_deref(), &routine.name)
        )
    }

    fn create_type(&self, ty: &TypeDef) -> Vec<String> {
        vec![ty.definition.clone()]
    }

    fn drop_type(&self, ty: &TypeDef) -> Option<String> {
        Some(format!(
            "DROP TYPE {}",
            qualified(ty.schema.as_deref(), &ty.name)
        ))
    }

    /// A keyspace for one data center / development (replication factor 1).
    fn create_schema(&self, name: &str) -> String {
        format!(
            "CREATE KEYSPACE {} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}",
            quote(name)
        )
    }

    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("DROP KEYSPACE {}", quote(name))
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        map_type(source, data_type, kind)
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        default_type_for(kind).into()
    }
}

pub(crate) fn default_type_for(kind: DataKind) -> &'static str {
    match kind {
        DataKind::Bool => "boolean",
        DataKind::Int => "bigint",
        DataKind::Float => "double",
        DataKind::Decimal => "decimal",
        DataKind::Binary => "blob",
        DataKind::Date => "date",
        DataKind::Time | DataKind::TimeTz => "time",
        DataKind::DateTime | DataKind::DateTimeTz => "timestamp",
        DataKind::Uuid => "uuid",
        DataKind::Interval => "duration",
        DataKind::Network => "inet",
        _ => "text",
    }
}

pub(crate) fn map_type(source: DbKind, data_type: &str, kind: DataKind) -> String {
    let raw = data_type.trim();
    if source == DbKind::Cassandra && !raw.is_empty() {
        return raw.to_string();
    }
    let lower = raw.to_ascii_lowercase();
    if lower.ends_with("[]") {
        return "list<text>".into();
    }
    let base = lower
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(" unsigned");
    match base {
        "tinyint" | "int1" | "int8" if source.is_mysql_family() && lower.contains("(1)") => {
            "boolean".into()
        }
        "tinyint" | "int1" => "tinyint".into(),
        "smallint" | "int2" | "int16" => "smallint".into(),
        "int" | "integer" | "int4" | "mediumint" | "int32" | "serial" => "int".into(),
        "bigint" | "int8" | "int64" | "bigserial" => "bigint".into(),
        "decimal" | "numeric" | "number" | "money" => "decimal".into(),
        "real" | "float4" | "float32" | "binary_float" => "float".into(),
        "float" | "double" | "double precision" | "float8" | "float64" | "binary_double" => {
            "double".into()
        }
        "bool" | "boolean" | "bit" => "boolean".into(),
        "blob" | "bytea" | "binary" | "varbinary" | "raw" | "image" | "longblob" => "blob".into(),
        "uuid" | "uniqueidentifier" => "uuid".into(),
        "date" | "date32" => "date".into(),
        "time" | "timetz" => "time".into(),
        "timestamp" | "timestamptz" | "datetime" | "datetime2" | "datetimeoffset"
        | "datetime64" => "timestamp".into(),
        "interval" => "duration".into(),
        "inet" | "cidr" | "ipv4" | "ipv6" => "inet".into(),
        _ => default_type_for(kind).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_without_subqueries() {
        let d = CqlDialect::new();
        assert_eq!(
            d.limit_query("SELECT * FROM ks.t;", 200, 0),
            "SELECT * FROM ks.t LIMIT 200"
        );
        assert_eq!(
            d.limit_query("SELECT * FROM ks.t LIMIT 10", 200, 0),
            "SELECT * FROM ks.t LIMIT 200"
        );
        assert_eq!(
            d.count_query("SELECT id, name FROM ks.t WHERE id = 1 LIMIT 5"),
            "SELECT COUNT(*) FROM ks.t WHERE id = 1"
        );
        assert_eq!(
            d.count_query("select a, (b) from t"),
            "SELECT COUNT(*) from t"
        );
        let t = ObjectRef::table(Some("Shop".into()), "users");
        assert_eq!(
            d.select_table(&t, None, Some("age > 3"), None, Some(10)),
            "SELECT * FROM \"Shop\".users WHERE age > 3 LIMIT 10 ALLOW FILTERING"
        );
        assert_eq!(
            d.literal(&Value::Uuid(uuid::Uuid::nil()), None),
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(d.quote_ident("Name"), "\"Name\"");
        assert_eq!(d.quote_ident("token"), "\"token\"");
    }

    #[test]
    fn tables_with_partition_and_clustering_keys() {
        let mut t = TableDef::new(Some("shop".into()), "events");
        t.columns = vec![
            ColumnDef::new("tenant", "text"),
            ColumnDef::new("day", "date"),
            ColumnDef::new("at", "timestamp"),
            ColumnDef::new("payload", "text"),
        ];
        t.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["tenant".into(), "day".into(), "at".into()],
        });
        t.options
            .insert("partition_key".into(), "tenant, day".into());
        t.options
            .insert("clustering_order".into(), "at DESC".into());
        t.comment = Some("Events".into());
        assert_eq!(
            CqlDdl.create_table(&t, false)[0],
            "CREATE TABLE shop.events (\n    tenant text,\n    day date,\n    at timestamp,\n    payload text,\n    PRIMARY KEY ((tenant, day), at)\n) WITH CLUSTERING ORDER BY (at DESC)\n    AND comment = 'Events'"
        );
        assert_eq!(
            map_type(DbKind::Postgres, "varchar(20)", DataKind::Text),
            "text"
        );
        assert_eq!(
            map_type(DbKind::MySql, "tinyint(1)", DataKind::Int),
            "boolean"
        );
        assert_eq!(
            map_type(DbKind::Oracle, "NUMBER(10,2)", DataKind::Decimal),
            "decimal"
        );
    }
}
