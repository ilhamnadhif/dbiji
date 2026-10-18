//! Catalog access through DuckDB's `duckdb_*()` table functions.
//!
//! DuckDB has two namespace levels: databases (the main database file plus
//! `ATTACH`ed ones) and schemas inside them. [`ObjectRef::database`] is
//! `None` for objects of the current database and the database name for
//! objects of other attached databases; `SchemaRef::database` selects the
//! database the same way.

use crate::convert::kind_of_name;
use crate::dialect::{qualified, quote_ident, string_literal};
use crate::lexer::{self, TokKind, Token};
use crate::worker::Worker;
use dbiji_core::config::wildcard_match;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{DbError, DbKind, DbResult};
use duckdb::ToSql;
use duckdb::types::Value as DValue;
use std::collections::{HashMap, HashSet};

// ------------------------------------------------------------------ helpers

fn text(r: &duckdb::Row<'_>, i: usize) -> String {
    r.get::<_, Option<String>>(i)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn opt_text(r: &duckdb::Row<'_>, i: usize) -> Option<String> {
    r.get::<_, Option<String>>(i)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

fn flag(r: &duckdb::Row<'_>, i: usize) -> bool {
    r.get::<_, Option<bool>>(i).ok().flatten().unwrap_or(false)
}

fn int(r: &duckdb::Row<'_>, i: usize) -> Option<i64> {
    r.get::<_, Option<i64>>(i).ok().flatten()
}

/// A `VARCHAR[]` column.
fn list(r: &duckdb::Row<'_>, i: usize) -> Vec<String> {
    match r.get::<_, DValue>(i) {
        Ok(DValue::List(items)) | Ok(DValue::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                DValue::Text(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// Current database and schema of the session.
struct Ctx {
    db: String,
    schema: String,
}

fn context(w: &Worker) -> DbResult<Ctx> {
    let rows = w.query_map("SELECT current_database(), current_schema()", &[], |r| {
        (text(r, 0), text(r, 1))
    })?;
    let (db, schema) = rows.into_iter().next().unwrap_or_default();
    Ok(Ctx { db, schema })
}

impl Ctx {
    /// Database name for an optional reference.
    fn database(&self, database: Option<&str>) -> String {
        database
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.db)
            .to_string()
    }

    /// Schema name for an optional reference (`main` in other databases).
    fn schema(&self, database: Option<&str>, schema: Option<&str>) -> String {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None if eq_ci(&self.database(database), &self.db) => self.schema.clone(),
            None => "main".to_string(),
        }
    }

    /// `ObjectRef::database` for objects of `db`.
    fn db_ref(&self, db: &str) -> Option<String> {
        (!eq_ci(db, &self.db)).then(|| db.to_string())
    }

    fn obj(
        &self,
        kind: ObjectKind,
        db: &str,
        schema: &str,
        table: Option<&str>,
        name: &str,
    ) -> ObjectRef {
        ObjectRef {
            kind,
            database: self.db_ref(db),
            schema: Some(schema.to_string()),
            table: table.map(str::to_string),
            name: name.to_string(),
            signature: None,
        }
    }
}

/// Namespace of an object reference: (database, schema).
fn namespace(ctx: &Ctx, obj: &ObjectRef) -> (String, String) {
    (
        ctx.database(obj.database.as_deref()),
        ctx.schema(obj.database.as_deref(), obj.schema.as_deref()),
    )
}

/// `WHERE` clause for database / schema / name filters; `name_col` is
/// compared when `name` is given.
// `&String` because a `&str` cannot be turned into a `&dyn ToSql`
#[allow(clippy::ptr_arg)]
fn filter_sql<'a>(
    base: &str,
    db: &'a String,
    schema: Option<&'a String>,
    name_col: &str,
    name: Option<&'a String>,
    params: &mut Vec<&'a dyn ToSql>,
) -> String {
    let mut sql = format!("{base} WHERE lower(database_name) = lower(?)");
    params.push(db);
    if let Some(s) = schema {
        sql.push_str(" AND lower(schema_name) = lower(?)");
        params.push(s);
    }
    if let Some(n) = name {
        sql.push_str(&format!(" AND lower({name_col}) = lower(?)"));
        params.push(n);
    }
    sql
}

fn not_found(kind: &str, obj: &ObjectRef) -> DbError {
    DbError::query(format!("{kind} {} does not exist", obj.display_path())).with_code("NOT_FOUND")
}

const SYSTEM_SCHEMAS: &[&str] = &["information_schema", "pg_catalog"];

// ---------------------------------------------------------------- databases

pub(crate) fn list_databases(w: &Worker) -> DbResult<Vec<DatabaseInfo>> {
    let ctx = context(w)?;
    let rows = w.query_map(
        "SELECT database_name, internal, comment, path FROM duckdb_databases() ORDER BY database_oid",
        &[],
        |r| (text(r, 0), flag(r, 1), opt_text(r, 2)),
    )?;
    Ok(rows
        .into_iter()
        .filter(|(_, internal, _)| w.show_system_objects || !internal)
        .map(|(name, internal, comment)| DatabaseInfo {
            is_current: eq_ci(&name, &ctx.db),
            is_system: internal,
            comment,
            name,
            owner: None,
            encoding: None,
            size_bytes: None,
        })
        .collect())
}

pub(crate) fn list_schemas(w: &Worker) -> DbResult<Vec<SchemaInfo>> {
    let ctx = context(w)?;
    let rows = w.query_map(
        "SELECT schema_name, comment FROM duckdb_schemas() WHERE lower(database_name) = lower(current_database()) \
         ORDER BY schema_name",
        &[],
        |r| (text(r, 0), opt_text(r, 1)),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, comment)| {
            let is_system = SYSTEM_SCHEMAS.iter().any(|s| eq_ci(s, &name));
            (w.show_system_objects || !is_system).then(|| SchemaInfo {
                is_current: eq_ci(&name, &ctx.schema),
                is_system,
                owner: None,
                comment,
                name,
            })
        })
        .collect())
}

// ------------------------------------------------------------------ loaders

struct TableRow {
    schema: String,
    name: String,
    comment: Option<String>,
    temporary: bool,
    estimated: Option<i64>,
    sql: Option<String>,
}

fn load_tables(
    w: &Worker,
    db: &String,
    schema: Option<&String>,
    table: Option<&String>,
) -> DbResult<Vec<TableRow>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT schema_name, table_name, comment, temporary, estimated_size, sql FROM duckdb_tables()",
        db,
        schema,
        "table_name",
        table,
        &mut p,
    );
    if !w.show_system_objects {
        sql.push_str(" AND NOT internal");
    }
    sql.push_str(" ORDER BY schema_name, table_name");
    w.query_map(&sql, &p, |r| TableRow {
        schema: text(r, 0),
        name: text(r, 1),
        comment: opt_text(r, 2),
        temporary: flag(r, 3),
        estimated: int(r, 4),
        sql: opt_text(r, 5),
    })
}

