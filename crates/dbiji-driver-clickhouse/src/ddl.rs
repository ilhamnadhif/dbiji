//! DDL generation for ClickHouse (`ClickHouseDdl`).
//!
//! ClickHouse has no foreign keys, unique constraints, triggers or
//! sequences; generator methods for those return a SQL comment explaining
//! that the operation is not supported (or nothing where the trait allows).
//! Table engine settings travel in [`TableDef::options`]:
//! `engine`, `order_by`, `primary_key`, `partition_by`, `sample_by`, `ttl`,
//! `settings`.

use crate::dialect::{ident, qualified, string_literal};
use crate::types::{make_nullable, strip_nullable};
use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::ansi::{match_columns, same_index, strip_trailing_semicolon};
use dbiji_core::value::DataKind;

/// DDL generator for ClickHouse.
#[derive(Debug, Default, Clone)]
pub struct ClickHouseDdl;

impl ClickHouseDdl {
    /// Create the generator.
    pub fn new() -> Self {
        ClickHouseDdl
    }
}

fn tname(t: &TableDef) -> String {
    qualified(t.schema.as_deref(), &t.name)
}

fn unsupported(what: &str) -> String {
    format!("-- ClickHouse does not support {what}")
}

/// Key columns: explicit `order_by` option or the primary key columns.
fn key_columns(t: &TableDef) -> Vec<String> {
    t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default()
}

fn in_key(t: &TableDef, column: &str) -> bool {
    if key_columns(t).iter().any(|c| c == column) {
        return true;
    }
    for opt in ["order_by", "sorting_key", "primary_key"] {
        if let Some(expr) = t.options.get(opt)
            && expr_mentions(expr, column)
        {
            return true;
        }
    }
    false
}

/// Does the expression list mention `column` as a whole word?
fn expr_mentions(expr: &str, column: &str) -> bool {
    expr.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '`')).any(|w| w.trim_matches('`') == column)
}

fn column_type(c: &ColumnDef, key: bool) -> String {
    let (inner, was_nullable) = strip_nullable(&c.data_type);
    if (c.nullable || (was_nullable && c.nullable)) && !key { make_nullable(&inner) } else { inner }
}

/// Column definition as used in `CREATE TABLE` / `ADD COLUMN` / `MODIFY COLUMN`.
pub fn column_definition(c: &ColumnDef, key: bool) -> String {
    let mut s = format!("{} {}", ident(&c.name), column_type(c, key));
    if let Some(g) = c.generated.as_deref().filter(|g| !g.trim().is_empty()) {
        let g = g.trim();
        let upper = g.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("ALIAS ") {
            let _ = rest;
            s.push_str(&format!(" ALIAS {}", g[6..].trim()));
        } else if upper.starts_with("EPHEMERAL") {
            s.push(' ');
            s.push_str(g);
        } else {
            s.push_str(&format!(" MATERIALIZED {g}"));
        }
    } else if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
        s.push_str(&format!(" COMMENT {}", string_literal(cm)));
    }
    s
}

fn wrap_list(expr: &str) -> String {
    let e = expr.trim();
    if e.is_empty() {
        return "tuple()".into();
    }
    if e.starts_with('(') && e.ends_with(')') || !e.contains(',') { e.to_string() } else { format!("({e})") }
}

/// Granularity from an `INDEX ... GRANULARITY n` definition.
fn index_granularity(def: Option<&str>) -> u32 {
    def.and_then(|d| {
        let upper = d.to_ascii_uppercase();
        let i = upper.rfind("GRANULARITY")?;
        d[i + 11..].split_whitespace().next()?.parse().ok()
    })
    .unwrap_or(1)
}

/// `INDEX name expr TYPE t GRANULARITY g`.
pub fn index_clause(idx: &IndexDef) -> String {
    if let Some(def) = idx.definition.as_deref().map(str::trim).filter(|d| d.to_ascii_uppercase().starts_with("INDEX ")) {
        return def.to_string();
    }
    let expr = if idx.columns.len() == 1 {
        let c = &idx.columns[0];
        if c.is_expression { c.name.clone() } else { ident(&c.name) }
    } else {
        let parts: Vec<String> = idx.columns.iter().map(|c| if c.is_expression { c.name.clone() } else { ident(&c.name) }).collect();
        format!("({})", parts.join(", "))
    };
    let ty = idx.method.as_deref().filter(|m| !m.is_empty()).unwrap_or("minmax");
    format!("INDEX {} {expr} TYPE {ty} GRANULARITY {}", ident(&idx.name), index_granularity(idx.definition.as_deref()))
}

