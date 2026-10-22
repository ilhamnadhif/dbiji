//! Metadata access for ClickHouse through the `system` database.

use crate::dialect::{ident, qualified};
use crate::session::{ClickHouseSession, lit};
use crate::types::strip_nullable;
use dbiji_core::admin::Metric;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{DbError, DbKind, DbResult, ErrorKind, Row, Value};
use std::collections::BTreeMap;

/// Databases that belong to the server itself.
pub fn is_system_db(name: &str) -> bool {
    matches!(name, "system" | "INFORMATION_SCHEMA" | "information_schema")
}

fn text(v: Option<&Value>) -> String {
    v.map(|v| v.to_text().into_owned()).unwrap_or_default()
}

fn opt(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_text().into_owned()).filter(|s| !s.is_empty()),
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

fn truthy(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool).unwrap_or(false)
}

fn schema_of(s: &ClickHouseSession, schema: &SchemaRef) -> String {
    schema.schema.clone().unwrap_or_else(|| s.db_or_default())
}

fn obj_schema(s: &ClickHouseSession, obj: &ObjectRef) -> String {
    obj.schema.clone().unwrap_or_else(|| s.db_or_default())
}

/// Run `primary`, and `fallback` when the server does not know a column
/// used by `primary` (older ClickHouse versions).
async fn rows_or(s: &mut ClickHouseSession, primary: &str, fallback: &str) -> DbResult<Vec<Row>> {
    match s.rows(primary).await {
        Ok(r) => Ok(r),
        Err(e) if e.kind == ErrorKind::Query && e.code.as_deref().is_some_and(|c| c.starts_with("47") || c.starts_with("16")) => s.rows(fallback).await,
        Err(e) => Err(e),
    }
}

// ------------------------------------------------------------ parsing helpers

/// Split `s` at top-level commas (outside parentheses and quotes).
pub fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '`' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The parenthesised element list of a `CREATE TABLE` statement.
fn create_body(create: &str) -> Option<&str> {
    let start = create.find('(')?;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escape = false;
    for (i, c) in create[start..].char_indices() {
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '`' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&create[start + 1..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `CONSTRAINT name CHECK expr` elements of a `CREATE TABLE` statement.
pub fn parse_constraints(create: &str) -> Vec<CheckDef> {
    let Some(body) = create_body(create) else { return Vec::new() };
    split_top_level(body)
        .into_iter()
        .filter_map(|el| {
            let upper = el.to_ascii_uppercase();
            if !upper.starts_with("CONSTRAINT ") {
                return None;
            }
            let rest = el[11..].trim_start();
            let (name, after) = split_ident(rest)?;
            let after = after.trim_start();
            let upper_after = after.to_ascii_uppercase();
            let expr = upper_after.strip_prefix("CHECK").map(|_| after[5..].trim())?;
            Some(CheckDef { name: Some(name), expression: expr.to_string() })
        })
        .collect()
}

/// Leading identifier (plain or backtick quoted) and the remainder.
fn split_ident(s: &str) -> Option<(String, &str)> {
    if let Some(r) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut chars = r.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        name.push(n);
                    }
                }
                '`' => return Some((name, &r[i + 1..])),
                c => name.push(c),
            }
        }
        None
    } else {
        let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
        Some((s[..end].to_string(), &s[end..]))
    }
}

/// Split `engine_full` into the engine (with parameters), TTL and SETTINGS.
pub fn split_engine_full(engine_full: &str) -> (String, Option<String>, Option<String>) {
    const CLAUSES: [&str; 6] = [" PARTITION BY ", " PRIMARY KEY ", " ORDER BY ", " SAMPLE BY ", " TTL ", " SETTINGS "];
    // positions of top-level clause keywords
    let upper = engine_full.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut positions: Vec<(usize, &str)> = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'`' | b'"' => quote = Some(b),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b' ' if depth == 0 => {
                for c in CLAUSES {
                    if upper[i..].starts_with(c) {
                        positions.push((i, c));
                        break;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    let engine_end = positions.first().map(|(p, _)| *p).unwrap_or(engine_full.len());
    let engine = engine_full[..engine_end].trim().to_string();
    let clause = |name: &str| -> Option<String> {
        let idx = positions.iter().position(|(_, c)| *c == name)?;
        let start = positions[idx].0 + name.len();
        let end = positions.get(idx + 1).map(|(p, _)| *p).unwrap_or(engine_full.len());
        Some(engine_full[start..end].trim().to_string()).filter(|s| !s.is_empty())
    };
    (engine, clause(" TTL "), clause(" SETTINGS "))
}

/// `TO db.table` target of a materialized view definition.
fn mv_target(create: &str) -> Option<String> {
    let upper = create.to_ascii_uppercase();
    let i = upper.find(" TO ")?;
    let as_pos = upper.find(" AS ").unwrap_or(usize::MAX);
    if i > as_pos {
        return None;
    }
    create[i + 4..].split_whitespace().next().map(str::to_string)
}

/// `MergeTree ORDER BY a SETTINGS …` of `CREATE MATERIALIZED VIEW v (…) ENGINE = … AS SELECT …`.
fn mv_engine(create: &str) -> Option<String> {
    let upper = create.to_ascii_uppercase();
    let e = upper.find(" ENGINE = ")? + " ENGINE = ".len();
    let end = upper[e..].find(" AS ").map(|i| e + i)?;
    Some(create[e..end].trim().to_string()).filter(|s| !s.is_empty())
}

fn is_plain_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '`' || c == '.')
}

fn key_list(expr: &str) -> Vec<String> {
    let e = expr.trim();
    let e = e.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(e);
    split_top_level(e).into_iter().map(|c| c.trim_matches('`').to_string()).filter(|c| !c.is_empty()).collect()
}

/// LIKE pattern for a metadata search pattern (`*`/`?` wildcards, plain
/// text = contains).
pub fn like_pattern(pattern: &str) -> String {
    let has_wild = pattern.contains('*') || pattern.contains('?');
    let mut out = String::new();
    for c in pattern.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    if has_wild { out } else { format!("%{out}%") }
}

// ------------------------------------------------------------ navigator

pub async fn list_databases(s: &mut ClickHouseSession) -> DbResult<Vec<DatabaseInfo>> {
    let rows = rows_or(s, "SELECT name, engine, comment FROM system.databases ORDER BY name", "SELECT name, engine, '' FROM system.databases ORDER BY name").await?;
    let sizes: BTreeMap<String, i64> = s
        .rows("SELECT database, sum(total_bytes) FROM system.tables GROUP BY database")
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| Some((text(r.first()), int(r.get(1))?)))
        .collect();
    let current = s.db_or_default();
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = text(r.first());
            DatabaseInfo {
                is_current: name == current,
                is_system: is_system_db(&name),
                owner: None,
                comment: opt(r.get(2)),
                encoding: None,
                size_bytes: sizes.get(&name).copied(),
                name,
            }
        })
        .collect())
}