struct ColumnRow {
    schema: String,
    table: String,
    name: String,
    index: i64,
    data_type: String,
    nullable: bool,
    default: Option<String>,
    comment: Option<String>,
}

fn load_columns(
    w: &Worker,
    db: &String,
    schema: Option<&String>,
    table: Option<&String>,
) -> DbResult<Vec<ColumnRow>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT schema_name, table_name, column_name, column_index, data_type, is_nullable, column_default, comment \
         FROM duckdb_columns()",
        db,
        schema,
        "table_name",
        table,
        &mut p,
    );
    sql.push_str(" ORDER BY schema_name, table_name, column_index");
    w.query_map(&sql, &p, |r| ColumnRow {
        schema: text(r, 0),
        table: text(r, 1),
        name: text(r, 2),
        index: int(r, 3).unwrap_or(0),
        data_type: text(r, 4),
        nullable: r.get::<_, Option<bool>>(5).ok().flatten().unwrap_or(true),
        default: opt_text(r, 6),
        comment: opt_text(r, 7),
    })
}

struct ConstraintRow {
    schema: String,
    table: String,
    kind: String,
    text: String,
    expression: Option<String>,
    columns: Vec<String>,
    name: String,
    ref_table: Option<String>,
    ref_columns: Vec<String>,
}

fn load_constraints(
    w: &Worker,
    db: &String,
    schema: Option<&String>,
    table: Option<&String>,
) -> DbResult<Vec<ConstraintRow>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT schema_name, table_name, constraint_type, constraint_text, expression, constraint_column_names, \
         constraint_name, referenced_table, referenced_column_names FROM duckdb_constraints()",
        db,
        schema,
        "table_name",
        table,
        &mut p,
    );
    sql.push_str(
        " AND constraint_type <> 'NOT NULL' ORDER BY schema_name, table_name, constraint_index",
    );
    w.query_map(&sql, &p, |r| ConstraintRow {
        schema: text(r, 0),
        table: text(r, 1),
        kind: text(r, 2),
        text: text(r, 3),
        expression: opt_text(r, 4),
        columns: list(r, 5),
        name: text(r, 6),
        ref_table: opt_text(r, 7),
        ref_columns: list(r, 8),
    })
}

struct IndexRow {
    schema: String,
    table: String,
    name: String,
    unique: bool,
    primary: bool,
    comment: Option<String>,
    sql: Option<String>,
}

fn load_indexes(
    w: &Worker,
    db: &String,
    schema: Option<&String>,
    table: Option<&String>,
) -> DbResult<Vec<IndexRow>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT schema_name, table_name, index_name, is_unique, is_primary, comment, sql FROM duckdb_indexes()",
        db,
        schema,
        "table_name",
        table,
        &mut p,
    );
    sql.push_str(" ORDER BY schema_name, table_name, index_name");
    w.query_map(&sql, &p, |r| IndexRow {
        schema: text(r, 0),
        table: text(r, 1),
        name: text(r, 2),
        unique: flag(r, 3),
        primary: flag(r, 4),
        comment: opt_text(r, 5),
        sql: opt_text(r, 6),
    })
}

/// Lower-cased (schema, table) names of all tables of a database.
fn table_names(w: &Worker, db: &str) -> DbResult<HashSet<(String, String)>> {
    w.query_map(
        "SELECT schema_name, table_name FROM duckdb_tables() WHERE lower(database_name) = lower(?)",
        &[&db],
        |r| (lower(&text(r, 0)), lower(&text(r, 1))),
    )
    .map(|v| v.into_iter().collect())
}

// ------------------------------------------------------------- SQL parsing