fn check_clause(c: &CheckDef) -> String {
    let name = c.name.clone().unwrap_or_else(|| "chk".into());
    format!("CONSTRAINT {} CHECK {}", ident(&name), c.expression)
}

/// Secondary (data skipping) indexes of a table.
fn skipping_indexes(t: &TableDef) -> impl Iterator<Item = &IndexDef> {
    t.indexes.iter().filter(|i| !i.primary)
}

impl DdlGenerator for ClickHouseDdl {
    fn create_table(&self, t: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        let mut lines: Vec<String> = t.columns.iter().map(|c| column_definition(c, in_key(t, &c.name))).collect();
        for idx in skipping_indexes(t) {
            lines.push(index_clause(idx));
        }
        for c in &t.checks {
            lines.push(check_clause(c));
        }
        let engine = t.options.get("engine").map(String::as_str).filter(|e| !e.trim().is_empty()).unwrap_or("MergeTree");
        let mut sql = format!("CREATE TABLE {}\n(\n    {}\n)\nENGINE = {engine}", tname(t), lines.join(",\n    "));
        let is_merge_tree = engine.contains("MergeTree");
        if let Some(p) = t.options.get("partition_by").or_else(|| t.options.get("partition_key")).filter(|p| !p.is_empty()) {
            sql.push_str(&format!("\nPARTITION BY {p}"));
        }
        let order_by = t
            .options
            .get("order_by")
            .or_else(|| t.options.get("sorting_key"))
            .cloned()
            .filter(|o| !o.trim().is_empty())
            .unwrap_or_else(|| key_columns(t).iter().map(|c| ident(c)).collect::<Vec<_>>().join(", "));
        if let Some(pk) = t.options.get("primary_key").filter(|p| !p.is_empty() && **p != order_by) {
            sql.push_str(&format!("\nPRIMARY KEY {}", wrap_list(pk)));
        }
        if is_merge_tree {
            sql.push_str(&format!("\nORDER BY {}", wrap_list(&order_by)));
        }
        if let Some(s) = t.options.get("sample_by").or_else(|| t.options.get("sampling_key")).filter(|s| !s.is_empty()) {
            sql.push_str(&format!("\nSAMPLE BY {s}"));
        }
        if let Some(ttl) = t.options.get("ttl").filter(|s| !s.is_empty()) {
            sql.push_str(&format!("\nTTL {ttl}"));
        }
        if let Some(s) = t.options.get("settings").filter(|s| !s.is_empty()) {
            sql.push_str(&format!("\nSETTINGS {s}"));
        }
        if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
            sql.push_str(&format!("\nCOMMENT {}", string_literal(c)));
        }
        vec![sql]
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DROP TABLE {}", qualified(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!("RENAME TABLE {} TO {}", qualified(schema, old_name), qualified(schema, new_name))
    }

    fn alter_table(&self, from: &TableDef, to: &TableDef, renamed: &[(String, String)]) -> Vec<String> {
        let mut out = Vec::new();
        if from.name != to.name {
            out.push(self.rename_table(from.schema.as_deref(), &from.name, &to.name));
        }
        let t = tname(to);
        let (pairs, dropped, added) = match_columns(from, to, renamed);

        let from_idx: Vec<&IndexDef> = skipping_indexes(from).collect();
        let to_idx: Vec<&IndexDef> = skipping_indexes(to).collect();
        for i in &from_idx {
            if !to_idx.iter().any(|x| same_index(x, i) && x.method == i.method) {
                out.push(self.drop_index(to, i));
            }
        }
        for c in &from.checks {
            if !to.checks.contains(c) {
                out.push(self.drop_check(to, c));
            }
        }
        for c in dropped {
            out.push(self.drop_column(to, &c.name));
        }
        for (fc, tc) in &pairs {
            if fc.name != tc.name {
                out.push(format!("ALTER TABLE {t} RENAME COLUMN {} TO {}", ident(&fc.name), ident(&tc.name)));
            }
            let key = in_key(to, &tc.name);
            let type_changed = column_type(fc, key) != column_type(tc, key);
            let expr_changed = fc.default != tc.default || fc.generated != tc.generated;
            if type_changed || expr_changed {
                if tc.default.is_none() && tc.generated.is_none() && (fc.default.is_some() || fc.generated.is_some()) {
                    out.push(format!("ALTER TABLE {t} MODIFY COLUMN {} REMOVE DEFAULT", ident(&tc.name)));
                    if type_changed {
                        out.push(format!("ALTER TABLE {t} MODIFY COLUMN {} {}", ident(&tc.name), column_type(tc, key)));
                    }
                } else {
                    let mut c = (*tc).clone();
                    c.comment = None;
                    out.push(format!("ALTER TABLE {t} MODIFY COLUMN {}", column_definition(&c, key)));
                }
            }
            if fc.comment != tc.comment {
                out.push(format!("ALTER TABLE {t} COMMENT COLUMN {} {}", ident(&tc.name), string_literal(tc.comment.as_deref().unwrap_or(""))));
            }
        }
        for c in added {
            let mut stmt = self.add_column(to, c);
            // keep the column order of the target definition
            if let Some(pos) = to.columns.iter().position(|x| x.name == c.name) {
                if let (Some(s), Some(prev)) = (stmt.first_mut(), pos.checked_sub(1).and_then(|p| to.columns.get(p))) {
                    s.push_str(&format!(" AFTER {}", ident(&prev.name)));
                } else if pos == 0
                    && let Some(s) = stmt.first_mut()
                {
                    s.push_str(" FIRST");
                }
            }
            out.extend(stmt);
        }
        for i in &to_idx {
            if !from_idx.iter().any(|x| same_index(x, i) && x.method == i.method) {
                out.push(self.create_index(to, i));
            }
        }
        for c in &to.checks {
            if !from.checks.contains(c) {
                out.push(self.add_check(to, c));
            }
        }
        if from.primary_key != to.primary_key
            && let Some(pk) = &to.primary_key
        {
            let old = key_columns(from);
            // ClickHouse only allows appending (new) columns to the sorting key.
            if pk.columns.starts_with(&old) && pk.columns.len() > old.len() {
                out.push(self.add_primary_key(to, pk));
            }
        }
        if from.comment != to.comment {
            out.push(format!("ALTER TABLE {t} MODIFY COMMENT {}", string_literal(to.comment.as_deref().unwrap_or(""))));
        }
        out
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        vec![format!("ALTER TABLE {} ADD COLUMN {}", tname(table), column_definition(column, in_key(table, &column.name)))]
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!("ALTER TABLE {} DROP COLUMN {}", tname(table), ident(column))
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), index_clause(index))
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("ALTER TABLE {} DROP INDEX {}", tname(table), ident(&index.name))
    }

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        let cols: Vec<String> = key.columns.iter().map(|c| ident(c)).collect();
        format!("ALTER TABLE {} MODIFY ORDER BY ({})", tname(table), cols.join(", "))
    }

    fn drop_primary_key(&self, table: &TableDef, _key: &KeyDef) -> String {
        unsupported(&format!("dropping the primary key of {}", tname(table)))
    }

    fn add_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        unsupported("unique constraints")
    }

    fn drop_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        unsupported("unique constraints")
    }

    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), check_clause(check))
    }

    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
        format!("ALTER TABLE {} DROP CONSTRAINT {}", tname(table), ident(check.name.as_deref().unwrap_or("chk")))
    }

    fn add_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        unsupported("foreign keys")
    }

    fn drop_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        unsupported("foreign keys")
    }

    fn create_trigger(&self, _table: &TableDef, _trigger: &TriggerDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_trigger(&self, _table: &TableDef, _trigger: &TriggerDef) -> String {
        unsupported("triggers")
    }

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
        let name = qualified(view.schema.as_deref(), &view.name);
        let select = strip_trailing_semicolon(&view.definition);
        if view.materialized {
            let mut s = format!("CREATE MATERIALIZED VIEW {name}");
            if let Some(to) = view.options.get("to").filter(|t| !t.is_empty()) {
                s.push_str(&format!(" TO {to}"));
            } else {
                if let Some(e) = view.options.get("engine").filter(|e| !e.is_empty()) {
                    s.push_str(&format!("\nENGINE = {e}"));
                    if let Some(o) = view.options.get("order_by").filter(|o| !o.is_empty()) {
                        s.push_str(&format!("\nORDER BY {}", wrap_list(o)));
                    }
                }
                if view.options.get("populate").is_some_and(|p| p == "true" || p == "1") {
                    s.push_str("\nPOPULATE");
                }
            }
            s.push_str(&format!("\nAS {select}"));
            vec![s]
        } else {
            vec![format!("CREATE {}VIEW {name} AS\n{select}", if or_replace { "OR REPLACE " } else { "" })]
        }
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!("DROP VIEW {}", qualified(view.schema.as_deref(), &view.name))
    }

    fn create_sequence(&self, _seq: &SequenceDef) -> String {
        unsupported("sequences")
    }

    fn alter_sequence(&self, _from: &SequenceDef, _to: &SequenceDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_sequence(&self, _seq: &SequenceDef) -> String {
        unsupported("sequences")
    }

    fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
        if routine.definition.trim().is_empty() { Vec::new() } else { vec![routine.definition.trim().to_string()] }
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        format!("DROP FUNCTION {}", ident(&routine.name))
    }

    fn create_type(&self, ty: &TypeDef) -> Vec<String> {
        if ty.definition.trim().is_empty() { Vec::new() } else { vec![ty.definition.trim().to_string()] }
    }

    fn drop_type(&self, ty: &TypeDef) -> Option<String> {
        // "Types" are dictionaries in this driver.
        Some(format!("DROP DICTIONARY {}", qualified(ty.schema.as_deref(), &ty.name)))
    }

    fn create_schema(&self, name: &str) -> String {
        format!("CREATE DATABASE {}", ident(name))
    }

    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("DROP DATABASE {}", ident(name))
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        if source == DbKind::ClickHouse {
            return data_type.to_string();
        }
        map_foreign_type(source, data_type, kind).unwrap_or_else(|| self.default_type_for(kind))
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool => "Bool",
            DataKind::Int => "Int64",
            DataKind::Float => "Float64",
            DataKind::Decimal => "Decimal(38, 10)",
            DataKind::Date => "Date32",
            DataKind::DateTime => "DateTime64(6)",
            DataKind::DateTimeTz => "DateTime64(6, 'UTC')",
            DataKind::Uuid => "UUID",
            DataKind::Enum => "LowCardinality(String)",
            DataKind::Array => "Array(String)",
            _ => "String",
        }
        .to_string()
    }
}