pub async fn list_schemas(s: &mut ClickHouseSession) -> DbResult<Vec<SchemaInfo>> {
    Ok(list_databases(s).await?.into_iter().map(|d| SchemaInfo { name: d.name, is_current: d.is_current, is_system: d.is_system, owner: None, comment: d.comment }).collect())
}

fn engine_filter(kind: ObjectKind) -> Option<&'static str> {
    Some(match kind {
        ObjectKind::Table => "engine NOT IN ('View', 'MaterializedView', 'LiveView', 'WindowView', 'Dictionary') AND name NOT LIKE '.inner%'",
        ObjectKind::View => "engine IN ('View', 'LiveView', 'WindowView')",
        ObjectKind::MaterializedView => "engine = 'MaterializedView'",
        _ => return None,
    })
}

pub async fn list_objects(s: &mut ClickHouseSession, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
    let db = schema_of(s, schema);
    let system = is_system_db(&db);
    if let Some(filter) = engine_filter(kind) {
        let sql = format!(
            "SELECT name, engine, comment, total_rows, total_bytes FROM system.tables \
             WHERE database = {} AND is_temporary = 0 AND {filter} ORDER BY name",
            lit(&db)
        );
        let rows = s.rows(&sql).await?;
        return Ok(rows
            .into_iter()
            .map(|r| {
                let mut info = ObjectInfo::new(ObjectRef::new(kind, Some(db.clone()), text(r.first()))).with_detail(text(r.get(1))).with_comment(opt(r.get(2)));
                info.is_system = system;
                info.row_estimate = int(r.get(3));
                info.size_bytes = int(r.get(4));
                info
            })
            .collect());
    }
    match kind {
        ObjectKind::Type => {
            let sql = format!("SELECT name, type, status, comment FROM system.dictionaries WHERE database = {} ORDER BY name", lit(&db));
            let rows = rows_or(s, &sql, &format!("SELECT name, type, status, '' FROM system.dictionaries WHERE database = {} ORDER BY name", lit(&db))).await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    ObjectInfo::new(ObjectRef::new(ObjectKind::Type, Some(db.clone()), text(r.first())))
                        .with_detail(format!("Dictionary {} ({})", text(r.get(1)), text(r.get(2))))
                        .with_comment(opt(r.get(3)))
                })
                .collect())
        }
        ObjectKind::Function => {
            let rows = s.rows("SELECT name, create_query FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name").await?;
            Ok(rows
                .into_iter()
                .map(|r| ObjectInfo::new(ObjectRef::new(ObjectKind::Function, Some(db.clone()), text(r.first()))).with_detail("SQL user defined function"))
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

fn column_detail(type_name: &str, default_kind: &str, default_expr: &str) -> String {
    let mut d = type_name.to_string();
    if !default_kind.is_empty() && !default_expr.is_empty() {
        d.push_str(&format!(" {default_kind} {default_expr}"));
    }
    d
}

pub async fn list_children(s: &mut ClickHouseSession, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
    let db = obj_schema(s, parent);
    let table = parent.name.clone();
    let where_ = format!("database = {} AND table = {}", lit(&db), lit(&table));
    match kind {
        ObjectKind::Column => {
            let rows = s
                .rows(&format!(
                    "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key \
                     FROM system.columns WHERE {where_} ORDER BY position"
                ))
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let mut detail = column_detail(&text(r.get(1)), &text(r.get(2)), &text(r.get(3)));
                    if truthy(r.get(5)) {
                        detail.push_str(" (key)");
                    }
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, text(r.first()))).with_detail(detail).with_comment(opt(r.get(4)))
                })
                .collect())
        }
        ObjectKind::Index => {
            let rows = rows_or(
                s,
                &format!("SELECT name, type_full, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
                &format!("SELECT name, type, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
            )
            .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Index, parent, text(r.first()))).with_detail(format!(
                        "{} TYPE {} GRANULARITY {}",
                        text(r.get(2)),
                        text(r.get(1)),
                        text(r.get(3))
                    ))
                })
                .collect())
        }
        ObjectKind::PrimaryKey => {
            let rows = s.rows(&format!("SELECT primary_key, sorting_key FROM system.tables WHERE database = {} AND name = {}", lit(&db), lit(&table))).await?;
            Ok(rows
                .into_iter()
                .filter_map(|r| {
                    let pk = opt(r.first()).or_else(|| opt(r.get(1)))?;
                    Some(ObjectInfo::new(ObjectRef::child(ObjectKind::PrimaryKey, parent, "PRIMARY KEY")).with_detail(format!("({pk})")))
                })
                .collect())
        }
        ObjectKind::CheckConstraint => {
            let create = create_query(s, &db, &table).await?;
            Ok(parse_constraints(&create)
                .into_iter()
                .map(|c| ObjectInfo::new(ObjectRef::child(ObjectKind::CheckConstraint, parent, c.name.clone().unwrap_or_default())).with_detail(c.expression))
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

async fn create_query(s: &mut ClickHouseSession, db: &str, table: &str) -> DbResult<String> {
    let v = s.scalar(&format!("SELECT create_table_query FROM system.tables WHERE database = {} AND name = {}", lit(db), lit(table))).await?;
    Ok(v.to_text().into_owned())
}

// ------------------------------------------------------------ definitions

const TABLE_COLS: &str = "name, engine, engine_full, partition_key, sorting_key, primary_key, sampling_key, \
                          total_rows, total_bytes, comment, create_table_query, as_select";

const COLUMN_COLS: &str = "table, name, type, default_kind, default_expression, comment, is_in_primary_key, position";

fn column_from_row(r: &Row) -> ColumnDef {
    let full_type = text(r.get(2));
    let (inner, nullable) = strip_nullable(&full_type);
    let default_kind = text(r.get(3)).to_ascii_uppercase();
    let expr = opt(r.get(4));
    let mut c = ColumnDef::new(text(r.get(1)), inner.clone());
    c.kind = crate::types::kind_of(&full_type);
    c.nullable = nullable;
    match default_kind.as_str() {
        "DEFAULT" => c.default = expr,
        "MATERIALIZED" => c.generated = expr,
        "ALIAS" => c.generated = expr.map(|e| format!("ALIAS {e}")),
        "EPHEMERAL" => c.generated = Some(format!("EPHEMERAL {}", expr.unwrap_or_default()).trim().to_string()),
        _ => {}
    }
    c.comment = opt(r.get(5));
    c.ordinal = int(r.get(7)).unwrap_or(0).max(0) as u32;
    c
}

fn index_from_row(r: &Row, name_at: usize) -> IndexDef {
    let name = text(r.get(name_at));
    let ty = text(r.get(name_at + 1));
    let expr = text(r.get(name_at + 2));
    let gran = text(r.get(name_at + 3));
    IndexDef {
        columns: vec![IndexColumn { name: expr.clone(), is_expression: !is_plain_ident(&expr), descending: false }],
        method: Some(ty.clone()).filter(|t| !t.is_empty()),
        definition: Some(format!("INDEX {} {expr} TYPE {ty} GRANULARITY {gran}", ident(&name))),
        name,
        ..Default::default()
    }
}

fn table_from_row(db: &str, r: &Row) -> TableDef {
    let mut t = TableDef::new(Some(db.to_string()), text(r.first()));
    let engine_full = text(r.get(2));
    let (engine, ttl, settings) = split_engine_full(&engine_full);
    let engine = if engine.is_empty() { text(r.get(1)) } else { engine };
    t.options.insert("engine".into(), engine);
    let sorting = text(r.get(4));
    let pk = text(r.get(5));
    if let Some(p) = opt(r.get(3)) {
        t.options.insert("partition_by".into(), p);
    }
    if !sorting.is_empty() {
        t.options.insert("order_by".into(), sorting.clone());
    }
    if !pk.is_empty() && pk != sorting {
        t.options.insert("primary_key".into(), pk.clone());
    }
    if let Some(sm) = opt(r.get(6)) {
        t.options.insert("sample_by".into(), sm);
    }
    if let Some(ttl) = ttl {
        t.options.insert("ttl".into(), ttl);
    }
    if let Some(st) = settings {
        t.options.insert("settings".into(), st);
    }
    let key = if pk.is_empty() { sorting } else { pk };
    let cols = key_list(&key);
    if !cols.is_empty() {
        t.primary_key = Some(KeyDef { name: None, columns: cols });
    }
    t.row_estimate = int(r.get(7));
    t.size_bytes = int(r.get(8));
    t.comment = opt(r.get(9));
    t.checks = parse_constraints(&text(r.get(10)));
    t
}

pub async fn table_definition(s: &mut ClickHouseSession, table: &ObjectRef) -> DbResult<TableDef> {
    let db = obj_schema(s, table);
    let rows = s.rows(&format!("SELECT {TABLE_COLS} FROM system.tables WHERE database = {} AND name = {}", lit(&db), lit(&table.name))).await?;
    let row = rows.first().ok_or_else(|| DbError::query(format!("Table {}.{} does not exist", db, table.name)).with_code("60"))?;
    let mut t = table_from_row(&db, row);
    let where_ = format!("database = {} AND table = {}", lit(&db), lit(&table.name));
    for r in s.rows(&format!("SELECT {COLUMN_COLS} FROM system.columns WHERE {where_} ORDER BY position")).await? {
        t.columns.push(column_from_row(&r));
    }
    let idx = rows_or(
        s,
        &format!("SELECT name, type_full, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
        &format!("SELECT name, type, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
    )
    .await?;
    t.indexes = idx.iter().map(|r| index_from_row(r, 0)).collect();
    Ok(t)
}

pub async fn view_definition(s: &mut ClickHouseSession, view: &ObjectRef) -> DbResult<ViewDef> {
    let db = obj_schema(s, view);
    let rows = s.rows(&format!("SELECT engine, as_select, comment, create_table_query FROM system.tables WHERE database = {} AND name = {}", lit(&db), lit(&view.name))).await?;
    let r = rows.first().ok_or_else(|| DbError::query(format!("View {}.{} does not exist", db, view.name)))?;
    let materialized = text(r.first()) == "MaterializedView";
    let mut v = ViewDef { schema: Some(db.clone()), name: view.name.clone(), materialized, definition: text(r.get(1)), comment: opt(r.get(2)), ..Default::default() };
    if materialized && let Some(to) = mv_target(&text(r.get(3))) {
        v.options.insert("to".into(), to);
    } else if materialized && let Some(engine) = mv_engine(&text(r.get(3))) {
        // Without TO the view stores its rows itself: its ENGINE (with ORDER BY, SETTINGS ...) is part of the DDL.
        v.options.insert("engine".into(), engine);
    }
    let where_ = format!("database = {} AND table = {}", lit(&db), lit(&view.name));
    for r in s.rows(&format!("SELECT {COLUMN_COLS} FROM system.columns WHERE {where_} ORDER BY position")).await? {
        v.columns.push(column_from_row(&r));
    }
    Ok(v)
}

pub async fn routine_definition(s: &mut ClickHouseSession, routine: &ObjectRef) -> DbResult<RoutineDef> {
    let v = s.scalar(&format!("SELECT create_query FROM system.functions WHERE origin = 'SQLUserDefined' AND name = {}", lit(&routine.name))).await?;
    if v.is_null() {
        return Err(DbError::query(format!("Function {} does not exist", routine.name)));
    }
    let def = v.to_text().into_owned();
    // CREATE FUNCTION name AS (a, b) -> expr
    let arguments = def
        .find(" AS ")
        .and_then(|i| {
            let rest = def[i + 4..].trim_start();
            let rest = rest.strip_prefix('(')?;
            rest.find(')').map(|j| rest[..j].to_string())
        })
        .unwrap_or_default();
    Ok(RoutineDef {
        schema: None,
        name: routine.name.clone(),
        kind: Some(ObjectKind::Function),
        arguments,
        returns: None,
        language: Some("SQL".into()),
        definition: def,
        comment: None,
    })
}

pub async fn object_ddl(s: &mut ClickHouseSession, obj: &ObjectRef) -> DbResult<String> {
    let db = obj_schema(s, obj);
    match obj.kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
            let v = s.scalar(&format!("SHOW CREATE TABLE {}", qualified(Some(&db), &obj.name))).await?;
            Ok(v.to_text().into_owned())
        }
        ObjectKind::Type => {
            let v = s.scalar(&format!("SHOW CREATE DICTIONARY {}", qualified(Some(&db), &obj.name))).await?;
            Ok(v.to_text().into_owned())
        }
        ObjectKind::Database | ObjectKind::Schema => {
            let v = s.scalar(&format!("SHOW CREATE DATABASE {}", ident(&obj.name))).await?;
            Ok(v.to_text().into_owned())
        }
        ObjectKind::Function => Ok(routine_definition(s, obj).await?.definition),
        ObjectKind::Column | ObjectKind::Index | ObjectKind::CheckConstraint | ObjectKind::PrimaryKey => {
            let parent = obj.parent_table().ok_or_else(|| DbError::config("Missing owning table"))?;
            let t = table_definition(s, &parent).await?;
            let ddl = crate::ddl::ClickHouseDdl::new();
            use dbiji_core::ddl::DdlGenerator;
            Ok(match obj.kind {
                ObjectKind::Column => t.column(&obj.name).map(|c| ddl.add_column(&t, c).join(";\n")).ok_or_else(|| DbError::query(format!("Column {} not found", obj.name)))?,
                ObjectKind::Index => {
                    t.indexes.iter().find(|i| i.name == obj.name).map(|i| ddl.create_index(&t, i)).ok_or_else(|| DbError::query(format!("Index {} not found", obj.name)))?
                }
                ObjectKind::CheckConstraint => t
                    .checks
                    .iter()
                    .find(|c| c.name.as_deref() == Some(obj.name.as_str()))
                    .map(|c| ddl.add_check(&t, c))
                    .ok_or_else(|| DbError::query(format!("Constraint {} not found", obj.name)))?,
                _ => t.primary_key.as_ref().map(|k| format!("PRIMARY KEY ({})", k.columns.join(", "))).unwrap_or_default(),
            })
        }
        other => Err(DbError::unsupported(format!("DDL of {} objects is not available in ClickHouse", other.label()))),
    }
}

fn human_bytes(n: i64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if v.abs() < 1024.0 || unit == "TiB" {
            return if unit == "B" { format!("{n} B") } else { format!("{v:.2} {unit}") };
        }
        v /= 1024.0;
    }
    format!("{n} B")
}

pub async fn object_properties(s: &mut ClickHouseSession, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let db = obj_schema(s, obj);
    match obj.kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
            let rows = s
                .rows(&format!(
                    "SELECT engine, engine_full, partition_key, sorting_key, primary_key, sampling_key, total_rows, \
                     total_bytes, comment, toString(metadata_modification_time), toString(uuid) \
                     FROM system.tables WHERE database = {} AND name = {}",
                    lit(&db),
                    lit(&obj.name)
                ))
                .await?;
            let Some(r) = rows.first() else { return Ok(Vec::new()) };
            let labels =
                ["Engine", "Engine (full)", "Partition key", "Sorting key", "Primary key", "Sampling key", "Total rows", "Total bytes", "Comment", "Metadata modified", "UUID"];
            let mut props = Vec::new();
            for (i, l) in labels.iter().enumerate() {
                if let Some(v) = opt(r.get(i)) {
                    let v = if *l == "Total bytes" { v.parse::<i64>().map(human_bytes).unwrap_or(v) } else { v };
                    props.push(Property::new(*l, v));
                }
            }
            Ok(props)
        }
        ObjectKind::Database | ObjectKind::Schema => {
            let rows = s.rows(&format!("SELECT engine, data_path, metadata_path, toString(uuid) FROM system.databases WHERE name = {}", lit(&obj.name))).await?;
            let Some(r) = rows.first() else { return Ok(Vec::new()) };
            Ok(["Engine", "Data path", "Metadata path", "UUID"].iter().enumerate().filter_map(|(i, l)| opt(r.get(i)).map(|v| Property::new(*l, v))).collect())
        }
        _ => Ok(Vec::new()),
    }
}