/// Tokens of the parenthesised list following the first top-level `(`
/// at or after token index `from`, split at top-level commas.
fn paren_items<'a>(toks: &'a [Token<'a>], from: usize) -> Vec<&'a [Token<'a>]> {
    let Some(open) = toks
        .iter()
        .skip(from)
        .position(|t| t.kind == TokKind::LParen)
        .map(|p| p + from)
    else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = open + 1;
    for (i, t) in toks.iter().enumerate().skip(open + 1) {
        match t.kind {
            TokKind::LParen | TokKind::LBracket => depth += 1,
            TokKind::RParen | TokKind::RBracket if depth > 0 => depth -= 1,
            TokKind::RParen => {
                if i > start {
                    items.push(&toks[start..i]);
                }
                return items;
            }
            TokKind::Comma if depth == 0 => {
                items.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items
}

fn span_text<'a>(sql: &'a str, toks: &[Token<'_>]) -> &'a str {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => sql.get(a.start..b.end()).unwrap_or(""),
        _ => "",
    }
}

/// Generated columns of a `CREATE TABLE` statement.
fn generated_columns(sql: &str) -> HashSet<String> {
    let toks = lexer::significant(sql);
    let mut out = HashSet::new();
    for item in paren_items(&toks, 0) {
        let Some(name) = item.first().and_then(|t| t.ident()) else {
            continue;
        };
        let generated = item.iter().skip(1).any(|t| t.is_word("GENERATED"))
            || item
                .windows(2)
                .skip(1)
                .any(|w| w[0].is_word("AS") && w[1].kind == TokKind::LParen);
        if generated {
            out.insert(lower(&name));
        }
    }
    out
}

/// Columns of a `CREATE INDEX ... ON t (...)` statement.
fn index_columns(sql: &str) -> Vec<IndexColumn> {
    let toks = lexer::significant(sql);
    let Some(on) = toks.iter().position(|t| t.is_word("ON")) else {
        return Vec::new();
    };
    paren_items(&toks, on)
        .into_iter()
        .map(|item| {
            let mut part = item;
            let mut descending = false;
            while let Some(last) = part.last() {
                if last.is_any_word(&["ASC", "DESC", "FIRST", "LAST", "NULLS"]) {
                    descending |= last.is_word("DESC");
                    part = &part[..part.len() - 1];
                } else {
                    break;
                }
            }
            match part {
                [t] if t.ident().is_some() => IndexColumn {
                    name: t.ident().unwrap_or_default(),
                    is_expression: false,
                    descending,
                },
                _ => {
                    let mut e = span_text(sql, part).trim().to_string();
                    if e.starts_with('(') && e.ends_with(')') {
                        e = e[1..e.len() - 1].trim().to_string();
                    }
                    IndexColumn {
                        name: e,
                        is_expression: true,
                        descending,
                    }
                }
            }
        })
        .collect()
}

/// The `SELECT` of a `CREATE VIEW ... AS SELECT` statement.
fn view_query(sql: &str) -> String {
    let toks = lexer::significant(sql);
    let mut depth = 0i32;
    for t in &toks {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => depth -= 1,
            TokKind::Word if depth == 0 && t.is_word("AS") => {
                let body = sql[t.end()..].trim();
                return body
                    .strip_suffix(';')
                    .unwrap_or(body)
                    .trim_end()
                    .to_string();
            }
            _ => {}
        }
    }
    sql.trim().to_string()
}

/// Schema of the referenced table in a FOREIGN KEY constraint text
/// (`... REFERENCES s.t(c)`); DuckDB omits `main`.
fn fk_ref_schema(text: &str, ref_table: &str) -> Option<String> {
    let rest = &text[text.find("REFERENCES ")? + "REFERENCES ".len()..];
    let idx = rest.to_lowercase().find(&ref_table.to_lowercase())?;
    let prefix = rest.get(..idx)?;
    prefix
        .strip_suffix('.')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Sequence name inside `nextval('name')`.
fn nextval_sequence(default: &str) -> Option<String> {
    let d = default.trim();
    let inner = d.strip_prefix("nextval(")?.strip_suffix(')')?.trim();
    let inner = inner.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

// ------------------------------------------------------------ definitions

fn ensure_strip_schema(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn build_column(
    c: &ColumnRow,
    table: &str,
    generated: &HashSet<String>,
    comments: bool,
) -> ColumnDef {
    let mut col = ColumnDef::new(c.name.clone(), c.data_type.clone());
    col.kind = kind_of_name(&c.data_type);
    col.nullable = c.nullable;
    col.ordinal = u32::try_from(c.index).unwrap_or(0);
    if comments {
        col.comment = c.comment.clone();
    }
    if generated.contains(&lower(&c.name)) {
        col.generated = c.default.clone();
    } else if let Some(d) = &c.default {
        match nextval_sequence(d) {
            Some(seq) => {
                col.auto_increment = true;
                // the DDL generator recreates sequences named <table>_<column>_seq
                let conventional = format!("{table}_{}_seq", c.name);
                if !eq_ci(ensure_strip_schema(&seq), &conventional) {
                    col.default = Some(d.clone());
                }
            }
            None => col.default = Some(d.clone()),
        }
    }
    col
}

fn build_fk(
    c: &ConstraintRow,
    owner_schema: &str,
    tables: &HashSet<(String, String)>,
) -> ForeignKeyDef {
    let ref_table = c.ref_table.clone().unwrap_or_default();
    let ref_schema = fk_ref_schema(&c.text, &ref_table).unwrap_or_else(|| {
        if tables.contains(&(lower(owner_schema), lower(&ref_table))) {
            owner_schema.to_string()
        } else {
            "main".to_string()
        }
    });
    ForeignKeyDef {
        name: None,
        schema: Some(c.schema.clone()),
        table: c.table.clone(),
        columns: c.columns.clone(),
        ref_schema: Some(ref_schema),
        ref_table,
        ref_columns: c.ref_columns.clone(),
        on_update: FkAction::NoAction,
        on_delete: FkAction::NoAction,
        deferrable: false,
    }
}

fn build_index(i: &IndexRow) -> IndexDef {
    IndexDef {
        name: i.name.clone(),
        columns: i.sql.as_deref().map(index_columns).unwrap_or_default(),
        unique: i.unique,
        primary: i.primary,
        method: Some("art".into()),
        predicate: None,
        include: Vec::new(),
        definition: i
            .sql
            .as_deref()
            .map(|s| s.trim().trim_end_matches(';').to_string()),
        comment: i.comment.clone(),
    }
}

/// Table definitions of one schema (optionally one table).
fn table_defs(
    w: &Worker,
    db: &String,
    schema: &String,
    table: Option<&String>,
    opts: &SnapshotOptions,
) -> DbResult<Vec<TableDef>> {
    let tables = load_tables(w, db, Some(schema), table)?;
    if tables.is_empty() {
        return Ok(Vec::new());
    }
    let mut defs: Vec<TableDef> = tables
        .iter()
        .map(|t| {
            let mut d = TableDef::new(Some(t.schema.clone()), t.name.clone());
            d.table_kind = if t.temporary {
                TableKind::Temporary
            } else {
                TableKind::Table
            };
            if opts.comments {
                d.comment = t.comment.clone();
            }
            d.row_estimate = t.estimated;
            d
        })
        .collect();
    let index_of: HashMap<String, usize> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| (lower(&d.name), i))
        .collect();
    if opts.columns {
        let generated: HashMap<String, HashSet<String>> = tables
            .iter()
            .map(|t| {
                (
                    lower(&t.name),
                    t.sql.as_deref().map(generated_columns).unwrap_or_default(),
                )
            })
            .collect();
        let empty = HashSet::new();
        for c in load_columns(w, db, Some(schema), table)? {
            if !eq_ci(&c.schema, schema) {
                continue;
            }
            if let Some(&i) = index_of.get(&lower(&c.table)) {
                let g = generated.get(&lower(&c.table)).unwrap_or(&empty);
                let col = build_column(&c, &defs[i].name, g, opts.comments);
                defs[i].columns.push(col);
            }
        }
    }
    if opts.constraints {
        let names = table_names(w, db)?;
        for c in load_constraints(w, db, Some(schema), table)? {
            let Some(&i) = index_of.get(&lower(&c.table)) else {
                continue;
            };
            match c.kind.as_str() {
                "PRIMARY KEY" => {
                    defs[i].primary_key = Some(KeyDef {
                        name: None,
                        columns: c.columns.clone(),
                    })
                }
                "UNIQUE" => defs[i].unique_keys.push(KeyDef {
                    name: None,
                    columns: c.columns.clone(),
                }),
                "CHECK" => defs[i].checks.push(CheckDef {
                    name: None,
                    expression: c.expression.clone().unwrap_or_else(|| c.text.clone()),
                }),
                "FOREIGN KEY" => {
                    let mut fk = build_fk(&c, schema, &names);
                    fk.schema = None;
                    fk.table = String::new();
                    defs[i].foreign_keys.push(fk);
                }
                _ => {}
            }
        }
        // foreign keys of the whole database referencing these tables
        for c in load_constraints(w, db, None, None)? {
            if c.kind != "FOREIGN KEY" {
                continue;
            }
            let fk = build_fk(&c, &c.schema, &names);
            if !fk.ref_schema.as_deref().is_some_and(|s| eq_ci(s, schema)) {
                continue;
            }
            if let Some(&i) = index_of.get(&lower(&fk.ref_table)) {
                defs[i].referenced_by.push(fk);
            }
        }
    }
    if opts.indexes {
        for idx in load_indexes(w, db, Some(schema), table)? {
            if let Some(&i) = index_of.get(&lower(&idx.table)) {
                defs[i].indexes.push(build_index(&idx));
            }
        }
    }
    Ok(defs)
}

pub(crate) fn table_definition(w: &Worker, obj: &ObjectRef) -> DbResult<TableDef> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    let name = obj
        .table
        .clone()
        .filter(|_| obj.kind.is_table_child())
        .unwrap_or_else(|| obj.name.clone());
    table_defs(w, &db, &schema, Some(&name), &SnapshotOptions::full())?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("Table", obj))
}

/// View name, `CREATE VIEW` statement, comment, temporary.
type ViewRow = (String, Option<String>, Option<String>, bool);

fn view_rows(
    w: &Worker,
    db: &String,
    schema: &String,
    name: Option<&String>,
) -> DbResult<Vec<ViewRow>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT view_name, sql, comment, temporary FROM duckdb_views()",
        db,
        Some(schema),
        "view_name",
        name,
        &mut p,
    );
    if !w.show_system_objects {
        sql.push_str(" AND NOT internal");
    }
    sql.push_str(" ORDER BY view_name");
    w.query_map(&sql, &p, |r| {
        (text(r, 0), opt_text(r, 1), opt_text(r, 2), flag(r, 3))
    })
}

fn view_defs(
    w: &Worker,
    db: &String,
    schema: &String,
    name: Option<&String>,
    opts: &SnapshotOptions,
) -> DbResult<Vec<ViewDef>> {
    let rows = view_rows(w, db, schema, name)?;
    let mut columns: HashMap<String, Vec<ColumnDef>> = HashMap::new();
    if opts.columns && !rows.is_empty() {
        let empty = HashSet::new();
        for c in load_columns(w, db, Some(schema), name)? {
            let col = build_column(&c, &c.table, &empty, opts.comments);
            columns.entry(lower(&c.table)).or_default().push(col);
        }
    }
    Ok(rows
        .into_iter()
        .map(|(view, sql, comment, temporary)| {
            let mut options = std::collections::BTreeMap::new();
            if temporary {
                options.insert("temporary".to_string(), "true".to_string());
            }
            ViewDef {
                schema: Some(schema.clone()),
                columns: columns.remove(&lower(&view)).unwrap_or_default(),
                definition: if opts.view_definitions {
                    sql.as_deref().map(view_query).unwrap_or_default()
                } else {
                    String::new()
                },
                name: view,
                materialized: false,
                comment: comment.filter(|_| opts.comments),
                options,
            }
        })
        .collect())
}

pub(crate) fn view_definition(w: &Worker, obj: &ObjectRef) -> DbResult<ViewDef> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    view_defs(w, &db, &schema, Some(&obj.name), &SnapshotOptions::full())?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("View", obj))
}