/// A type name from another database, split into parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignType {
    /// Lower-case base name (`character varying`, `int`, `timestamp with time zone`).
    pub base: String,
    /// Arguments inside parentheses.
    pub args: Vec<String>,
    /// Number of array dimensions (`int[]`, `_int4`).
    pub array_dims: usize,
    /// `UNSIGNED` modifier (MySQL).
    pub unsigned: bool,
}

/// Parse a foreign type name such as `numeric(10,2)`, `int unsigned`,
/// `timestamp(3) with time zone`, `integer[]`, `_int4`.
pub fn parse_foreign_type(data_type: &str) -> ForeignType {
    let mut t = data_type.trim().to_ascii_lowercase();
    let mut array_dims = 0;
    loop {
        let trimmed = t.trim_end();
        if let Some(stripped) = trimmed.strip_suffix(']')
            && let Some(open) = stripped.rfind('[')
        {
            t = stripped[..open].to_string();
            array_dims += 1;
            continue;
        }
        break;
    }
    if t.starts_with('_') && t.len() > 1 {
        array_dims += 1;
        t = t[1..].to_string();
    }
    let mut args = Vec::new();
    let mut words = String::new();
    if let Some(open) = t.find('(') {
        let close = t[open..].find(')').map(|c| open + c).unwrap_or(t.len());
        args = t[open + 1..close].split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        words.push_str(&t[..open]);
        words.push(' ');
        if close < t.len() {
            words.push_str(&t[close + 1..]);
        }
    } else {
        words = t.clone();
    }
    let mut unsigned = false;
    let parts: Vec<&str> = words
        .split_whitespace()
        .filter(|w| match *w {
            "unsigned" => {
                unsigned = true;
                false
            }
            "signed" | "zerofill" => false,
            _ => true,
        })
        .collect();
    ForeignType { base: parts.join(" "), args, array_dims, unsigned }
}