pub async fn object_statistics(s: &mut ClickHouseSession, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    if !matches!(obj.kind, ObjectKind::Table | ObjectKind::MaterializedView) {
        return Ok(Vec::new());
    }
    let db = obj_schema(s, obj);
    let rows = s
        .rows(&format!(
            "SELECT count(), sum(rows), sum(bytes_on_disk), sum(data_compressed_bytes), sum(data_uncompressed_bytes), \
             toString(max(modification_time)) \
             FROM system.parts WHERE database = {} AND table = {} AND active",
            lit(&db),
            lit(&obj.name)
        ))
        .await?;
    let Some(r) = rows.first() else { return Ok(Vec::new()) };
    let mut props = vec![Property::new("Active parts", text(r.first())), Property::new("Rows", text(r.get(1)))];
    for (i, label) in [(2, "Bytes on disk"), (3, "Compressed bytes"), (4, "Uncompressed bytes")] {
        if let Some(n) = int(r.get(i)) {
            props.push(Property::new(label, human_bytes(n)));
        }
    }
    if let (Some(c), Some(u)) = (int(r.get(3)), int(r.get(4)))
        && c > 0
    {
        props.push(Property::new("Compression ratio", format!("{:.2}", u as f64 / c as f64)));
    }
    if let Some(m) = opt(r.get(5)) {
        props.push(Property::new("Last modification", m));
    }
    Ok(props)
}