fn sequence_defs(
    w: &Worker,
    db: &String,
    schema: &String,
    name: Option<&String>,
) -> DbResult<Vec<SequenceDef>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT sequence_name, start_value, increment_by, min_value, max_value, cycle, last_value, comment \
         FROM duckdb_sequences()",
        db,
        Some(schema),
        "sequence_name",
        name,
        &mut p,
    );
    sql.push_str(" ORDER BY sequence_name");
    w.query_map(&sql, &p, |r| SequenceDef {
        schema: Some(schema.clone()),
        name: text(r, 0),
        data_type: Some("BIGINT".into()),
        start: int(r, 1),
        increment: int(r, 2),
        min_value: int(r, 3),
        max_value: int(r, 4),
        cache: None,
        cycle: flag(r, 5),
        owned_by: None,
        last_value: int(r, 6),
        comment: opt_text(r, 7),
    })
}

/// `CREATE SEQUENCE` for a sequence (DuckDB's own `sql` is not schema
/// qualified and starts at the current value).
fn sequence_sql(s: &SequenceDef) -> String {
    let mut sql = format!(
        "CREATE SEQUENCE {}",
        qualified(s.schema.as_deref(), &s.name)
    );
    if let Some(i) = s.increment {
        sql.push_str(&format!(" INCREMENT BY {i}"));
    }
    if let Some(m) = s.min_value {
        sql.push_str(&format!(" MINVALUE {m}"));
    }
    if let Some(m) = s.max_value {
        sql.push_str(&format!(" MAXVALUE {m}"));
    }
    if let Some(st) = s.start {
        sql.push_str(&format!(" START WITH {st}"));
    }
    if s.cycle {
        sql.push_str(" CYCLE");
    }
    sql
}

pub(crate) fn sequence_definition(w: &Worker, obj: &ObjectRef) -> DbResult<SequenceDef> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    sequence_defs(w, &db, &schema, Some(&obj.name))?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("Sequence", obj))
}

/// User macros (DuckDB's functions written in SQL).
fn routine_defs(
    w: &Worker,
    db: &String,
    schema: &String,
    name: Option<&String>,
) -> DbResult<Vec<RoutineDef>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT function_name, function_type, parameters, macro_definition, comment, return_type FROM duckdb_functions()",
        db,
        Some(schema),
        "function_name",
        name,
        &mut p,
    );
    sql.push_str(
        " AND function_type IN ('macro', 'table_macro') AND NOT internal ORDER BY function_name",
    );
    let rows = w.query_map(&sql, &p, |r| {
        (
            text(r, 0),
            text(r, 1),
            list(r, 2),
            text(r, 3),
            opt_text(r, 4),
            opt_text(r, 5),
        )
    })?;
    let mut seen = HashSet::new();
    Ok(rows
        .into_iter()
        .filter(|(n, _, params, ..)| seen.insert((n.clone(), params.clone())))
        .map(|(fname, ftype, params, body, comment, returns)| {
            let arguments = params
                .iter()
                .map(|p| quote_ident(p))
                .collect::<Vec<_>>()
                .join(", ");
            let table = ftype == "table_macro";
            let definition = format!(
                "CREATE MACRO {}({arguments}) AS {}{}",
                qualified(Some(schema), &fname),
                if table { "TABLE " } else { "" },
                body.trim()
            );
            RoutineDef {
                schema: Some(schema.clone()),
                name: fname,
                kind: Some(ObjectKind::Function),
                arguments,
                returns: if table { Some("TABLE".into()) } else { returns },
                language: Some("SQL".into()),
                definition,
                comment,
            }
        })
        .collect())
}