fn arg_u32(ft: &ForeignType, i: usize) -> Option<u32> {
    ft.args.get(i).and_then(|a| a.parse().ok())
}

/// Map a type of another product to ClickHouse; `None` = use the kind default.
pub fn map_foreign_type(source: DbKind, data_type: &str, kind: DataKind) -> Option<String> {
    let ft = parse_foreign_type(data_type);
    let int = |bits: u16| -> String { if ft.unsigned { format!("UInt{bits}") } else { format!("Int{bits}") } };
    let base = ft.base.as_str();
    let mapped: String = match base {
        "bool" | "boolean" => "Bool".into(),
        "bit" => {
            if ft.args.is_empty() || ft.args.first().map(String::as_str) == Some("1") {
                "Bool".into()
            } else {
                "String".into()
            }
        }
        "tinyint" | "int1" => {
            if source.is_mysql_family() && ft.args.first().map(String::as_str) == Some("1") {
                "Bool".into()
            } else if source == DbKind::SqlServer || ft.unsigned {
                "UInt8".into()
            } else {
                "Int8".into()
            }
        }
        "smallint" | "int2" | "smallserial" | "serial2" => int(16),
        "mediumint" | "int" | "integer" | "int4" | "serial" | "serial4" | "int32" => int(32),
        "bigint" | "int8" | "bigserial" | "serial8" | "int64" | "long" => int(64),
        "hugeint" | "int128" => "Int128".into(),
        "uhugeint" => "UInt128".into(),
        "utinyint" => "UInt8".into(),
        "usmallint" => "UInt16".into(),
        "uinteger" => "UInt32".into(),
        "ubigint" => "UInt64".into(),
        "real" | "float4" | "float32" => "Float32".into(),
        "float" => match source {
            DbKind::MySql | DbKind::MariaDb => "Float32".into(),
            DbKind::SqlServer if arg_u32(&ft, 0).is_some_and(|p| p <= 24) => "Float32".into(),
            _ => "Float64".into(),
        },
        "double" | "double precision" | "float8" | "float64" => "Float64".into(),
        "numeric" | "decimal" | "dec" | "number" | "fixed" => match (arg_u32(&ft, 0), arg_u32(&ft, 1)) {
            (Some(p), s) => {
                let p = p.clamp(1, 76);
                format!("Decimal({p}, {})", s.unwrap_or(0).min(p))
            }
            (None, _) => "Decimal(38, 10)".into(),
        },
        "money" => "Decimal(19, 4)".into(),
        "smallmoney" => "Decimal(10, 4)".into(),
        "char" | "character" | "nchar" | "bpchar" | "varchar" | "character varying" | "nvarchar" | "varchar2" | "nvarchar2" | "text" | "ntext" | "tinytext" | "mediumtext"
        | "longtext" | "clob" | "citext" | "string" | "name" | "sysname" | "enum" | "set" | "xml" | "json" | "jsonb" | "interval" | "inet" | "cidr" | "macaddr" | "macaddr8"
        | "hierarchyid" | "sql_variant" | "struct" | "map" | "union" => "String".into(),
        "uuid" | "uniqueidentifier" => "UUID".into(),
        "date" => "Date32".into(),
        "time" | "timetz" | "time with time zone" | "time without time zone" => "String".into(),
        "datetime" if source == DbKind::SqlServer => "DateTime64(3)".into(),
        "smalldatetime" => "DateTime".into(),
        "timestamp" if source == DbKind::SqlServer => "String".into(), // rowversion
        "timestamp" | "timestamp without time zone" | "datetime" | "datetime2" => {
            let p = arg_u32(&ft, 0).unwrap_or(if base == "datetime2" { 7 } else { 6 }).min(9);
            format!("DateTime64({p})")
        }
        "timestamptz" | "timestamp with time zone" | "datetimeoffset" => {
            let p = arg_u32(&ft, 0).unwrap_or(6).min(9);
            format!("DateTime64({p}, 'UTC')")
        }
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image" | "rowversion" | "raw" | "bytes" => "String".into(),
        "year" => "UInt16".into(),
        "geometry" | "geography" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" => "String".into(),
        "list" | "array" => "Array(String)".into(),
        _ => return if ft.array_dims > 0 { Some(format!("Array({})", default_kind_type(kind))) } else { None },
    };
    let mut out = mapped;
    for _ in 0..ft.array_dims {
        out = format!("Array({out})");
    }
    Some(out)
}