pub async fn object_dependencies(s: &mut ClickHouseSession, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
    if !obj.kind.has_data() {
        return Ok(Vec::new());
    }
    let db = obj_schema(s, obj);
    let rows = s
        .rows(&format!(
            "SELECT dependencies_database, dependencies_table, create_table_query, engine \
             FROM system.tables WHERE database = {} AND name = {}",
            lit(&db),
            lit(&obj.name)
        ))
        .await?;
    let Some(r) = rows.first() else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    if let (Some(Value::Array(dbs)), Some(Value::Array(tables))) = (r.first(), r.get(1)) {
        for (d, t) in dbs.iter().zip(tables.iter()) {
            out.push(Dependency {
                direction: DependencyDirection::UsedBy,
                target: ObjectRef::new(ObjectKind::View, Some(d.to_text().into_owned()), t.to_text().into_owned()),
                kind: Some("view".into()),
            });
        }
    }
    if text(r.get(3)) == "MaterializedView"
        && let Some(target) = mv_target(&text(r.get(2)))
    {
        let (d, n) = match target.split_once('.') {
            Some((d, n)) => (Some(d.trim_matches('`').to_string()), n.trim_matches('`').to_string()),
            None => (Some(db.clone()), target.trim_matches('`').to_string()),
        };
        out.push(Dependency { direction: DependencyDirection::DependsOn, target: ObjectRef::new(ObjectKind::Table, d, n), kind: Some("materialized view target".into()) });
    }
    Ok(out)
}