pub(crate) fn routine_definition(w: &Worker, obj: &ObjectRef) -> DbResult<RoutineDef> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    let all = routine_defs(w, &db, &schema, Some(&obj.name))?;
    let wanted = obj.signature.as_deref().map(|s| {
        s.trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .to_string()
    });
    let found = match wanted {
        Some(sig) => all
            .iter()
            .find(|r| r.arguments == sig)
            .cloned()
            .or_else(|| all.first().cloned()),
        None => all.first().cloned(),
    };
    found.ok_or_else(|| not_found("Macro", obj))
}

fn type_defs(
    w: &Worker,
    db: &String,
    schema: &String,
    name: Option<&String>,
) -> DbResult<Vec<TypeDef>> {
    let mut p = Vec::new();
    let mut sql = filter_sql(
        "SELECT type_name, logical_type, labels, comment FROM duckdb_types()",
        db,
        Some(schema),
        "type_name",
        name,
        &mut p,
    );
    sql.push_str(" AND NOT internal ORDER BY type_name");
    w.query_map(&sql, &p, |r| {
        let type_name = text(r, 0);
        let logical = text(r, 1);
        let labels = list(r, 2);
        let is_enum = logical.eq_ignore_ascii_case("ENUM");
        let target = if is_enum {
            format!(
                "ENUM ({})",
                labels
                    .iter()
                    .map(|l| string_literal(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            logical.clone()
        };
        TypeDef {
            schema: Some(schema.clone()),
            definition: format!(
                "CREATE TYPE {} AS {target}",
                qualified(Some(schema), &type_name)
            ),
            name: type_name,
            category: if is_enum {
                "enum".into()
            } else {
                "alias".into()
            },
            elements: labels,
            comment: opt_text(r, 3),
        }
    })
}

// --------------------------------------------------------------- navigator

pub(crate) fn list_objects(
    w: &Worker,
    schema_ref: &SchemaRef,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let ctx = context(w)?;
    let db = ctx.database(schema_ref.database.as_deref());
    let schema = ctx.schema(schema_ref.database.as_deref(), schema_ref.schema.as_deref());
    let info = |k: ObjectKind, table: Option<&str>, name: &str| {
        ObjectInfo::new(ctx.obj(k, &db, &schema, table, name))
    };
    Ok(match kind {
        ObjectKind::Table => load_tables(w, &db, Some(&schema), None)?
            .into_iter()
            .map(|t| {
                let mut o = info(ObjectKind::Table, None, &t.name).with_comment(t.comment);
                o.row_estimate = t.estimated;
                if t.temporary {
                    o.detail = Some("temporary".into());
                }
                o
            })
            .collect(),
        ObjectKind::View => view_rows(w, &db, &schema, None)?
            .into_iter()
            .map(|(name, _, comment, _)| info(ObjectKind::View, None, &name).with_comment(comment))
            .collect(),
        ObjectKind::Index => load_indexes(w, &db, Some(&schema), None)?
            .into_iter()
            .map(|i| {
                let detail = format!("{}ON {}", if i.unique { "UNIQUE " } else { "" }, i.table);
                info(ObjectKind::Index, Some(&i.table), &i.name)
                    .with_detail(detail)
                    .with_comment(i.comment)
            })
            .collect(),
        ObjectKind::Sequence => sequence_defs(w, &db, &schema, None)?
            .into_iter()
            .map(|s| info(ObjectKind::Sequence, None, &s.name).with_comment(s.comment))
            .collect(),
        ObjectKind::Function => routine_defs(w, &db, &schema, None)?
            .into_iter()
            .map(|r| {
                let mut o =
                    info(ObjectKind::Function, None, &r.name).with_comment(r.comment.clone());
                o.obj.signature = Some(format!("({})", r.arguments));
                o.with_detail(format!("({})", r.arguments))
            })
            .collect(),
        ObjectKind::Type => type_defs(w, &db, &schema, None)?
            .into_iter()
            .map(|t| {
                let detail = if t.category == "enum" {
                    "ENUM".to_string()
                } else {
                    t.definition.clone()
                };
                info(ObjectKind::Type, None, &t.name)
                    .with_detail(detail)
                    .with_comment(t.comment)
            })
            .collect(),
        _ => Vec::new(),
    })
}

fn column_detail(c: &ColumnDef, pk: bool) -> String {
    let mut s = c.data_type.clone();
    if pk {
        s.push_str(" PK");
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if c.generated.is_some() {
        s.push_str(" GENERATED");
    }
    s
}

pub(crate) fn list_children(
    w: &Worker,
    parent: &ObjectRef,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, parent);
    let table = parent.name.clone();
    let child =
        |k: ObjectKind, name: &str| ObjectInfo::new(ctx.obj(k, &db, &schema, Some(&table), name));
    Ok(match kind {
        ObjectKind::Column => {
            let generated = load_tables(w, &db, Some(&schema), Some(&table))?
                .first()
                .and_then(|t| t.sql.as_deref().map(generated_columns))
                .unwrap_or_default();
            let pk: Vec<String> = load_constraints(w, &db, Some(&schema), Some(&table))?
                .into_iter()
                .filter(|c| c.kind == "PRIMARY KEY")
                .flat_map(|c| c.columns)
                .collect();
            load_columns(w, &db, Some(&schema), Some(&table))?
                .iter()
                .map(|c| {
                    let col = build_column(c, &table, &generated, true);
                    let is_pk = pk.iter().any(|p| eq_ci(p, &col.name));
                    child(ObjectKind::Column, &col.name)
                        .with_detail(column_detail(&col, is_pk))
                        .with_comment(col.comment)
                })
                .collect()
        }
        ObjectKind::PrimaryKey
        | ObjectKind::UniqueKey
        | ObjectKind::ForeignKey
        | ObjectKind::CheckConstraint => {
            let wanted = match kind {
                ObjectKind::PrimaryKey => "PRIMARY KEY",
                ObjectKind::UniqueKey => "UNIQUE",
                ObjectKind::ForeignKey => "FOREIGN KEY",
                _ => "CHECK",
            };
            load_constraints(w, &db, Some(&schema), Some(&table))?
                .into_iter()
                .filter(|c| c.kind == wanted)
                .map(|c| child(kind, &c.name).with_detail(c.text))
                .collect()
        }
        ObjectKind::Index => load_indexes(w, &db, Some(&schema), Some(&table))?
            .into_iter()
            .map(|i| {
                let cols: Vec<String> = i
                    .sql
                    .as_deref()
                    .map(index_columns)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.name)
                    .collect();
                let detail = format!(
                    "{}({})",
                    if i.unique { "UNIQUE " } else { "" },
                    cols.join(", ")
                );
                child(ObjectKind::Index, &i.name)
                    .with_detail(detail)
                    .with_comment(i.comment)
            })
            .collect(),
        _ => Vec::new(),
    })
}

// --------------------------------------------------------------------- DDL

fn push_stmt(out: &mut String, sql: &str) {
    let s = sql.trim();
    if s.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(s);
    if !s.ends_with(';') {
        out.push(';');
    }
    out.push('\n');
}

pub(crate) fn object_ddl(w: &Worker, obj: &ObjectRef) -> DbResult<String> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    let mut out = String::new();
    match obj.kind {
        ObjectKind::Table => {
            let t = load_tables(w, &db, Some(&schema), Some(&obj.name))?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Table", obj))?;
            // sequences used by column defaults come first
            for c in load_columns(w, &db, Some(&schema), Some(&t.name))? {
                if let Some(seq) = c.default.as_deref().and_then(nextval_sequence) {
                    let (seq_schema, seq_name) = match seq.rsplit_once('.') {
                        Some((s, n)) => {
                            (s.rsplit('.').next().unwrap_or(s).to_string(), n.to_string())
                        }
                        None => (schema.clone(), seq.clone()),
                    };
                    if let Some(def) = sequence_defs(w, &db, &seq_schema, Some(&seq_name))?.first()
                    {
                        push_stmt(&mut out, &sequence_sql(def));
                    }
                }
            }
            push_stmt(&mut out, t.sql.as_deref().unwrap_or(""));
            for i in load_indexes(w, &db, Some(&schema), Some(&t.name))? {
                push_stmt(&mut out, i.sql.as_deref().unwrap_or(""));
            }
            let name = qualified(Some(&schema), &t.name);
            if let Some(c) = &t.comment {
                push_stmt(
                    &mut out,
                    &format!("COMMENT ON TABLE {name} IS {}", string_literal(c)),
                );
            }
            for c in load_columns(w, &db, Some(&schema), Some(&t.name))? {
                if let Some(cm) = &c.comment {
                    push_stmt(
                        &mut out,
                        &format!(
                            "COMMENT ON COLUMN {name}.{} IS {}",
                            quote_ident(&c.name),
                            string_literal(cm)
                        ),
                    );
                }
            }
        }
        ObjectKind::View => {
            let (_, sql, comment, _) = view_rows(w, &db, &schema, Some(&obj.name))?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("View", obj))?;
            push_stmt(&mut out, sql.as_deref().unwrap_or(""));
            if let Some(c) = comment {
                push_stmt(
                    &mut out,
                    &format!(
                        "COMMENT ON VIEW {} IS {}",
                        qualified(Some(&schema), &obj.name),
                        string_literal(&c)
                    ),
                );
            }
        }
        ObjectKind::Index => {
            let i = load_indexes(w, &db, Some(&schema), None)?
                .into_iter()
                .find(|i| eq_ci(&i.name, &obj.name))
                .ok_or_else(|| not_found("Index", obj))?;
            push_stmt(&mut out, i.sql.as_deref().unwrap_or(""));
        }
        ObjectKind::Sequence => {
            let def = sequence_defs(w, &db, &schema, Some(&obj.name))?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Sequence", obj))?;
            push_stmt(&mut out, &sequence_sql(&def));
        }
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
            push_stmt(&mut out, &routine_definition(w, obj)?.definition);
        }
        ObjectKind::Type => {
            let t = type_defs(w, &db, &schema, Some(&obj.name))?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Type", obj))?;
            push_stmt(&mut out, &t.definition);
        }
        ObjectKind::Schema => {
            let name = obj
                .schema
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&obj.name);
            push_stmt(
                &mut out,
                &format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(name)),
            );
        }
        ObjectKind::Database => {
            let rows = w.query_map(
                "SELECT path, readonly FROM duckdb_databases() WHERE lower(database_name) = lower(?)",
                &[&obj.name],
                |r| (opt_text(r, 0), flag(r, 1)),
            )?;
            let (path, readonly) = rows
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Database", obj))?;
            let path = path.unwrap_or_else(|| ":memory:".into());
            push_stmt(
                &mut out,
                &format!(
                    "ATTACH {} AS {}{}",
                    string_literal(&path),
                    quote_ident(&obj.name),
                    if readonly { " (READ_ONLY)" } else { "" }
                ),
            );
        }
        ObjectKind::PrimaryKey
        | ObjectKind::UniqueKey
        | ObjectKind::ForeignKey
        | ObjectKind::CheckConstraint
        | ObjectKind::Column => {
            let table = obj.table.clone().unwrap_or_default();
            let mut found = String::new();
            if obj.kind == ObjectKind::Column {
                let def = table_definition(w, &obj.parent_table().unwrap_or_else(|| obj.clone()))?;
                if let Some(c) = def.columns.iter().find(|c| eq_ci(&c.name, &obj.name)) {
                    found = format!("{} {}", quote_ident(&c.name), c.data_type);
                }
            } else if let Some(c) = load_constraints(w, &db, Some(&schema), Some(&table))?
                .into_iter()
                .find(|c| eq_ci(&c.name, &obj.name))
            {
                found = c.text;
            }
            if found.is_empty() {
                return Err(not_found(obj.kind.label(), obj));
            }
            out.push_str(&format!(
                "-- {} of {}\n{found}\n",
                obj.kind.label(),
                qualified(Some(&schema), &table)
            ));
        }
        other => {
            return Err(DbError::unsupported(format!(
                "DDL of {} objects is not available for DuckDB",
                other.label()
            )));
        }
    }
    Ok(out)
}