fn default_kind_type(kind: DataKind) -> String {
    ClickHouseDdl.default_type_for(if kind == DataKind::Array { DataKind::Text } else { kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample() -> TableDef {
        let mut t = TableDef::new(Some("app".into()), "events");
        t.columns.push(ColumnDef::new("id", "UInt64").not_null());
        let mut name = ColumnDef::new("name", "String");
        name.comment = Some("the name".into());
        t.columns.push(name);
        t.columns.push(ColumnDef::new("ts", "DateTime64(3)").not_null().default_value("now64()"));
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        t.indexes.push(IndexDef { name: "idx_name".into(), columns: vec![IndexColumn::column("name")], method: Some("bloom_filter".into()), ..Default::default() });
        t.checks.push(CheckDef { name: Some("c_id".into()), expression: "id > 0".into() });
        t.comment = Some("all events".into());
        t
    }

    #[test]
    fn create_table() {
        let d = ClickHouseDdl::new();
        let sql = d.create_table(&sample(), true);
        assert_eq!(sql.len(), 1);
        let s = &sql[0];
        assert!(s.starts_with("CREATE TABLE app.events\n("), "{s}");
        assert!(s.contains("id UInt64,"), "{s}");
        assert!(s.contains("name Nullable(String) COMMENT 'the name'"), "{s}");
        assert!(s.contains("ts DateTime64(3) DEFAULT now64()"), "{s}");
        assert!(s.contains("INDEX idx_name name TYPE bloom_filter GRANULARITY 1"), "{s}");
        assert!(s.contains("CONSTRAINT c_id CHECK id > 0"), "{s}");
        assert!(s.contains("ENGINE = MergeTree\nORDER BY id"), "{s}");
        assert!(s.ends_with("COMMENT 'all events'"), "{s}");
    }

    #[test]
    fn create_table_with_options() {
        let d = ClickHouseDdl::new();
        let mut t = sample();
        t.primary_key = None;
        let mut o = BTreeMap::new();
        o.insert("engine".to_string(), "ReplacingMergeTree(ts)".to_string());
        o.insert("order_by".to_string(), "id, ts".to_string());
        o.insert("partition_by".to_string(), "toYYYYMM(ts)".to_string());
        o.insert("ttl".to_string(), "ts + INTERVAL 1 YEAR".to_string());
        t.options = o;
        let s = &d.create_table(&t, true)[0];
        assert!(s.contains("ENGINE = ReplacingMergeTree(ts)\nPARTITION BY toYYYYMM(ts)\nORDER BY (id, ts)\nTTL ts + INTERVAL 1 YEAR"), "{s}");
        let mut t2 = sample();
        t2.primary_key = None;
        t2.options.insert("engine".into(), "Memory".into());
        let s = &d.create_table(&t2, true)[0];
        assert!(!s.contains("ORDER BY"), "{s}");
        let mut t3 = sample();
        t3.primary_key = None;
        assert!(d.create_table(&t3, true)[0].contains("ORDER BY tuple()"));
    }

    #[test]
    fn alter_table() {
        let d = ClickHouseDdl::new();
        let from = sample();
        let mut to = from.clone();
        to.columns[1].data_type = "LowCardinality(String)".into();
        to.columns[1].comment = Some("changed".into());
        to.columns.push(ColumnDef::new("extra", "UInt8").not_null().default_value("0"));
        to.columns.retain(|c| c.name != "ts");
        to.indexes.clear();
        to.comment = Some("new comment".into());
        let alter = d.alter_table(&from, &to, &[]);
        assert_eq!(
            alter,
            vec![
                "ALTER TABLE app.events DROP INDEX idx_name",
                "ALTER TABLE app.events DROP COLUMN ts",
                "ALTER TABLE app.events MODIFY COLUMN name LowCardinality(Nullable(String))",
                "ALTER TABLE app.events COMMENT COLUMN name 'changed'",
                "ALTER TABLE app.events ADD COLUMN extra UInt8 DEFAULT 0 AFTER name",
                "ALTER TABLE app.events MODIFY COMMENT 'new comment'",
            ]
        );
        let mut renamed = from.clone();
        renamed.columns[1].name = "title".into();
        let alter = d.alter_table(&from, &renamed, &[("name".into(), "title".into())]);
        assert_eq!(alter, vec!["ALTER TABLE app.events RENAME COLUMN name TO title"]);
    }

    #[test]
    fn misc_statements() {
        let d = ClickHouseDdl::new();
        let t = sample();
        assert_eq!(d.drop_table(Some("app"), "events", true), "DROP TABLE app.events");
        assert_eq!(d.rename_table(Some("app"), "a", "b"), "RENAME TABLE app.a TO app.b");
        assert!(d.add_foreign_key(&t, &ForeignKeyDef::default()).starts_with("--"));
        let v = ViewDef { schema: Some("app".into()), name: "v".into(), definition: "SELECT 1;".into(), ..Default::default() };
        assert_eq!(d.create_view(&v, true), vec!["CREATE OR REPLACE VIEW app.v AS\nSELECT 1"]);
        let mut mv = v.clone();
        mv.materialized = true;
        mv.options.insert("to".into(), "app.target".into());
        assert_eq!(d.create_view(&mv, false), vec!["CREATE MATERIALIZED VIEW app.v TO app.target\nAS SELECT 1"]);
        assert_eq!(d.create_schema("analytics"), "CREATE DATABASE analytics");
        let idx =
            IndexDef { name: "i".into(), columns: vec![IndexColumn::column("a"), IndexColumn::column("b")], definition: Some("x GRANULARITY 4".into()), ..Default::default() };
        assert_eq!(d.create_index(&t, &idx), "ALTER TABLE app.events ADD INDEX i (a, b) TYPE minmax GRANULARITY 4");
    }

    #[test]
    fn type_mapping() {
        let d = ClickHouseDdl::new();
        let m = |src, t: &str| d.map_type(src, t, DataKind::guess_from_type_name(t));
        assert_eq!(m(DbKind::Postgres, "character varying(255)"), "String");
        assert_eq!(m(DbKind::Postgres, "integer"), "Int32");
        assert_eq!(m(DbKind::Postgres, "bigint"), "Int64");
        assert_eq!(m(DbKind::Postgres, "boolean"), "Bool");
        assert_eq!(m(DbKind::Postgres, "timestamp without time zone"), "DateTime64(6)");
        assert_eq!(m(DbKind::Postgres, "timestamp with time zone"), "DateTime64(6, 'UTC')");
        assert_eq!(m(DbKind::Postgres, "timestamptz"), "DateTime64(6, 'UTC')");
        assert_eq!(m(DbKind::Postgres, "numeric(12,2)"), "Decimal(12, 2)");
        assert_eq!(m(DbKind::Postgres, "numeric"), "Decimal(38, 10)");
        assert_eq!(m(DbKind::Postgres, "uuid"), "UUID");
        assert_eq!(m(DbKind::Postgres, "jsonb"), "String");
        assert_eq!(m(DbKind::Postgres, "bytea"), "String");
        assert_eq!(m(DbKind::Postgres, "integer[]"), "Array(Int32)");
        assert_eq!(m(DbKind::Postgres, "_text"), "Array(String)");
        assert_eq!(m(DbKind::MySql, "int unsigned"), "UInt32");
        assert_eq!(m(DbKind::MySql, "tinyint(1)"), "Bool");
        assert_eq!(m(DbKind::MySql, "bigint(20) unsigned"), "UInt64");
        assert_eq!(m(DbKind::SqlServer, "tinyint"), "UInt8");
        assert_eq!(m(DbKind::SqlServer, "nvarchar(max)"), "String");
        assert_eq!(m(DbKind::SqlServer, "datetime2(7)"), "DateTime64(7)");
        assert_eq!(m(DbKind::SqlServer, "datetimeoffset"), "DateTime64(6, 'UTC')");
        assert_eq!(m(DbKind::SqlServer, "bit"), "Bool");
        assert_eq!(m(DbKind::SqlServer, "money"), "Decimal(19, 4)");
        assert_eq!(m(DbKind::DuckDb, "HUGEINT"), "Int128");
        assert_eq!(m(DbKind::ClickHouse, "LowCardinality(String)"), "LowCardinality(String)");
        assert_eq!(d.map_type(DbKind::Postgres, "weird_type", DataKind::Int), "Int64");
        assert_eq!(d.default_type_for(DataKind::Text), "String");
        assert_eq!(d.default_type_for(DataKind::DateTimeTz), "DateTime64(6, 'UTC')");
    }

    #[test]
    fn foreign_type_parse() {
        let ft = parse_foreign_type("NUMERIC(10, 2)[]");
        assert_eq!(ft.base, "numeric");
        assert_eq!(ft.args, vec!["10", "2"]);
        assert_eq!(ft.array_dims, 1);
        let ft = parse_foreign_type("timestamp(3) with time zone");
        assert_eq!(ft.base, "timestamp with time zone");
        assert_eq!(ft.args, vec!["3"]);
        let ft = parse_foreign_type("int(11) unsigned zerofill");
        assert!(ft.unsigned);
        assert_eq!(ft.base, "int");
    }
}