// ------------------------------------------------------------ search / snapshot

pub async fn search_metadata(s: &mut ClickHouseSession, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
    let pat = lit(&like_pattern(&search.pattern));
    let op = if search.case_sensitive { "LIKE" } else { "ILIKE" };
    let db_filter = if search.schemas.is_empty() {
        "database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema')".to_string()
    } else {
        format!("database IN ({})", search.schemas.iter().map(|s| lit(s)).collect::<Vec<_>>().join(", "))
    };
    let limit = search.limit.max(1);
    let mut out: Vec<ObjectInfo> = Vec::new();
    let table_kinds: Vec<ObjectKind> = search.kinds.iter().copied().filter(|k| matches!(k, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView)).collect();
    if !table_kinds.is_empty() {
        let comment_cond = if search.search_comments { format!(" OR comment {op} {pat}") } else { String::new() };
        let rows = s
            .rows(&format!(
                "SELECT database, name, engine, comment, total_rows FROM system.tables \
                 WHERE {db_filter} AND is_temporary = 0 AND name NOT LIKE '.inner%' AND (name {op} {pat}{comment_cond}) \
                 ORDER BY database, name LIMIT {limit}"
            ))
            .await?;
        for r in rows {
            let engine = text(r.get(2));
            let kind = match engine.as_str() {
                "View" | "LiveView" | "WindowView" => ObjectKind::View,
                "MaterializedView" => ObjectKind::MaterializedView,
                "Dictionary" => ObjectKind::Type,
                _ => ObjectKind::Table,
            };
            if !table_kinds.contains(&kind) {
                continue;
            }
            let mut info = ObjectInfo::new(ObjectRef::new(kind, Some(text(r.first())), text(r.get(1)))).with_detail(engine).with_comment(opt(r.get(3)));
            info.row_estimate = int(r.get(4));
            out.push(info);
        }
    }
    if search.kinds.contains(&ObjectKind::Column) && out.len() < limit {
        let comment_cond = if search.search_comments { format!(" OR comment {op} {pat}") } else { String::new() };
        let rows = s
            .rows(&format!(
                "SELECT database, table, name, type, comment FROM system.columns \
                 WHERE {db_filter} AND (name {op} {pat}{comment_cond}) ORDER BY database, table, position LIMIT {}",
                limit - out.len()
            ))
            .await?;
        for r in rows {
            let parent = ObjectRef::table(Some(text(r.first())), text(r.get(1)));
            out.push(ObjectInfo::new(ObjectRef::child(ObjectKind::Column, &parent, text(r.get(2)))).with_detail(text(r.get(3))).with_comment(opt(r.get(4))));
        }
    }
    if search.kinds.contains(&ObjectKind::Function) && out.len() < limit {
        let rows = s.rows(&format!("SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' AND name {op} {pat} ORDER BY name LIMIT {}", limit - out.len())).await?;
        for r in rows {
            out.push(ObjectInfo::new(ObjectRef::new(ObjectKind::Function, None, text(r.first()))));
        }
    }
    if search.kinds.contains(&ObjectKind::Type) && out.len() < limit {
        let rows = s
            .rows(&format!(
                "SELECT database, name, type FROM system.dictionaries WHERE {db_filter} AND name {op} {pat} \
                 ORDER BY database, name LIMIT {}",
                limit - out.len()
            ))
            .await?;
        for r in rows {
            out.push(ObjectInfo::new(ObjectRef::new(ObjectKind::Type, Some(text(r.first())), text(r.get(1)))).with_detail(format!("Dictionary {}", text(r.get(2)))));
        }
    }
    out.truncate(limit);
    Ok(out)
}