// -------------------------------------------------------------- properties

fn yes_no(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

pub(crate) fn object_properties(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    let mut props = vec![Property::new("Name", obj.name.clone())];
    match obj.kind {
        ObjectKind::Database => {
            let rows = w.query_map(
                "SELECT path, type, readonly, internal, comment FROM duckdb_databases() WHERE lower(database_name) = lower(?)",
                &[&obj.name],
                |r| (opt_text(r, 0), text(r, 1), flag(r, 2), flag(r, 3), opt_text(r, 4)),
            )?;
            let (path, ty, ro, internal, comment) = rows
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Database", obj))?;
            props.push(Property::new(
                "File",
                path.unwrap_or_else(|| ":memory:".into()),
            ));
            props.push(Property::new("Type", ty));
            props.push(Property::new("Read only", yes_no(ro)));
            props.push(Property::new("System", yes_no(internal)));
            props.push(Property::new("Current", yes_no(eq_ci(&obj.name, &ctx.db))));
            if let Some(c) = comment {
                props.push(Property::new("Comment", c));
            }
        }
        ObjectKind::Table => {
            let t = table_definition(w, obj)?;
            props.push(Property::new("Database", db));
            props.push(Property::new("Schema", schema));
            props.push(Property::new(
                "Temporary",
                yes_no(t.table_kind == TableKind::Temporary),
            ));
            props.push(Property::new("Columns", t.columns.len().to_string()));
            if let Some(pk) = &t.primary_key {
                props.push(Property::new("Primary key", pk.columns.join(", ")));
            }
            props.push(Property::new("Indexes", t.indexes.len().to_string()));
            props.push(Property::new(
                "Foreign keys",
                t.foreign_keys.len().to_string(),
            ));
            if let Some(n) = t.row_estimate {
                props.push(Property::new("Estimated rows", n.to_string()));
            }
            if let Some(c) = t.comment {
                props.push(Property::new("Comment", c));
            }
        }
        ObjectKind::View => {
            let v = view_definition(w, obj)?;
            props.push(Property::new("Database", db));
            props.push(Property::new("Schema", schema));
            props.push(Property::new("Columns", v.columns.len().to_string()));
            if let Some(c) = v.comment {
                props.push(Property::new("Comment", c));
            }
        }
        ObjectKind::Sequence => {
            let s = sequence_definition(w, obj)?;
            let opt = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_default();
            props.push(Property::new("Start", opt(s.start)));
            props.push(Property::new("Increment", opt(s.increment)));
            props.push(Property::new("Minimum", opt(s.min_value)));
            props.push(Property::new("Maximum", opt(s.max_value)));
            props.push(Property::new("Cycle", yes_no(s.cycle)));
            props.push(Property::new("Last value", opt(s.last_value)));
        }
        ObjectKind::Column => {
            let def = table_definition(w, &obj.parent_table().unwrap_or_else(|| obj.clone()))?;
            let c = def
                .columns
                .iter()
                .find(|c| eq_ci(&c.name, &obj.name))
                .ok_or_else(|| not_found("Column", obj))?;
            props.push(Property::new("Type", c.data_type.clone()));
            props.push(Property::new("Nullable", yes_no(c.nullable)));
            props.push(Property::new("Position", c.ordinal.to_string()));
            if let Some(d) = &c.default {
                props.push(Property::new("Default", d.clone()));
            }
            if let Some(g) = &c.generated {
                props.push(Property::new("Generated", g.clone()));
            }
            props.push(Property::new("Auto increment", yes_no(c.auto_increment)));
            if let Some(cm) = &c.comment {
                props.push(Property::new("Comment", cm.clone()));
            }
        }
        _ => {
            props.push(Property::new("Database", db));
            props.push(Property::new("Schema", schema));
            props.push(Property::new("Type", obj.kind.label()));
        }
    }
    Ok(props)
}

pub(crate) fn object_statistics(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let ctx = context(w)?;
    let (db, schema) = namespace(&ctx, obj);
    match obj.kind {
        ObjectKind::Database | ObjectKind::Schema => {
            let name = if obj.kind == ObjectKind::Database {
                obj.name.clone()
            } else {
                db
            };
            let rows = w.query_map(
                "SELECT database_size, block_size, total_blocks, used_blocks, free_blocks, wal_size, memory_usage, \
                 memory_limit FROM pragma_database_size() WHERE lower(database_name) = lower(?)",
                &[&name],
                |r| (0..8).map(|i| text(r, i)).collect::<Vec<_>>(),
            )?;
            let Some(v) = rows.into_iter().next() else {
                return Ok(Vec::new());
            };
            let labels = [
                "Database size",
                "Block size",
                "Total blocks",
                "Used blocks",
                "Free blocks",
                "WAL size",
                "Memory usage",
                "Memory limit",
            ];
            Ok(labels
                .iter()
                .zip(v)
                .map(|(l, v)| Property::new(*l, v))
                .collect())
        }
        ObjectKind::Table => {
            let t = load_tables(w, &db, Some(&schema), Some(&obj.name))?
                .into_iter()
                .next()
                .ok_or_else(|| not_found("Table", obj))?;
            let counts = w.query_map(
                "SELECT column_count, index_count, check_constraint_count FROM duckdb_tables() \
                 WHERE lower(database_name) = lower(?) AND lower(schema_name) = lower(?) AND lower(table_name) = lower(?)",
                &[&db, &schema, &obj.name],
                |r| (int(r, 0), int(r, 1), int(r, 2)),
            )?;
            let mut props = Vec::new();
            if let Some(n) = t.estimated {
                props.push(Property::new("Estimated rows", n.to_string()));
            }
            if let Some((c, i, k)) = counts.into_iter().next() {
                let s = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_default();
                props.push(Property::new("Columns", s(c)));
                props.push(Property::new("Indexes", s(i)));
                props.push(Property::new("Check constraints", s(k)));
            }
            Ok(props)
        }
        _ => Ok(Vec::new()),
    }
}

// ------------------------------------------------------------------ search

fn matcher(search: &MetadataSearch) -> impl Fn(&str) -> bool + '_ {
    let wildcard = search.pattern.contains(['*', '?']);
    let lower_pattern = search.pattern.to_lowercase();
    move |name: &str| {
        if search.pattern.is_empty() {
            true
        } else if wildcard {
            if search.case_sensitive {
                wildcard_match(&search.pattern, name) && {
                    // wildcard_match ignores case: require the literal parts to match exactly
                    search
                        .pattern
                        .split(['*', '?'])
                        .filter(|p| !p.is_empty())
                        .all(|p| name.contains(p))
                }
            } else {
                wildcard_match(&search.pattern, name)
            }
        } else if search.case_sensitive {
            name.contains(search.pattern.as_str())
        } else {
            name.to_lowercase().contains(&lower_pattern)
        }
    }
}