pub async fn schema_snapshot(s: &mut ClickHouseSession, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
    let db = schema_of(s, schema);
    let mut snap = SchemaSnapshot { kind: Some(DbKind::ClickHouse), database: None, schema: Some(db.clone()), ..Default::default() };
    let rows = s.rows(&format!("SELECT {TABLE_COLS} FROM system.tables WHERE database = {} AND is_temporary = 0 AND name NOT LIKE '.inner%' ORDER BY name", lit(&db))).await?;
    let mut tables: BTreeMap<String, TableDef> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewDef> = BTreeMap::new();
    for r in &rows {
        let engine = text(r.get(1));
        let name = text(r.first());
        match engine.as_str() {
            "View" | "LiveView" | "WindowView" | "MaterializedView" => {
                if opts.views {
                    let mut v = ViewDef {
                        schema: Some(db.clone()),
                        name: name.clone(),
                        materialized: engine == "MaterializedView",
                        comment: if opts.comments { opt(r.get(9)) } else { None },
                        ..Default::default()
                    };
                    if opts.view_definitions {
                        v.definition = text(r.get(11));
                    }
                    if v.materialized
                        && let Some(to) = mv_target(&text(r.get(10)))
                    {
                        v.options.insert("to".into(), to);
                    }
                    views.insert(name, v);
                }
            }
            "Dictionary" => {}
            _ => {
                let mut t = table_from_row(&db, r);
                if !opts.comments {
                    t.comment = None;
                }
                if !opts.constraints {
                    t.checks.clear();
                }
                if !opts.statistics {
                    t.row_estimate = None;
                    t.size_bytes = None;
                }
                tables.insert(name, t);
            }
        }
    }
    if opts.columns {
        let rows = s.rows(&format!("SELECT {COLUMN_COLS} FROM system.columns WHERE database = {} ORDER BY table, position", lit(&db))).await?;
        for r in rows {
            let table = text(r.first());
            let mut c = column_from_row(&r);
            if !opts.comments {
                c.comment = None;
            }
            if let Some(t) = tables.get_mut(&table) {
                t.columns.push(c);
            } else if let Some(v) = views.get_mut(&table) {
                v.columns.push(c);
            }
        }
    }
    if opts.indexes {
        let where_ = format!("database = {}", lit(&db));
        let rows = rows_or(
            s,
            &format!("SELECT table, name, type_full, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
            &format!("SELECT table, name, type, expr, granularity FROM system.data_skipping_indices WHERE {where_}"),
        )
        .await?;
        for r in rows {
            if let Some(t) = tables.get_mut(&text(r.first())) {
                t.indexes.push(index_from_row(&r, 1));
            }
        }
    }
    if opts.routines {
        let rows = s.rows("SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name").await.unwrap_or_default();
        for r in rows {
            let name = text(r.first());
            if let Ok(rd) = routine_definition(s, &ObjectRef::new(ObjectKind::Function, None, name)).await {
                snap.routines.push(rd);
            }
        }
    }
    if opts.types {
        let rows = s.rows(&format!("SELECT name, type FROM system.dictionaries WHERE database = {} ORDER BY name", lit(&db))).await.unwrap_or_default();
        for r in rows {
            let name = text(r.first());
            let def = s.scalar(&format!("SHOW CREATE DICTIONARY {}", qualified(Some(&db), &name))).await.map(|v| v.to_text().into_owned()).unwrap_or_default();
            snap.types.push(TypeDef { schema: Some(db.clone()), name, category: "dictionary".into(), elements: Vec::new(), definition: def, comment: None });
        }
    }
    snap.tables = tables.into_values().collect();
    snap.views = views.into_values().collect();
    Ok(snap)
}

// ------------------------------------------------------------ metrics

const GAUGES: &[(&str, &str, &str, &str)] = &[
    ("Query", "Queries", "queries.running", "Running queries"),
    ("Merge", "Background", "merges.running", "Running merges"),
    ("PartMutation", "Background", "mutations.running", "Running mutations"),
    ("TCPConnection", "Connections", "connections.tcp", "TCP connections"),
    ("HTTPConnection", "Connections", "connections.http", "HTTP connections"),
    ("MySQLConnection", "Connections", "connections.mysql", "MySQL protocol connections"),
    ("PostgreSQLConnection", "Connections", "connections.postgresql", "PostgreSQL protocol connections"),
    ("InterserverConnection", "Connections", "connections.interserver", "Interserver connections"),
    ("MemoryTracking", "Memory", "memory.tracked", "Tracked memory"),
    ("DelayedInserts", "Queries", "inserts.delayed", "Delayed inserts"),
    ("ReadonlyReplica", "Replication", "replicas.readonly", "Read-only replicas"),
];

const COUNTERS: &[(&str, &str, &str, &str, &str)] = &[
    ("Query", "Queries", "queries.total", "Queries", ""),
    ("SelectQuery", "Queries", "queries.select", "SELECT queries", ""),
    ("InsertQuery", "Queries", "queries.insert", "INSERT queries", ""),
    ("FailedQuery", "Queries", "queries.failed", "Failed queries", ""),
    ("InsertedRows", "Rows", "rows.inserted", "Inserted rows", "rows"),
    ("SelectedRows", "Rows", "rows.selected", "Selected rows", "rows"),
    ("InsertedBytes", "I/O", "bytes.inserted", "Inserted bytes", "bytes"),
    ("SelectedBytes", "I/O", "bytes.selected", "Selected bytes", "bytes"),
    ("ReadCompressedBytes", "I/O", "bytes.read_compressed", "Read compressed bytes", "bytes"),
    ("NetworkReceiveBytes", "Network", "network.received", "Network received", "bytes"),
    ("NetworkSendBytes", "Network", "network.sent", "Network sent", "bytes"),
    ("MergedRows", "Background", "rows.merged", "Merged rows", "rows"),
];

const ASYNC_GAUGES: &[(&str, &str, &str, &str, &str)] = &[
    ("Uptime", "Server", "server.uptime", "Uptime", "s"),
    ("MemoryResident", "Memory", "memory.resident", "Resident memory", "bytes"),
    ("LoadAverage1", "Server", "server.load1", "Load average (1 min)", ""),
    ("NumberOfDatabases", "Server", "server.databases", "Databases", ""),
    ("NumberOfTables", "Server", "server.tables", "Tables", ""),
    ("TotalPartsOfMergeTreeTables", "Storage", "storage.parts", "MergeTree parts", ""),
    ("TotalRowsOfMergeTreeTables", "Storage", "storage.rows", "MergeTree rows", "rows"),
    ("TotalBytesOfMergeTreeTables", "Storage", "storage.bytes", "MergeTree bytes", "bytes"),
    ("MaxPartCountForPartition", "Storage", "storage.max_parts_per_partition", "Max parts per partition", ""),
];

fn names_list<T>(items: &[T], name: impl Fn(&T) -> &str) -> String {
    items.iter().map(|i| lit(name(i))).collect::<Vec<_>>().join(", ")
}

pub async fn server_metrics(s: &mut ClickHouseSession) -> DbResult<Vec<Metric>> {
    let mut out = Vec::new();
    let rows = s.rows(&format!("SELECT metric, value FROM system.metrics WHERE metric IN ({})", names_list(GAUGES, |g| g.0))).await?;
    for (name, group, key, label) in GAUGES {
        if let Some(v) = rows.iter().find(|r| text(r.first()) == *name).and_then(|r| r.get(1)).and_then(Value::as_f64) {
            let unit = if *name == "MemoryTracking" { "bytes" } else { "" };
            out.push(Metric::gauge(group, key, label, v, unit));
        }
    }
    let rows = s.rows(&format!("SELECT event, value FROM system.events WHERE event IN ({})", names_list(COUNTERS, |c| c.0))).await?;
    for (name, group, key, label, unit) in COUNTERS {
        let v = rows.iter().find(|r| text(r.first()) == *name).and_then(|r| r.get(1)).and_then(Value::as_f64);
        // events that never happened are absent: report 0
        out.push(Metric::counter(group, key, label, v.unwrap_or(0.0), unit));
    }
    let rows = s.rows(&format!("SELECT metric, value FROM system.asynchronous_metrics WHERE metric IN ({})", names_list(ASYNC_GAUGES, |c| c.0))).await?;
    for (name, group, key, label, unit) in ASYNC_GAUGES {
        if let Some(v) = rows.iter().find(|r| text(r.first()) == *name).and_then(|r| r.get(1)).and_then(Value::as_f64) {
            out.push(Metric::gauge(group, key, label, v, unit));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_constraints() {
        assert_eq!(split_top_level("a, f(b, c), 'x,y', `q,r`"), vec!["a", "f(b, c)", "'x,y'", "`q,r`"]);
        let create = "CREATE TABLE db.t (`a` UInt8 DEFAULT 1, `b` String DEFAULT 'x,(y', INDEX i a TYPE minmax GRANULARITY 1, CONSTRAINT c1 CHECK a > 0, CONSTRAINT `c 2` CHECK length(b) < 10, CONSTRAINT c3 ASSUME a < 5) ENGINE = MergeTree ORDER BY a";
        let checks = parse_constraints(create);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0], CheckDef { name: Some("c1".into()), expression: "a > 0".into() });
        assert_eq!(checks[1].name.as_deref(), Some("c 2"));
        assert_eq!(checks[1].expression, "length(b) < 10");
        assert!(parse_constraints("CREATE VIEW v AS SELECT 1").is_empty());
    }

    #[test]
    fn engine_full_split() {
        let (e, ttl, st) = split_engine_full("ReplacingMergeTree(ver) PARTITION BY toYYYYMM(ts) ORDER BY (id, ts) TTL ts + toIntervalDay(30) SETTINGS index_granularity = 8192");
        assert_eq!(e, "ReplacingMergeTree(ver)");
        assert_eq!(ttl.as_deref(), Some("ts + toIntervalDay(30)"));
        assert_eq!(st.as_deref(), Some("index_granularity = 8192"));
        let (e, ttl, st) = split_engine_full("Memory");
        assert_eq!((e.as_str(), ttl, st), ("Memory", None, None));
        let (e, _, _) = split_engine_full("Distributed('cluster', 'db', 'tbl ORDER BY x', rand())");
        assert_eq!(e, "Distributed('cluster', 'db', 'tbl ORDER BY x', rand())");
    }

    #[test]
    fn helpers() {
        assert_eq!(like_pattern("user"), "%user%");
        assert_eq!(like_pattern("us*r?"), "us%r_");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(key_list("(id, toDate(ts))"), vec!["id", "toDate(ts)"]);
        assert_eq!(key_list("id"), vec!["id"]);
        assert!(key_list("").is_empty());
        assert_eq!(mv_target("CREATE MATERIALIZED VIEW db.mv TO db.target (`x` UInt8) AS SELECT 1"), Some("db.target".into()));
        assert_eq!(mv_target("CREATE MATERIALIZED VIEW db.mv ENGINE = Memory AS SELECT 1 TO x"), None);
        assert_eq!(
            mv_engine("CREATE MATERIALIZED VIEW db.mv (`a` UInt8) ENGINE = MergeTree ORDER BY a SETTINGS index_granularity = 8192 AS SELECT 1 AS a"),
            Some("MergeTree ORDER BY a SETTINGS index_granularity = 8192".into())
        );
        assert!(is_system_db("system"));
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KiB");
    }

    #[test]
    fn rows_to_definitions() {
        let r: Row = vec![
            Value::text("t"),
            Value::text("ReplacingMergeTree"),
            Value::text("ReplacingMergeTree(v) ORDER BY (a, b) SETTINGS index_granularity = 8192"),
            Value::text(""),
            Value::text("a, b"),
            Value::text("a"),
            Value::text(""),
            Value::Int(5),
            Value::Int(100),
            Value::text("c"),
            Value::text("CREATE TABLE db.t (`a` UInt8, CONSTRAINT k CHECK a > 1) ENGINE = ReplacingMergeTree(v) ORDER BY (a, b)"),
            Value::text(""),
        ];
        let t = table_from_row("db", &r);
        assert_eq!(t.options.get("engine").map(String::as_str), Some("ReplacingMergeTree(v)"));
        assert_eq!(t.options.get("order_by").map(String::as_str), Some("a, b"));
        assert_eq!(t.options.get("primary_key").map(String::as_str), Some("a"));
        assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["a"]);
        assert_eq!(t.checks.len(), 1);
        assert_eq!(t.row_estimate, Some(5));
        let c = column_from_row(&vec![
            Value::text("t"),
            Value::text("x"),
            Value::text("Nullable(String)"),
            Value::text("ALIAS"),
            Value::text("upper(y)"),
            Value::text(""),
            Value::Bool(false),
            Value::Int(2),
        ]);
        assert!(c.nullable);
        assert_eq!(c.data_type, "String");
        assert_eq!(c.generated.as_deref(), Some("ALIAS upper(y)"));
        assert_eq!(c.ordinal, 2);
    }
}