pub(crate) fn search_metadata(w: &Worker, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
    let ctx = context(w)?;
    let matches = matcher(search);
    let limit = if search.limit == 0 {
        usize::MAX
    } else {
        search.limit
    };
    let wants = |k: ObjectKind| search.kinds.is_empty() || search.kinds.contains(&k);
    let in_scope =
        |schema: &str| search.schemas.is_empty() || search.schemas.iter().any(|s| eq_ci(s, schema));
    let hit = |name: &str, comment: Option<&str>| {
        matches(name) || (search.search_comments && comment.is_some_and(&matches))
    };
    let mut out = Vec::new();
    let dbs: Vec<String> = list_databases(w)?.into_iter().map(|d| d.name).collect();
    for db in &dbs {
        if wants(ObjectKind::Table) {
            for t in load_tables(w, db, None, None)? {
                if out.len() < limit && in_scope(&t.schema) && hit(&t.name, t.comment.as_deref()) {
                    out.push(
                        ObjectInfo::new(ctx.obj(ObjectKind::Table, db, &t.schema, None, &t.name))
                            .with_comment(t.comment),
                    );
                }
            }
        }
        if wants(ObjectKind::View) {
            let mut p: Vec<&dyn ToSql> = Vec::new();
            let mut sql = filter_sql(
                "SELECT schema_name, view_name, comment FROM duckdb_views()",
                db,
                None,
                "",
                None,
                &mut p,
            );
            if !w.show_system_objects {
                sql.push_str(" AND NOT internal");
            }
            for (schema, name, comment) in
                w.query_map(&sql, &p, |r| (text(r, 0), text(r, 1), opt_text(r, 2)))?
            {
                if out.len() < limit && in_scope(&schema) && hit(&name, comment.as_deref()) {
                    out.push(
                        ObjectInfo::new(ctx.obj(ObjectKind::View, db, &schema, None, &name))
                            .with_comment(comment),
                    );
                }
            }
        }
        if wants(ObjectKind::Column) {
            for c in load_columns(w, db, None, None)? {
                if out.len() < limit && in_scope(&c.schema) && hit(&c.name, c.comment.as_deref()) {
                    out.push(
                        ObjectInfo::new(ctx.obj(
                            ObjectKind::Column,
                            db,
                            &c.schema,
                            Some(&c.table),
                            &c.name,
                        ))
                        .with_detail(c.data_type.clone())
                        .with_comment(c.comment),
                    );
                }
            }
        }
        if wants(ObjectKind::Index) {
            for i in load_indexes(w, db, None, None)? {
                if out.len() < limit && in_scope(&i.schema) && hit(&i.name, i.comment.as_deref()) {
                    out.push(ObjectInfo::new(ctx.obj(
                        ObjectKind::Index,
                        db,
                        &i.schema,
                        Some(&i.table),
                        &i.name,
                    )));
                }
            }
        }
        if wants(ObjectKind::Sequence) || wants(ObjectKind::Function) {
            let mut p: Vec<&dyn ToSql> = Vec::new();
            let sql = filter_sql(
                "SELECT schema_name, sequence_name, comment FROM duckdb_sequences()",
                db,
                None,
                "",
                None,
                &mut p,
            );
            if wants(ObjectKind::Sequence) {
                for (schema, name, comment) in
                    w.query_map(&sql, &p, |r| (text(r, 0), text(r, 1), opt_text(r, 2)))?
                {
                    if out.len() < limit && in_scope(&schema) && hit(&name, comment.as_deref()) {
                        out.push(ObjectInfo::new(ctx.obj(
                            ObjectKind::Sequence,
                            db,
                            &schema,
                            None,
                            &name,
                        )));
                    }
                }
            }
            if wants(ObjectKind::Function) {
                let mut p: Vec<&dyn ToSql> = Vec::new();
                let mut sql = filter_sql(
                    "SELECT DISTINCT schema_name, function_name, comment FROM duckdb_functions()",
                    db,
                    None,
                    "",
                    None,
                    &mut p,
                );
                sql.push_str(" AND function_type IN ('macro', 'table_macro') AND NOT internal");
                for (schema, name, comment) in
                    w.query_map(&sql, &p, |r| (text(r, 0), text(r, 1), opt_text(r, 2)))?
                {
                    if out.len() < limit && in_scope(&schema) && hit(&name, comment.as_deref()) {
                        out.push(ObjectInfo::new(ctx.obj(
                            ObjectKind::Function,
                            db,
                            &schema,
                            None,
                            &name,
                        )));
                    }
                }
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    out.truncate(limit);
    Ok(out)
}

// ---------------------------------------------------------------- snapshot

pub(crate) fn schema_snapshot(
    w: &Worker,
    schema_ref: &SchemaRef,
    opts: &SnapshotOptions,
) -> DbResult<SchemaSnapshot> {
    let ctx = context(w)?;
    let db = ctx.database(schema_ref.database.as_deref());
    let schema = ctx.schema(schema_ref.database.as_deref(), schema_ref.schema.as_deref());
    let mut snap = SchemaSnapshot {
        kind: Some(DbKind::DuckDb),
        database: ctx.db_ref(&db),
        schema: Some(schema.clone()),
        tables: table_defs(w, &db, &schema, None, opts)?,
        ..Default::default()
    };
    if opts.views {
        snap.views = view_defs(w, &db, &schema, None, opts)?;
    }
    if opts.sequences {
        snap.sequences = sequence_defs(w, &db, &schema, None)?;
    }
    if opts.routines {
        snap.routines = routine_defs(w, &db, &schema, None)?;
    }
    if opts.types {
        snap.types = type_defs(w, &db, &schema, None)?;
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_helpers() {
        let g = generated_columns(
            "CREATE TABLE t(a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL, c INTEGER AS (a * 2), \"D x\" VARCHAR DEFAULT('x'));",
        );
        assert!(g.contains("b") && g.contains("c") && !g.contains("a") && !g.contains("d x"));
        let cols = index_columns("CREATE UNIQUE INDEX i ON s.t(a, \"B\" DESC, (lower(c)));");
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "a");
        assert!(cols[1].descending && cols[1].name == "B");
        assert!(cols[2].is_expression && cols[2].name == "lower(c)");
        assert_eq!(
            view_query("CREATE VIEW v AS SELECT 1 AS a;"),
            "SELECT 1 AS a"
        );
        assert_eq!(
            view_query("CREATE VIEW v(x) AS SELECT (1) AS a"),
            "SELECT (1) AS a"
        );
        assert_eq!(
            fk_ref_schema("FOREIGN KEY (b) REFERENCES s.parent(id)", "parent"),
            Some("s".into())
        );
        assert_eq!(
            fk_ref_schema("FOREIGN KEY (b) REFERENCES parent(id)", "parent"),
            None
        );
        assert_eq!(
            nextval_sequence("nextval('main.t_id_seq')"),
            Some("main.t_id_seq".into())
        );
        assert_eq!(nextval_sequence("42"), None);
    }
}
