//! Catalog access: navigator listings, table / view definitions, DDL,
//! properties, statistics, dependencies, metadata search and snapshots.
//!
//! Schemas are the attached databases of the connection (`main`, `temp` and
//! every `ATTACH`ed file). Definitions combine `PRAGMA table_xinfo`,
//! `foreign_key_list`, `index_list` / `index_xinfo` with the parsed
//! `sqlite_schema.sql` text (constraint names, CHECK and generated
//! expressions, AUTOINCREMENT, ...). Bulk queries join `sqlite_schema` with the
//! table-valued pragma functions so a snapshot needs a handful of statements
//! regardless of the number of tables.

use crate::convert::declared_kind;
use crate::ddl::options::*;
use crate::dialect::quote_ident;
use crate::error::SqliteResultExt;
use crate::lexer::{TokKind, tokenize};
use crate::schema_sql::{
    ParsedIndex, ParsedTable, parse_create_index, parse_create_table, parse_create_trigger,
    parse_create_view,
};
use crate::worker::Worker;
use dbiji_core::config::{DbKind, wildcard_match};
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{DbError, DbResult};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ToSql};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Databases larger than this skip expensive statistics (exact row counts).
const EXACT_COUNT_LIMIT: i64 = 256 * 1024 * 1024;
/// Databases larger than this skip `dbstat` page statistics.
const DBSTAT_LIMIT: i64 = 1024 * 1024 * 1024;

// ------------------------------------------------------------------ helpers

fn text(r: &rusqlite::Row<'_>, i: usize) -> String {
    match r.get_ref(i) {
        Ok(ValueRef::Text(b)) | Ok(ValueRef::Blob(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ValueRef::Integer(n)) => n.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn opt_text(r: &rusqlite::Row<'_>, i: usize) -> Option<String> {
    match r.get_ref(i) {
        Ok(ValueRef::Null) | Err(_) => None,
        Ok(_) => Some(text(r, i)),
    }
}

fn int(r: &rusqlite::Row<'_>, i: usize) -> i64 {
    match r.get_ref(i) {
        Ok(ValueRef::Integer(n)) => n,
        Ok(ValueRef::Real(f)) => f as i64,
        Ok(ValueRef::Text(b)) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

/// Run `sql` and map every row.
fn query<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn ToSql],
    mut f: impl FnMut(&rusqlite::Row<'_>) -> T,
) -> DbResult<Vec<T>> {
    let mut stmt = conn.prepare(sql).db()?;
    let mut rows = stmt.query(params).db()?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        out.push(f(r));
    }
    Ok(out)
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_internal(name: &str) -> bool {
    name.len() >= 7 && name[..7].eq_ignore_ascii_case("sqlite_")
}

/// `schema.sqlite_schema` reference.
fn schema_table(schema: &str) -> String {
    format!("{}.sqlite_schema", quote_ident(schema))
}

/// One row of `sqlite_schema`.
#[derive(Debug, Clone)]
struct SchemaRow {
    kind: String,
    name: String,
    tbl_name: String,
    rootpage: i64,
    sql: Option<String>,
}

fn schema_rows(conn: &Connection, schema: &str) -> DbResult<Vec<SchemaRow>> {
    query(
        conn,
        &format!(
            "SELECT type, name, tbl_name, rootpage, sql FROM {} ORDER BY rowid",
            schema_table(schema)
        ),
        &[],
        |r| SchemaRow {
            kind: text(r, 0),
            name: text(r, 1),
            tbl_name: text(r, 2),
            rootpage: int(r, 3),
            sql: opt_text(r, 4),
        },
    )
}

/// Attached databases: (name, file).
fn databases(conn: &Connection) -> DbResult<Vec<(String, String)>> {
    query(
        conn,
        "SELECT name, file FROM pragma_database_list ORDER BY seq",
        &[],
        |r| (text(r, 0), text(r, 1)),
    )
}

/// Canonical name of an attached database.
fn known_schema(conn: &Connection, schema: &str) -> DbResult<String> {
    if eq_ci(schema, "temp") {
        return Ok("temp".into());
    }
    databases(conn)?
        .into_iter()
        .find(|(n, _)| eq_ci(n, schema))
        .map(|(n, _)| n)
        .ok_or_else(|| {
            DbError::query(format!(
                "Unknown database '{schema}' (not attached to this connection)"
            ))
        })
}

/// Schema containing object `name`: the given schema, else SQLite's search
/// order (temp, main, attached databases).
fn resolve_schema(conn: &Connection, schema: Option<&str>, name: &str) -> DbResult<String> {
    if let Some(s) = schema.filter(|s| !s.is_empty()) {
        return known_schema(conn, s);
    }
    let mut order: Vec<String> = vec!["temp".into()];
    order.extend(
        databases(conn)?
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| !eq_ci(n, "temp")),
    );
    for s in order {
        let found = query(
            conn,
            &format!(
                "SELECT 1 FROM {} WHERE name = ?1 COLLATE NOCASE LIMIT 1",
                schema_table(&s)
            ),
            &[&name],
            |_| (),
        )
        .map(|v| !v.is_empty())
        .unwrap_or(false);
        if found {
            return Ok(s);
        }
    }
    Ok("main".into())
}

fn schema_or_main(conn: &Connection, schema: Option<&str>) -> DbResult<String> {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => known_schema(conn, s),
        None => Ok("main".into()),
    }
}

/// File behind a schema ("" for in-memory and temp databases).
fn schema_file(conn: &Connection, schema: &str) -> String {
    databases(conn)
        .ok()
        .and_then(|d| {
            d.into_iter()
                .find(|(n, _)| eq_ci(n, schema))
                .map(|(_, f)| f)
        })
        .unwrap_or_default()
}

fn pragma_i64(conn: &Connection, schema: &str, pragma: &str) -> Option<i64> {
    conn.query_row(
        &format!("PRAGMA {}.{pragma}", quote_ident(schema)),
        [],
        |r| Ok(int(r, 0)),
    )
    .ok()
}

fn pragma_text(conn: &Connection, schema: Option<&str>, pragma: &str) -> Option<String> {
    let sql = match schema {
        Some(s) => format!("PRAGMA {}.{pragma}", quote_ident(s)),
        None => format!("PRAGMA {pragma}"),
    };
    conn.query_row(&sql, [], |r| Ok(text(r, 0))).ok()
}

/// Size of a schema's database in bytes.
fn schema_size(conn: &Connection, schema: &str) -> Option<i64> {
    Some(pragma_i64(conn, schema, "page_count")? * pragma_i64(conn, schema, "page_size")?)
}

/// Row estimates from `sqlite_stat1` (table → rows).
/// Database files up to this size get their tables' sizes: `dbstat` reads
/// every page, too slow for a large file.
const SIZES_UP_TO: i64 = 256 * 1024 * 1024;

/// Bytes of each table with its indexes (lower-case table name), from the
/// `dbstat` virtual table; empty for a large database file.
fn table_sizes(conn: &Connection, schema: &str) -> HashMap<String, i64> {
    let q = quote_ident(schema);
    let pragma = |name: &str| {
        conn.query_row(&format!("PRAGMA {q}.{name}"), [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
    };
    if pragma("page_count") * pragma("page_size") > SIZES_UP_TO {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT coalesce(m.tbl_name, d.name), SUM(d.pgsize) FROM dbstat(?1) d \
         LEFT JOIN {schema_table} m ON m.name = d.name GROUP BY 1",
        schema_table = schema_table(schema),
    );
    query(conn, &sql, &[&schema], |r| {
        (text(r, 0).to_ascii_lowercase(), int(r, 1))
    })
    .unwrap_or_default()
    .into_iter()
    .collect()
}

fn stat1_estimates(conn: &Connection, schema: &str) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let has = conn
        .query_row(
            &format!(
                "SELECT 1 FROM {} WHERE type = 'table' AND name = 'sqlite_stat1'",
                schema_table(schema)
            ),
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has {
        return out;
    }
    let rows = query(
        conn,
        &format!("SELECT tbl, stat FROM {}.sqlite_stat1", quote_ident(schema)),
        &[],
        |r| (text(r, 0), text(r, 1)),
    );
    for (tbl, stat) in rows.unwrap_or_default() {
        if let Some(n) = stat
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<i64>().ok())
        {
            let e = out.entry(tbl.to_ascii_lowercase()).or_insert(n);
            *e = (*e).max(n);
        }
    }
    out
}

/// `PRAGMA table_list` rows of a schema: name → (type, without_rowid, strict, ncol).
fn table_list(conn: &Connection, schema: &str) -> HashMap<String, (String, bool, bool, i64)> {
    query(
        conn,
        "SELECT name, type, wr, strict, ncol FROM pragma_table_list WHERE schema = ?1",
        &[&schema],
        |r| {
            (
                text(r, 0).to_ascii_lowercase(),
                (text(r, 1), int(r, 2) != 0, int(r, 3) != 0, int(r, 4)),
            )
        },
    )
    .map(|v| v.into_iter().collect())
    .unwrap_or_default()
}

// ------------------------------------------------------------ table loading

/// One row of `PRAGMA table_xinfo`.
#[derive(Debug, Clone)]
struct XInfo {
    name: String,
    data_type: String,
    not_null: bool,
    default: Option<String>,
    pk: i64,
    hidden: i64,
}

/// One row of `PRAGMA foreign_key_list`.
#[derive(Debug, Clone)]
struct FkRow {
    id: i64,
    table: String,
    from: String,
    to: Option<String>,
    on_update: String,
    on_delete: String,
}

/// One index of `PRAGMA index_list` with its `index_xinfo` key columns.
#[derive(Debug, Clone)]
struct IndexInfo {
    name: String,
    unique: bool,
    origin: String,
    partial: bool,
    /// (cid, name, desc)
    columns: Vec<(i64, Option<String>, bool)>,
}

/// Group per-table rows of a bulk pragma join; falls back to one query per
/// table when the bulk query fails (e.g. a virtual table whose module is not
/// available).
fn per_table<T>(
    conn: &Connection,
    schema: &str,
    tables: &[String],
    bulk_sql: &str,
    single_sql: &str,
    mut map: impl FnMut(&rusqlite::Row<'_>, usize) -> T,
    offset: usize,
) -> HashMap<String, Vec<T>> {
    let mut out: HashMap<String, Vec<T>> = HashMap::new();
    if tables.len() > 1 {
        let bulk = query(conn, bulk_sql, &[&schema], |r| {
            (text(r, 0).to_ascii_lowercase(), map(r, offset))
        });
        if let Ok(rows) = bulk {
            for (t, v) in rows {
                out.entry(t).or_default().push(v);
            }
            return out;
        }
    }
    for t in tables {
        if let Ok(rows) = query(conn, single_sql, &[t, &schema], |r| map(r, offset - 1)) {
            out.insert(t.to_ascii_lowercase(), rows);
        }
    }
    out
}

fn xinfo_map(r: &rusqlite::Row<'_>, o: usize) -> XInfo {
    XInfo {
        name: text(r, o + 1),
        data_type: text(r, o + 2),
        not_null: int(r, o + 3) != 0,
        default: opt_text(r, o + 4),
        pk: int(r, o + 5),
        hidden: int(r, o + 6),
    }
}

fn load_columns(conn: &Connection, schema: &str, tables: &[String]) -> HashMap<String, Vec<XInfo>> {
    let bulk = format!(
        "SELECT m.name, x.cid, x.name, x.type, x.\"notnull\", x.dflt_value, x.pk, x.hidden \
         FROM {} AS m JOIN pragma_table_xinfo(m.name, ?1) AS x \
         WHERE m.type IN ('table', 'view') ORDER BY m.name, x.cid",
        schema_table(schema)
    );
    let single = "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid";
    per_table(conn, schema, tables, &bulk, single, xinfo_map, 1)
}

fn fk_map(r: &rusqlite::Row<'_>, o: usize) -> FkRow {
    FkRow {
        id: int(r, o),
        table: text(r, o + 2),
        from: text(r, o + 3),
        to: opt_text(r, o + 4),
        on_update: text(r, o + 5),
        on_delete: text(r, o + 6),
    }
}

fn load_fks(conn: &Connection, schema: &str, tables: &[String]) -> HashMap<String, Vec<FkRow>> {
    let bulk = format!(
        "SELECT m.name, f.id, f.seq, f.\"table\", f.\"from\", f.\"to\", f.on_update, f.on_delete \
         FROM {} AS m JOIN pragma_foreign_key_list(m.name, ?1) AS f \
         WHERE m.type = 'table' ORDER BY m.name, f.id, f.seq",
        schema_table(schema)
    );
    let single = "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq";
    per_table(conn, schema, tables, &bulk, single, fk_map, 1)
}

fn load_indexes(
    conn: &Connection,
    schema: &str,
    tables: &[String],
) -> HashMap<String, Vec<IndexInfo>> {
    struct Raw {
        index: String,
        unique: bool,
        origin: String,
        partial: bool,
        cid: Option<i64>,
        col: Option<String>,
        desc: bool,
        key: bool,
    }
    let map = |r: &rusqlite::Row<'_>, o: usize| Raw {
        index: text(r, o),
        unique: int(r, o + 1) != 0,
        origin: text(r, o + 2),
        partial: int(r, o + 3) != 0,
        cid: match r.get_ref(o + 4) {
            Ok(ValueRef::Null) | Err(_) => None,
            Ok(_) => Some(int(r, o + 4)),
        },
        col: opt_text(r, o + 5),
        desc: int(r, o + 6) != 0,
        key: int(r, o + 7) != 0,
    };
    let bulk = format!(
        "SELECT m.name, il.name, il.\"unique\", il.origin, il.partial, ix.cid, ix.name, ix.\"desc\", ix.\"key\" \
         FROM {} AS m JOIN pragma_index_list(m.name, ?1) AS il \
         LEFT JOIN pragma_index_xinfo(il.name, ?1) AS ix \
         WHERE m.type = 'table' ORDER BY m.name, il.seq DESC, ix.seqno",
        schema_table(schema)
    );
    let single = "SELECT il.name, il.\"unique\", il.origin, il.partial, ix.cid, ix.name, ix.\"desc\", ix.\"key\" \
                  FROM pragma_index_list(?1, ?2) AS il LEFT JOIN pragma_index_xinfo(il.name, ?2) AS ix \
                  ORDER BY il.seq DESC, ix.seqno";
    let raw = per_table(conn, schema, tables, &bulk, single, map, 1);
    let mut out = HashMap::new();
    for (table, rows) in raw {
        let mut list: Vec<IndexInfo> = Vec::new();
        for r in rows {
            let pos = match list.iter().position(|i| i.name == r.index) {
                Some(p) => p,
                None => {
                    list.push(IndexInfo {
                        name: r.index.clone(),
                        unique: r.unique,
                        origin: r.origin.clone(),
                        partial: r.partial,
                        columns: Vec::new(),
                    });
                    list.len() - 1
                }
            };
            if r.key
                && let Some(cid) = r.cid
            {
                list[pos].columns.push((cid, r.col, r.desc));
            }
        }
        out.insert(table, list);
    }
    out
}

/// Primary key columns of a table (for implicit foreign key targets).
fn pk_columns(conn: &Connection, schema: &str, table: &str) -> Vec<String> {
    let mut cols = query(
        conn,
        "SELECT name, pk FROM pragma_table_xinfo(?1, ?2) WHERE pk > 0",
        &[&table, &schema],
        |r| (int(r, 1), text(r, 0)),
    )
    .unwrap_or_default();
    cols.sort();
    cols.into_iter().map(|(_, n)| n).collect()
}

/// Foreign keys from pragma rows, completed with names and DEFERRABLE from the parsed SQL.
fn build_fks(
    conn: &Connection,
    schema: &str,
    table: &str,
    rows: &[FkRow],
    parsed: Option<&ParsedTable>,
) -> Vec<ForeignKeyDef> {
    let mut ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids.dedup();
    // PRAGMA ids are assigned in reverse declaration order
    ids.reverse();
    let parsed_fks = parsed.map(ParsedTable::foreign_keys).unwrap_or_default();
    let mut used = vec![false; parsed_fks.len()];
    let mut out = Vec::new();
    for id in ids {
        let parts: Vec<&FkRow> = rows.iter().filter(|r| r.id == id).collect();
        let Some(first) = parts.first() else { continue };
        let columns: Vec<String> = parts.iter().map(|r| r.from.clone()).collect();
        let mut ref_columns: Vec<String> = parts.iter().filter_map(|r| r.to.clone()).collect();
        if ref_columns.len() != parts.len() {
            ref_columns = pk_columns(conn, schema, &first.table);
        }
        let matched = parsed_fks.iter().enumerate().find(|(i, p)| {
            !used[*i]
                && eq_ci(&p.ref_table, &first.table)
                && p.columns.len() == columns.len()
                && p.columns.iter().zip(&columns).all(|(a, b)| eq_ci(a, b))
        });
        let (name, deferrable) = match matched {
            Some((i, p)) => {
                used[i] = true;
                (p.name.clone(), p.deferred)
            }
            None => (None, false),
        };
        out.push(ForeignKeyDef {
            name,
            schema: Some(schema.to_string()),
            table: table.to_string(),
            columns,
            ref_schema: Some(schema.to_string()),
            ref_table: first.table.clone(),
            ref_columns,
            on_update: FkAction::parse(&first.on_update),
            on_delete: FkAction::parse(&first.on_delete),
            deferrable,
        });
    }
    out
}

fn build_index(info: &IndexInfo, sql: Option<&String>, table_cols: &[XInfo]) -> IndexDef {
    let parsed: Option<ParsedIndex> = sql.and_then(|s| parse_create_index(s));
    let columns = info
        .columns
        .iter()
        .enumerate()
        .map(|(i, (cid, name, desc))| {
            let term = parsed.as_ref().and_then(|p| p.columns.get(i));
            match (*cid, name) {
                (-2, _) => IndexColumn {
                    name: term
                        .map(|t| t.name.clone())
                        .unwrap_or_else(|| "<expression>".into()),
                    is_expression: true,
                    descending: *desc,
                },
                (-1, _) => IndexColumn {
                    name: "rowid".into(),
                    is_expression: false,
                    descending: *desc,
                },
                (c, n) => IndexColumn {
                    name: n
                        .clone()
                        .or_else(|| {
                            usize::try_from(c)
                                .ok()
                                .and_then(|c| table_cols.get(c))
                                .map(|x| x.name.clone())
                        })
                        .unwrap_or_default(),
                    is_expression: false,
                    descending: *desc,
                },
            }
        })
        .collect();
    IndexDef {
        name: info.name.clone(),
        columns,
        unique: info.unique,
        primary: info.origin == "pk",
        method: None,
        predicate: if info.partial {
            parsed.as_ref().and_then(|p| p.predicate.clone())
        } else {
            None
        },
        include: Vec::new(),
        definition: sql.cloned(),
        comment: None,
    }
}

fn build_trigger(sql: &str) -> Option<TriggerDef> {
    let p = parse_create_trigger(sql)?;
    Some(TriggerDef {
        name: p.name,
        timing: p.timing,
        events: vec![p.event],
        for_each_row: true,
        enabled: true,
        definition: sql.to_string(),
        body: Some(p.body),
        comment: None,
    })
}

struct Loaded {
    tables: Vec<TableDef>,
}

/// Load table definitions of `schema` (only `only` when given).
fn load_tables(
    w: &Worker,
    schema: &str,
    only: Option<&str>,
    opts: &SnapshotOptions,
) -> DbResult<Loaded> {
    let conn = &w.conn;
    let rows = schema_rows(conn, schema)?;
    let list = table_list(conn, schema);
    let table_rows: Vec<&SchemaRow> = rows
        .iter()
        .filter(|r| r.kind == "table")
        .filter(|r| match only {
            Some(n) => eq_ci(&r.name, n),
            None => {
                let shadow = list
                    .get(&r.name.to_ascii_lowercase())
                    .is_some_and(|(t, ..)| t == "shadow");
                !is_internal(&r.name) && !shadow
            }
        })
        .collect();
    let names: Vec<String> = table_rows.iter().map(|r| r.name.clone()).collect();
    let columns = if opts.columns || opts.indexes {
        load_columns(conn, schema, &names)
    } else {
        HashMap::new()
    };
    let fks = if opts.constraints {
        load_fks(conn, schema, &names)
    } else {
        HashMap::new()
    };
    let indexes = if opts.indexes || opts.constraints {
        load_indexes(conn, schema, &names)
    } else {
        HashMap::new()
    };
    let estimates = if opts.statistics || only.is_some() {
        stat1_estimates(conn, schema)
    } else {
        HashMap::new()
    };
    let index_sql: HashMap<String, &String> = rows
        .iter()
        .filter(|r| r.kind == "index")
        .filter_map(|r| r.sql.as_ref().map(|s| (r.name.to_ascii_lowercase(), s)))
        .collect();
    let mut tables = Vec::with_capacity(table_rows.len());
    for row in table_rows {
        let key = row.name.to_ascii_lowercase();
        let parsed = row.sql.as_deref().and_then(parse_create_table);
        let empty = Vec::new();
        let cols = columns.get(&key).unwrap_or(&empty);
        let mut t = TableDef::new(Some(schema.to_string()), row.name.clone());
        t.table_kind = if eq_ci(schema, "temp") {
            TableKind::Temporary
        } else {
            TableKind::Table
        };
        if let Some(p) = &parsed {
            if p.without_rowid {
                t.options.insert(WITHOUT_ROWID.into(), "true".into());
            }
            if p.strict {
                t.options.insert(STRICT.into(), "true".into());
            }
            if let Some((module, args)) = &p.virtual_module {
                t.options.insert(VIRTUAL_MODULE.into(), module.clone());
                t.options.insert(VIRTUAL_ARGS.into(), args.clone());
            }
        }
        let autoinc = parsed.as_ref().and_then(ParsedTable::autoincrement_column);
        if opts.columns {
            for (i, x) in cols.iter().filter(|x| x.hidden != 1).enumerate() {
                let pc = parsed.as_ref().and_then(|p| p.column(&x.name));
                let generated = if x.hidden == 2 || x.hidden == 3 {
                    pc.and_then(|c| c.generated.as_ref().map(|(e, _)| e.clone()))
                } else {
                    None
                };
                if x.hidden == 3 {
                    t.options
                        .insert(format!("{GENERATED_PREFIX}{}", x.name), "stored".into());
                }
                if let Some(cf) = pc
                    .and_then(|c| c.not_null.as_ref())
                    .and_then(|n| n.conflict.clone())
                {
                    t.options
                        .insert(format!("{CONFLICT_NOT_NULL_PREFIX}{}", x.name), cf);
                }
                t.columns.push(ColumnDef {
                    name: x.name.clone(),
                    data_type: x.data_type.clone(),
                    kind: declared_kind(&x.data_type),
                    nullable: !x.not_null,
                    default: if generated.is_some() {
                        None
                    } else {
                        pc.and_then(|c| c.default.clone())
                            .or_else(|| x.default.clone())
                    },
                    auto_increment: autoinc.as_deref().is_some_and(|a| eq_ci(a, &x.name)),
                    generated,
                    comment: pc.and_then(|c| c.comment.clone()),
                    collation: pc.and_then(|c| c.collation.clone()),
                    ordinal: u32::try_from(i + 1).unwrap_or(u32::MAX),
                });
            }
        }
        if opts.constraints {
            let mut pk: Vec<(i64, String)> = cols
                .iter()
                .filter(|x| x.pk > 0)
                .map(|x| (x.pk, x.name.clone()))
                .collect();
            pk.sort();
            if pk.is_empty()
                && let Some(p) = &parsed
            {
                pk = p
                    .primary_key_columns()
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| (i as i64, c))
                    .collect();
            }
            if !pk.is_empty() {
                let info = parsed.as_ref().and_then(ParsedTable::primary_key_info);
                if let Some(cf) = info.as_ref().and_then(|i| i.conflict.clone()) {
                    t.options.insert(CONFLICT_PK.into(), cf);
                }
                t.primary_key = Some(KeyDef {
                    name: info.and_then(|i| i.name),
                    columns: pk.into_iter().map(|(_, c)| c).collect(),
                });
            }
            match &parsed {
                Some(p) => {
                    for (name, columns, conflict) in p.unique_constraints() {
                        let key = KeyDef { name, columns };
                        if let Some(cf) = conflict {
                            t.options.insert(
                                format!(
                                    "{CONFLICT_UNIQUE_PREFIX}{}",
                                    key.name.clone().unwrap_or_else(|| key.columns.join(","))
                                ),
                                cf,
                            );
                        }
                        t.unique_keys.push(key);
                    }
                    t.checks = p
                        .checks()
                        .into_iter()
                        .map(|c| CheckDef {
                            name: c.name,
                            expression: c.expression,
                        })
                        .collect();
                }
                None => {
                    for idx in indexes.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
                        if idx.origin == "u" {
                            let columns = idx
                                .columns
                                .iter()
                                .filter_map(|(_, n, _)| n.clone())
                                .collect();
                            t.unique_keys.push(KeyDef {
                                name: None,
                                columns,
                            });
                        }
                    }
                }
            }
            t.foreign_keys = build_fks(
                conn,
                schema,
                &row.name,
                fks.get(&key).map(Vec::as_slice).unwrap_or(&[]),
                parsed.as_ref(),
            );
        }
        if opts.indexes {
            for info in indexes.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
                t.indexes.push(build_index(
                    info,
                    index_sql.get(&info.name.to_ascii_lowercase()).copied(),
                    cols,
                ));
            }
        }
        if opts.triggers {
            for tr in rows
                .iter()
                .filter(|r| r.kind == "trigger" && eq_ci(&r.tbl_name, &row.name))
            {
                if let Some(def) = tr.sql.as_deref().and_then(build_trigger) {
                    t.triggers.push(def);
                }
            }
        }
        t.row_estimate = estimates.get(&key).copied();
        tables.push(t);
    }
    Ok(Loaded { tables })
}

/// Foreign keys of all tables of `schema` referencing `table`.
fn referencing_fks(w: &Worker, schema: &str, table: &str) -> DbResult<Vec<ForeignKeyDef>> {
    let opts = SnapshotOptions {
        columns: false,
        constraints: true,
        indexes: false,
        triggers: false,
        ..SnapshotOptions::light()
    };
    let loaded = load_tables(w, schema, None, &opts)?;
    Ok(loaded
        .tables
        .into_iter()
        .flat_map(|t| t.foreign_keys)
        .filter(|fk| eq_ci(&fk.ref_table, table))
        .collect())
}

// --------------------------------------------------------------- listings

/// Display name of the main database (file name or `:memory:`).
pub(crate) fn main_database_name(w: &Worker) -> String {
    let file = schema_file(&w.conn, "main");
    if file.is_empty() {
        return ":memory:".into();
    }
    std::path::Path::new(&file)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or(file)
}

pub(crate) fn list_databases(w: &mut Worker) -> DbResult<Vec<DatabaseInfo>> {
    let file = schema_file(&w.conn, "main");
    Ok(vec![DatabaseInfo {
        name: main_database_name(w),
        is_current: true,
        is_system: false,
        owner: None,
        comment: (!file.is_empty()).then_some(file),
        encoding: pragma_text(&w.conn, None, "encoding"),
        size_bytes: schema_size(&w.conn, "main"),
    }])
}

pub(crate) fn list_schemas(w: &mut Worker) -> DbResult<Vec<SchemaInfo>> {
    let mut dbs = databases(&w.conn)?;
    if !dbs.iter().any(|(n, _)| eq_ci(n, "temp")) {
        dbs.insert(1.min(dbs.len()), ("temp".into(), String::new()));
    }
    Ok(dbs
        .into_iter()
        .map(|(name, file)| SchemaInfo {
            is_current: eq_ci(&name, "main"),
            is_system: eq_ci(&name, "temp"),
            owner: None,
            comment: (!file.is_empty()).then_some(file),
            name,
        })
        .collect())
}

fn object_info(kind: ObjectKind, schema: &str, name: &str) -> ObjectInfo {
    ObjectInfo::new(ObjectRef::new(kind, Some(schema.to_string()), name))
}

fn index_detail(info: &IndexInfo, table: &str) -> String {
    let cols: Vec<String> = info
        .columns
        .iter()
        .map(|(cid, n, desc)| {
            let mut s = match cid {
                -2 => "<expr>".to_string(),
                -1 => "rowid".to_string(),
                _ => n.clone().unwrap_or_default(),
            };
            if *desc {
                s.push_str(" DESC");
            }
            s
        })
        .collect();
    let mut d = String::new();
    if info.unique {
        d.push_str("UNIQUE ");
    }
    d.push_str(&format!("ON {table} ({})", cols.join(", ")));
    match info.origin.as_str() {
        "pk" => d.push_str(" — primary key"),
        "u" => d.push_str(" — unique constraint"),
        _ => {}
    }
    if info.partial {
        d.push_str(" — partial");
    }
    d
}

pub(crate) fn list_objects(
    w: &Worker,
    schema: Option<&str>,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let conn = &w.conn;
    let schema = schema_or_main(conn, schema)?;
    let rows = schema_rows(conn, &schema)?;
    let show_system = w.show_system_objects;
    let mut out = Vec::new();
    match kind {
        ObjectKind::Table => {
            let list = table_list(conn, &schema);
            let estimates = stat1_estimates(conn, &schema);
            let sizes = table_sizes(conn, &schema);
            for r in rows.iter().filter(|r| r.kind == "table") {
                let key = r.name.to_ascii_lowercase();
                let (ty, wr, strict, _) = list
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| ("table".into(), false, false, 0));
                let system = is_internal(&r.name) || ty == "shadow";
                if system && !show_system {
                    continue;
                }
                let mut info = object_info(ObjectKind::Table, &schema, &r.name);
                info.is_system = system;
                info.row_estimate = estimates.get(&key).copied();
                info.size_bytes = sizes.get(&key).copied();
                let mut detail = Vec::new();
                if ty == "virtual" {
                    let module = r
                        .sql
                        .as_deref()
                        .and_then(parse_create_table)
                        .and_then(|p| p.virtual_module)
                        .map(|(m, _)| m);
                    detail.push(format!("virtual table ({})", module.unwrap_or_default()));
                } else if ty == "shadow" {
                    detail.push("shadow table".into());
                }
                if wr {
                    detail.push("WITHOUT ROWID".into());
                }
                if strict {
                    detail.push("STRICT".into());
                }
                if !detail.is_empty() {
                    info.detail = Some(detail.join(", "));
                }
                out.push(info);
            }
        }
        ObjectKind::View => {
            for r in rows.iter().filter(|r| r.kind == "view") {
                out.push(object_info(ObjectKind::View, &schema, &r.name));
            }
        }
        ObjectKind::Index => {
            let tables: Vec<String> = rows
                .iter()
                .filter(|r| r.kind == "table")
                .map(|r| r.name.clone())
                .collect();
            let indexes = load_indexes(conn, &schema, &tables);
            for r in rows.iter().filter(|r| r.kind == "index") {
                let auto = r.sql.is_none();
                if auto && !show_system {
                    continue;
                }
                let mut info = object_info(ObjectKind::Index, &schema, &r.name);
                info.obj.table = Some(r.tbl_name.clone());
                info.is_system = auto;
                let found = indexes
                    .get(&r.tbl_name.to_ascii_lowercase())
                    .and_then(|l| l.iter().find(|i| i.name == r.name));
                info.detail = Some(match found {
                    Some(i) => index_detail(i, &r.tbl_name),
                    None => format!("ON {}", r.tbl_name),
                });
                out.push(info);
            }
        }
        ObjectKind::Trigger => {
            for r in rows.iter().filter(|r| r.kind == "trigger") {
                let mut info = object_info(ObjectKind::Trigger, &schema, &r.name);
                info.obj.table = Some(r.tbl_name.clone());
                let p = r.sql.as_deref().and_then(parse_create_trigger);
                info.detail = Some(match p {
                    Some(p) => format!("{} {} ON {}", p.timing, p.event, r.tbl_name),
                    None => format!("ON {}", r.tbl_name),
                });
                out.push(info);
            }
        }
        _ => {}
    }
    out.sort_by_key(|i| i.obj.name.to_lowercase());
    Ok(out)
}

fn column_detail(c: &ColumnDef, pk: bool) -> String {
    let mut d = if c.data_type.trim().is_empty() {
        "ANY".to_string()
    } else {
        c.data_type.clone()
    };
    if pk {
        d.push_str(" PK");
    }
    if !c.nullable {
        d.push_str(" NOT NULL");
    }
    if c.generated.is_some() {
        d.push_str(" GENERATED");
    }
    if c.auto_increment {
        d.push_str(" AUTOINCREMENT");
    }
    d
}

/// Display name of a foreign key without a name: `<table>_fk<n>`.
fn fk_display_name(table: &str, fk: &ForeignKeyDef, n: usize) -> String {
    fk.name.clone().unwrap_or_else(|| format!("{table}_fk{n}"))
}

pub(crate) fn list_children(
    w: &Worker,
    parent: &ObjectRef,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let conn = &w.conn;
    let schema = resolve_schema(conn, parent.schema.as_deref(), &parent.name)?;
    let mut parent = parent.clone();
    parent.schema = Some(schema.clone());
    let child = |k: ObjectKind, name: &str| ObjectInfo::new(ObjectRef::child(k, &parent, name));
    let mut out = Vec::new();
    match kind {
        ObjectKind::Column => {
            let names = vec![parent.name.clone()];
            let cols = load_columns(conn, &schema, &names);
            let empty = Vec::new();
            for x in cols
                .get(&parent.name.to_ascii_lowercase())
                .unwrap_or(&empty)
            {
                if x.hidden == 1 && !w.show_system_objects {
                    continue;
                }
                let mut def = ColumnDef::new(x.name.clone(), x.data_type.clone());
                def.nullable = !x.not_null;
                if x.hidden == 2 || x.hidden == 3 {
                    def.generated = Some(String::new());
                }
                let mut info = child(ObjectKind::Column, &x.name);
                info.detail = Some(column_detail(&def, x.pk > 0));
                info.is_system = x.hidden == 1;
                out.push(info);
            }
        }
        ObjectKind::Index => {
            let names = vec![parent.name.clone()];
            let idx = load_indexes(conn, &schema, &names);
            for i in idx
                .get(&parent.name.to_ascii_lowercase())
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let mut info = child(ObjectKind::Index, &i.name);
                info.detail = Some(index_detail(i, &parent.name));
                out.push(info);
            }
        }
        ObjectKind::Trigger => {
            for r in schema_rows(conn, &schema)?
                .into_iter()
                .filter(|r| r.kind == "trigger" && eq_ci(&r.tbl_name, &parent.name))
            {
                let mut info = child(ObjectKind::Trigger, &r.name);
                if let Some(p) = r.sql.as_deref().and_then(parse_create_trigger) {
                    info.detail = Some(format!("{} {}", p.timing, p.event));
                }
                out.push(info);
            }
        }
        ObjectKind::PrimaryKey
        | ObjectKind::UniqueKey
        | ObjectKind::ForeignKey
        | ObjectKind::CheckConstraint => {
            let def = table_definition(w, &parent)?;
            match kind {
                ObjectKind::PrimaryKey => {
                    if let Some(pk) = &def.primary_key {
                        let mut info = child(
                            ObjectKind::PrimaryKey,
                            pk.name.as_deref().unwrap_or("PRIMARY KEY"),
                        );
                        info.detail = Some(format!("({})", pk.columns.join(", ")));
                        out.push(info);
                    }
                }
                ObjectKind::UniqueKey => {
                    for (n, u) in def.unique_keys.iter().enumerate() {
                        let name = u
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("{}_unique{}", def.name, n + 1));
                        let mut info = child(ObjectKind::UniqueKey, &name);
                        info.detail = Some(format!("({})", u.columns.join(", ")));
                        out.push(info);
                    }
                }
                ObjectKind::ForeignKey => {
                    for (n, fk) in def.foreign_keys.iter().enumerate() {
                        let mut info = child(
                            ObjectKind::ForeignKey,
                            &fk_display_name(&def.name, fk, n + 1),
                        );
                        info.detail = Some(format!(
                            "({}) → {} ({})",
                            fk.columns.join(", "),
                            fk.ref_table,
                            fk.ref_columns.join(", ")
                        ));
                        out.push(info);
                    }
                }
                _ => {
                    for (n, c) in def.checks.iter().enumerate() {
                        let name = c
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("{}_check{}", def.name, n + 1));
                        let mut info = child(ObjectKind::CheckConstraint, &name);
                        info.detail = Some(c.expression.clone());
                        out.push(info);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

// ------------------------------------------------------------ definitions

pub(crate) fn table_definition(w: &Worker, table: &ObjectRef) -> DbResult<TableDef> {
    let schema = resolve_schema(&w.conn, table.schema.as_deref(), &table.name)?;
    let loaded = load_tables(w, &schema, Some(&table.name), &SnapshotOptions::full())?;
    let mut def =
        loaded.tables.into_iter().next().ok_or_else(|| {
            DbError::query(format!("Table {schema}.{} does not exist", table.name))
        })?;
    def.referenced_by = referencing_fks(w, &schema, &def.name)?;
    if let Some(size) = schema_size(&w.conn, &schema).filter(|s| *s <= DBSTAT_LIMIT) {
        let _ = size;
        def.size_bytes = dbstat_size(&w.conn, &schema, &def.name).map(|(_, bytes, _)| bytes);
    }
    Ok(def)
}

fn view_def(
    w: &Worker,
    schema: &str,
    row: &SchemaRow,
    with_columns: bool,
    with_definition: bool,
) -> ViewDef {
    let parsed = row.sql.as_deref().and_then(parse_create_view);
    let mut v = ViewDef {
        schema: Some(schema.to_string()),
        name: row.name.clone(),
        ..Default::default()
    };
    if with_definition {
        v.definition = parsed
            .as_ref()
            .map(|p| p.select.clone())
            .or_else(|| row.sql.clone())
            .unwrap_or_default();
    }
    if let Some(cols) = parsed.as_ref().and_then(|p| p.columns.clone()) {
        v.options.insert(
            VIEW_COLUMNS.into(),
            format!(
                "({})",
                cols.iter()
                    .map(|c| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    if with_columns {
        let names = vec![row.name.clone()];
        let cols = load_columns(&w.conn, schema, &names);
        for (i, x) in cols
            .get(&row.name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            v.columns.push(ColumnDef {
                name: x.name.clone(),
                data_type: x.data_type.clone(),
                kind: declared_kind(&x.data_type),
                nullable: !x.not_null,
                ordinal: u32::try_from(i + 1).unwrap_or(u32::MAX),
                ..Default::default()
            });
        }
    }
    v
}

pub(crate) fn view_definition(w: &Worker, view: &ObjectRef) -> DbResult<ViewDef> {
    let schema = resolve_schema(&w.conn, view.schema.as_deref(), &view.name)?;
    let rows = schema_rows(&w.conn, &schema)?;
    let row = rows
        .iter()
        .find(|r| r.kind == "view" && eq_ci(&r.name, &view.name))
        .ok_or_else(|| DbError::query(format!("View {schema}.{} does not exist", view.name)))?;
    Ok(view_def(w, &schema, row, true, true))
}

fn with_temp_keyword(sql: &str, schema: &str) -> String {
    if !eq_ci(schema, "temp") {
        return sql.to_string();
    }
    for object in ["TABLE", "VIEW", "TRIGGER"] {
        let prefix = format!("CREATE {object} ");
        if sql.len() >= prefix.len() && sql[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            return format!("CREATE TEMP {object} {}", &sql[prefix.len()..]);
        }
    }
    sql.to_string()
}

fn push_stmt(out: &mut String, sql: &str) {
    let s = sql.trim_end();
    out.push_str(s);
    if !s.ends_with(';') {
        out.push(';');
    }
    out.push_str("\n\n");
}

pub(crate) fn object_ddl(w: &Worker, obj: &ObjectRef) -> DbResult<String> {
    let conn = &w.conn;
    let lookup_name = match obj.kind {
        k if k.is_table_child() && k != ObjectKind::Index && k != ObjectKind::Trigger => {
            obj.table.clone().unwrap_or_else(|| obj.name.clone())
        }
        _ => obj.name.clone(),
    };
    let schema = match obj.kind {
        ObjectKind::Schema => {
            schema_or_main(conn, Some(obj.schema.as_deref().unwrap_or(&obj.name)))?
        }
        ObjectKind::Database => "main".to_string(),
        _ => resolve_schema(conn, obj.schema.as_deref(), &lookup_name)?,
    };
    let rows = schema_rows(conn, &schema)?;
    let mut out = String::new();
    let find =
        |kind: &str, name: &str| rows.iter().find(|r| r.kind == kind && eq_ci(&r.name, name));
    match obj.kind {
        ObjectKind::Schema | ObjectKind::Database => {
            let list = table_list(conn, &schema);
            for kind in ["table", "index", "view", "trigger"] {
                for r in rows.iter().filter(|r| r.kind == kind) {
                    let shadow = list
                        .get(&r.name.to_ascii_lowercase())
                        .is_some_and(|(t, ..)| t == "shadow");
                    let shadow_index = kind == "index"
                        && list
                            .get(&r.tbl_name.to_ascii_lowercase())
                            .is_some_and(|(t, ..)| t == "shadow");
                    if is_internal(&r.name) || shadow || shadow_index {
                        continue;
                    }
                    if let Some(sql) = &r.sql {
                        push_stmt(&mut out, &with_temp_keyword(sql, &schema));
                    }
                }
            }
        }
        ObjectKind::View => {
            let r = find("view", &obj.name).ok_or_else(|| {
                DbError::query(format!("View {schema}.{} does not exist", obj.name))
            })?;
            push_stmt(
                &mut out,
                &with_temp_keyword(r.sql.as_deref().unwrap_or(""), &schema),
            );
        }
        ObjectKind::Index => {
            let r = find("index", &obj.name).ok_or_else(|| {
                DbError::query(format!("Index {schema}.{} does not exist", obj.name))
            })?;
            match &r.sql {
                Some(sql) => push_stmt(&mut out, sql),
                None => out.push_str(&format!(
                    "-- Index {} is created automatically by SQLite for a PRIMARY KEY or UNIQUE constraint of table {}.\n",
                    r.name, r.tbl_name
                )),
            }
        }
        ObjectKind::Trigger => {
            let r = find("trigger", &obj.name).ok_or_else(|| {
                DbError::query(format!("Trigger {schema}.{} does not exist", obj.name))
            })?;
            push_stmt(
                &mut out,
                &with_temp_keyword(r.sql.as_deref().unwrap_or(""), &schema),
            );
        }
        _ => {
            let table = find("table", &lookup_name)
                .or_else(|| find("view", &lookup_name))
                .ok_or_else(|| {
                    DbError::query(format!("Table {schema}.{lookup_name} does not exist"))
                })?;
            push_stmt(
                &mut out,
                &with_temp_keyword(table.sql.as_deref().unwrap_or(""), &schema),
            );
            for kind in ["index", "trigger"] {
                for r in rows
                    .iter()
                    .filter(|r| r.kind == kind && eq_ci(&r.tbl_name, &table.name))
                {
                    if let Some(sql) = &r.sql {
                        push_stmt(&mut out, &with_temp_keyword(sql, &schema));
                    }
                }
            }
        }
    }
    Ok(out.trim_end().to_string() + "\n")
}

// ------------------------------------------------------------- properties

fn yes_no(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

fn database_properties(w: &Worker, schema: &str) -> Vec<Property> {
    let conn = &w.conn;
    let mut props = Vec::new();
    let g = "Database";
    let file = schema_file(conn, schema);
    props.push(Property::grouped(g, "Name", schema));
    props.push(Property::grouped(
        g,
        "File",
        if file.is_empty() {
            "(in memory)".to_string()
        } else {
            file
        },
    ));
    for p in [
        "page_size",
        "page_count",
        "freelist_count",
        "user_version",
        "application_id",
        "schema_version",
    ] {
        if let Some(v) = pragma_i64(conn, schema, p) {
            props.push(Property::grouped(g, p, v.to_string()));
        }
    }
    if let Some(size) = schema_size(conn, schema) {
        props.push(Property::grouped(g, "size_bytes", size.to_string()));
    }
    for p in ["journal_mode", "auto_vacuum", "synchronous", "locking_mode"] {
        if let Some(v) = pragma_text(conn, Some(schema), p) {
            props.push(Property::grouped(g, p, v));
        }
    }
    if let Some(v) = pragma_text(conn, None, "encoding") {
        props.push(Property::grouped(g, "encoding", v));
    }
    let c = "Connection";
    for p in ["foreign_keys", "cache_size", "temp_store", "query_only"] {
        if let Some(v) = pragma_text(conn, None, p) {
            props.push(Property::grouped(c, p, v));
        }
    }
    props.push(Property::grouped(
        c,
        "busy_timeout_ms",
        w.busy_timeout.as_millis().to_string(),
    ));
    if let Ok(v) = conn.query_row("SELECT sqlite_version()", [], |r| Ok(text(r, 0))) {
        props.push(Property::grouped(c, "sqlite_version", v));
    }
    props
}

/// Child objects defined inside `CREATE TABLE` (columns and constraints).
fn is_table_part(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Column
            | ObjectKind::PrimaryKey
            | ObjectKind::UniqueKey
            | ObjectKind::ForeignKey
            | ObjectKind::CheckConstraint
    )
}

/// Owning table definition of a column or constraint.
fn owner_definition(w: &Worker, obj: &ObjectRef) -> DbResult<TableDef> {
    let table = obj.parent_table().ok_or_else(|| {
        DbError::query(format!(
            "{} {} does not name its table",
            obj.kind.label(),
            obj.name
        ))
    })?;
    table_definition(w, &table)
}

fn not_found(obj: &ObjectRef, table: &str) -> DbError {
    DbError::query(format!(
        "{} {} does not exist in table {table}",
        obj.kind.label(),
        obj.name
    ))
}

/// Unique key with its navigator name (unnamed keys: `<table>_unique<n>`).
fn find_unique<'a>(def: &'a TableDef, name: &str) -> Option<&'a KeyDef> {
    def.unique_keys.iter().enumerate().find_map(|(n, u)| {
        let shown = u
            .name
            .clone()
            .unwrap_or_else(|| format!("{}_unique{}", def.name, n + 1));
        eq_ci(&shown, name).then_some(u)
    })
}

/// Foreign key with its navigator name (unnamed keys: `<table>_fk<n>`).
fn find_fk<'a>(def: &'a TableDef, name: &str) -> Option<&'a ForeignKeyDef> {
    def.foreign_keys
        .iter()
        .enumerate()
        .find_map(|(n, fk)| eq_ci(&fk_display_name(&def.name, fk, n + 1), name).then_some(fk))
}

/// CHECK constraint with its navigator name (unnamed checks: `<table>_check<n>`).
fn find_check<'a>(def: &'a TableDef, name: &str) -> Option<&'a CheckDef> {
    def.checks.iter().enumerate().find_map(|(n, c)| {
        let shown = c
            .name
            .clone()
            .unwrap_or_else(|| format!("{}_check{}", def.name, n + 1));
        eq_ci(&shown, name).then_some(c)
    })
}

fn table_part_properties(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let def = owner_definition(w, obj)?;
    let mut props = vec![
        Property::new("Name", obj.name.clone()),
        Property::new("Table", def.name.clone()),
    ];
    match obj.kind {
        ObjectKind::Column => {
            let c = def
                .columns
                .iter()
                .find(|c| eq_ci(&c.name, &obj.name))
                .ok_or_else(|| not_found(obj, &def.name))?;
            let ty = if c.data_type.is_empty() {
                "(none)".to_string()
            } else {
                c.data_type.clone()
            };
            props.push(Property::new("Type", ty));
            props.push(Property::new("Position", c.ordinal.to_string()));
            props.push(Property::new("Nullable", yes_no(c.nullable)));
            if let Some(d) = &c.default {
                props.push(Property::new("Default", d.clone()));
            }
            let pk_pos = def
                .primary_key
                .as_ref()
                .and_then(|k| k.columns.iter().position(|p| eq_ci(p, &c.name)));
            if let Some(pos) = pk_pos {
                props.push(Property::new("Primary key position", (pos + 1).to_string()));
            }
            props.push(Property::new("AUTOINCREMENT", yes_no(c.auto_increment)));
            if let Some(g) = &c.generated {
                let key = format!("{GENERATED_PREFIX}{}", c.name);
                let stored = def.options.get(&key).is_some_and(|v| v == "stored");
                props.push(Property::new(
                    "Generated",
                    format!("{g} ({})", if stored { "STORED" } else { "VIRTUAL" }),
                ));
            }
            if let Some(coll) = &c.collation {
                props.push(Property::new("Collation", coll.clone()));
            }
            if let Some(comment) = &c.comment {
                props.push(Property::new("Comment", comment.clone()));
            }
        }
        ObjectKind::PrimaryKey => {
            let pk = def
                .primary_key
                .as_ref()
                .ok_or_else(|| not_found(obj, &def.name))?;
            props.push(Property::new("Columns", pk.columns.join(", ")));
            if let Some(cf) = def.options.get(CONFLICT_PK) {
                props.push(Property::new("ON CONFLICT", cf.clone()));
            }
        }
        ObjectKind::UniqueKey => {
            let u = find_unique(&def, &obj.name).ok_or_else(|| not_found(obj, &def.name))?;
            props.push(Property::new("Columns", u.columns.join(", ")));
        }
        ObjectKind::ForeignKey => {
            let fk = find_fk(&def, &obj.name).ok_or_else(|| not_found(obj, &def.name))?;
            props.push(Property::new("Columns", fk.columns.join(", ")));
            props.push(Property::new(
                "References",
                format!("{} ({})", fk.ref_table, fk.ref_columns.join(", ")),
            ));
            props.push(Property::new("ON UPDATE", fk.on_update.sql()));
            props.push(Property::new("ON DELETE", fk.on_delete.sql()));
            props.push(Property::new("Deferred", yes_no(fk.deferrable)));
        }
        _ => {
            let c = find_check(&def, &obj.name).ok_or_else(|| not_found(obj, &def.name))?;
            props.push(Property::new("Expression", c.expression.clone()));
        }
    }
    Ok(props)
}

pub(crate) fn object_properties(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let conn = &w.conn;
    match obj.kind {
        ObjectKind::Database => return Ok(database_properties(w, "main")),
        ObjectKind::Schema => {
            let schema = known_schema(conn, obj.schema.as_deref().unwrap_or(&obj.name))?;
            return Ok(database_properties(w, &schema));
        }
        k if is_table_part(k) => return table_part_properties(w, obj),
        _ => {}
    }
    let schema = resolve_schema(conn, obj.schema.as_deref(), &obj.name)?;
    let rows = schema_rows(conn, &schema)?;
    let kind = match obj.kind {
        ObjectKind::View => "view",
        ObjectKind::Index => "index",
        ObjectKind::Trigger => "trigger",
        _ => "table",
    };
    let Some(row) = rows
        .iter()
        .find(|r| r.kind == kind && eq_ci(&r.name, &obj.name))
    else {
        return Err(DbError::query(format!(
            "{} {schema}.{} does not exist",
            obj.kind.label(),
            obj.name
        )));
    };
    let mut props = vec![
        Property::new("Name", row.name.clone()),
        Property::new("Database", schema.clone()),
        Property::new("Type", row.kind.clone()),
    ];
    if kind == "index" || kind == "trigger" {
        props.push(Property::new("Table", row.tbl_name.clone()));
    }
    props.push(Property::new("Root page", row.rootpage.to_string()));
    match kind {
        "table" => {
            if let Some((ty, wr, strict, ncol)) =
                table_list(conn, &schema).get(&row.name.to_ascii_lowercase())
            {
                props.push(Property::new("Table type", ty.clone()));
                props.push(Property::new("Columns", ncol.to_string()));
                props.push(Property::new("WITHOUT ROWID", yes_no(*wr)));
                props.push(Property::new("STRICT", yes_no(*strict)));
            }
            if let Some(p) = row.sql.as_deref().and_then(parse_create_table) {
                if let Some((module, args)) = &p.virtual_module {
                    props.push(Property::new("Module", module.clone()));
                    props.push(Property::new("Module arguments", args.clone()));
                }
                if let Some(c) = p.autoincrement_column() {
                    props.push(Property::new("AUTOINCREMENT column", c));
                }
            }
        }
        "index" => {
            let names = vec![row.tbl_name.clone()];
            let idx = load_indexes(conn, &schema, &names);
            if let Some(i) = idx
                .get(&row.tbl_name.to_ascii_lowercase())
                .and_then(|l| l.iter().find(|i| i.name == row.name))
            {
                props.push(Property::new("Unique", yes_no(i.unique)));
                props.push(Property::new(
                    "Origin",
                    match i.origin.as_str() {
                        "pk" => "PRIMARY KEY constraint",
                        "u" => "UNIQUE constraint",
                        _ => "CREATE INDEX",
                    },
                ));
                props.push(Property::new("Partial", yes_no(i.partial)));
            }
        }
        "trigger" => {
            if let Some(p) = row.sql.as_deref().and_then(parse_create_trigger) {
                props.push(Property::new("Timing", p.timing));
                props.push(Property::new("Event", p.event));
                if let Some(when) = p.when {
                    props.push(Property::new("When", when));
                }
            }
        }
        _ => {}
    }
    if let Some(sql) = &row.sql {
        props.push(Property::new("SQL", sql.clone()));
    }
    Ok(props)
}

/// (pages, bytes, unused bytes) of a b-tree from `dbstat`.
fn dbstat_size(conn: &Connection, schema: &str, name: &str) -> Option<(i64, i64, i64)> {
    conn.query_row(
        "SELECT count(*), total(pgsize), total(unused) FROM dbstat(?1) WHERE name = ?2",
        [schema, name],
        |r| Ok((int(r, 0), int(r, 1), int(r, 2))),
    )
    .ok()
    .filter(|(pages, _, _)| *pages > 0)
}

pub(crate) fn object_statistics(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let conn = &w.conn;
    let schema = match obj.kind {
        ObjectKind::Database => "main".to_string(),
        ObjectKind::Schema => known_schema(conn, obj.schema.as_deref().unwrap_or(&obj.name))?,
        _ => resolve_schema(conn, obj.schema.as_deref(), &obj.name)?,
    };
    let size = schema_size(conn, &schema).unwrap_or(i64::MAX);
    let mut props = Vec::new();
    match obj.kind {
        ObjectKind::Database | ObjectKind::Schema => {
            for p in ["page_count", "page_size", "freelist_count"] {
                if let Some(v) = pragma_i64(conn, &schema, p) {
                    props.push(Property::new(p, v.to_string()));
                }
            }
            props.push(Property::new("size_bytes", size.to_string()));
        }
        ObjectKind::Table | ObjectKind::Index => {
            let rows = schema_rows(conn, &schema)?;
            let kind = if obj.kind == ObjectKind::Table {
                "table"
            } else {
                "index"
            };
            let Some(row) = rows
                .iter()
                .find(|r| r.kind == kind && eq_ci(&r.name, &obj.name))
            else {
                return Err(DbError::query(format!(
                    "{} {schema}.{} does not exist",
                    obj.kind.label(),
                    obj.name
                )));
            };
            if obj.kind == ObjectKind::Table {
                if let Some(e) = stat1_estimates(conn, &schema).get(&row.name.to_ascii_lowercase())
                {
                    props.push(Property::new(
                        "Row count (estimate, ANALYZE)",
                        e.to_string(),
                    ));
                }
                if size <= EXACT_COUNT_LIMIT {
                    let sql = format!(
                        "SELECT count(*) FROM {}.{}",
                        quote_ident(&schema),
                        quote_ident(&row.name)
                    );
                    if let Ok(n) = conn.query_row(&sql, [], |r| Ok(int(r, 0))) {
                        props.push(Property::new("Row count", n.to_string()));
                    }
                } else {
                    props.push(Property::new(
                        "Row count",
                        "not computed for databases larger than 256 MiB (run SELECT count(*) explicitly)",
                    ));
                }
            }
            if size <= DBSTAT_LIMIT {
                if let Some((pages, bytes, unused)) = dbstat_size(conn, &schema, &row.name) {
                    props.push(Property::new("Pages", pages.to_string()));
                    props.push(Property::new("Size (bytes)", bytes.to_string()));
                    props.push(Property::new("Unused bytes", unused.to_string()));
                }
                if obj.kind == ObjectKind::Table {
                    let mut index_bytes = 0i64;
                    for i in rows
                        .iter()
                        .filter(|r| r.kind == "index" && eq_ci(&r.tbl_name, &row.name))
                    {
                        if let Some((_, b, _)) = dbstat_size(conn, &schema, &i.name) {
                            index_bytes += b;
                        }
                    }
                    props.push(Property::new("Index size (bytes)", index_bytes.to_string()));
                }
            }
        }
        _ => {}
    }
    Ok(props)
}

/// Identifiers mentioned in a statement (lower case).
fn mentioned_names(sql: &str) -> HashSet<String> {
    tokenize(sql)
        .iter()
        .filter(|t| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent))
        .filter_map(|t| t.ident())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

pub(crate) fn object_dependencies(w: &Worker, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
    if obj.kind == ObjectKind::ForeignKey {
        let def = owner_definition(w, obj)?;
        let fk = find_fk(&def, &obj.name).ok_or_else(|| not_found(obj, &def.name))?;
        return Ok(vec![Dependency {
            direction: DependencyDirection::DependsOn,
            target: ObjectRef::table(def.schema.clone(), fk.ref_table.clone()),
            kind: Some("foreign key".into()),
        }]);
    }
    if is_table_part(obj.kind) {
        return Ok(Vec::new());
    }
    let conn = &w.conn;
    let schema = resolve_schema(conn, obj.schema.as_deref(), &obj.name)?;
    let rows = schema_rows(conn, &schema)?;
    let me = obj.name.to_ascii_lowercase();
    let kind_of = |k: &str| match k {
        "view" => ObjectKind::View,
        "trigger" => ObjectKind::Trigger,
        "index" => ObjectKind::Index,
        _ => ObjectKind::Table,
    };
    let target = |r: &SchemaRow| {
        let mut o = ObjectRef::new(kind_of(&r.kind), Some(schema.clone()), r.name.clone());
        if matches!(r.kind.as_str(), "index" | "trigger") {
            o.table = Some(r.tbl_name.clone());
        }
        o
    };
    let mut out = Vec::new();
    let push = |out: &mut Vec<Dependency>, direction, target: ObjectRef, kind: &str| {
        let d = Dependency {
            direction,
            target,
            kind: Some(kind.to_string()),
        };
        if !out.contains(&d) {
            out.push(d);
        }
    };
    let Some(row) = rows.iter().find(|r| eq_ci(&r.name, &obj.name)) else {
        return Err(DbError::query(format!(
            "Object {schema}.{} does not exist",
            obj.name
        )));
    };
    match row.kind.as_str() {
        "table" => {
            let def =
                table_definition(w, &ObjectRef::table(Some(schema.clone()), row.name.clone()))?;
            for fk in &def.foreign_keys {
                push(
                    &mut out,
                    DependencyDirection::DependsOn,
                    ObjectRef::table(Some(schema.clone()), fk.ref_table.clone()),
                    "foreign key",
                );
            }
            for fk in &def.referenced_by {
                push(
                    &mut out,
                    DependencyDirection::UsedBy,
                    ObjectRef::table(Some(schema.clone()), fk.table.clone()),
                    "foreign key",
                );
            }
        }
        "index" | "trigger" => {
            push(
                &mut out,
                DependencyDirection::DependsOn,
                ObjectRef::table(Some(schema.clone()), row.tbl_name.clone()),
                "table",
            );
        }
        _ => {}
    }
    // objects mentioned in this object's SQL (views and trigger bodies)
    if matches!(row.kind.as_str(), "view" | "trigger") {
        let names = mentioned_names(row.sql.as_deref().unwrap_or(""));
        for r in rows
            .iter()
            .filter(|r| matches!(r.kind.as_str(), "table" | "view") && !eq_ci(&r.name, &row.name))
        {
            if names.contains(&r.name.to_ascii_lowercase()) && !eq_ci(&r.name, &row.tbl_name) {
                push(&mut out, DependencyDirection::DependsOn, target(r), &r.kind);
            }
        }
    }
    // views and triggers mentioning this object
    if matches!(row.kind.as_str(), "table" | "view") {
        for r in rows
            .iter()
            .filter(|r| matches!(r.kind.as_str(), "view" | "trigger") && !eq_ci(&r.name, &row.name))
        {
            let on_me = r.kind == "trigger" && eq_ci(&r.tbl_name, &row.name);
            if on_me || mentioned_names(r.sql.as_deref().unwrap_or("")).contains(&me) {
                push(&mut out, DependencyDirection::UsedBy, target(r), &r.kind);
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ search

fn matcher(search: &MetadataSearch) -> impl Fn(&str) -> bool + '_ {
    let wildcard = search.pattern.contains(['*', '?']);
    let lower = search.pattern.to_lowercase();
    move |name: &str| {
        if search.pattern.is_empty() {
            return true;
        }
        if wildcard {
            if search.case_sensitive {
                case_sensitive_wildcard(&search.pattern, name)
            } else {
                wildcard_match(&search.pattern, name)
            }
        } else if search.case_sensitive {
            name.contains(search.pattern.as_str())
        } else {
            name.to_lowercase().contains(&lower)
        }
    }
}

fn case_sensitive_wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub(crate) fn search_metadata(w: &Worker, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
    let conn = &w.conn;
    let matches = matcher(search);
    let schemas: Vec<String> = if search.schemas.is_empty() {
        let mut s: Vec<String> = databases(conn)?.into_iter().map(|(n, _)| n).collect();
        if !s.iter().any(|n| eq_ci(n, "temp")) {
            s.push("temp".into());
        }
        s
    } else {
        search
            .schemas
            .iter()
            .filter_map(|s| known_schema(conn, s).ok())
            .collect()
    };
    let limit = if search.limit == 0 {
        usize::MAX
    } else {
        search.limit
    };
    let wants = |k: ObjectKind| search.kinds.is_empty() || search.kinds.contains(&k);
    let mut out = Vec::new();
    for schema in schemas {
        let Ok(rows) = schema_rows(conn, &schema) else {
            continue;
        };
        let list = table_list(conn, &schema);
        for r in &rows {
            let system = is_internal(&r.name)
                || list
                    .get(&r.name.to_ascii_lowercase())
                    .is_some_and(|(t, ..)| t == "shadow");
            if system && !w.show_system_objects {
                continue;
            }
            let kind = match r.kind.as_str() {
                "table" => ObjectKind::Table,
                "view" => ObjectKind::View,
                "index" => ObjectKind::Index,
                "trigger" => ObjectKind::Trigger,
                _ => continue,
            };
            if wants(kind) && matches(&r.name) {
                let mut info = object_info(kind, &schema, &r.name);
                if matches!(kind, ObjectKind::Index | ObjectKind::Trigger) {
                    info.obj.table = Some(r.tbl_name.clone());
                    info.detail = Some(format!("ON {}", r.tbl_name));
                }
                info.is_system = system;
                out.push(info);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        if wants(ObjectKind::Column) {
            let owners: Vec<&SchemaRow> = rows
                .iter()
                .filter(|r| matches!(r.kind.as_str(), "table" | "view"))
                .filter(|r| w.show_system_objects || !is_internal(&r.name))
                .collect();
            let names: Vec<String> = owners.iter().map(|r| r.name.clone()).collect();
            let cols = load_columns(conn, &schema, &names);
            for owner in owners {
                let parsed = if search.search_comments {
                    owner.sql.as_deref().and_then(parse_create_table)
                } else {
                    None
                };
                let parent = ObjectRef::new(
                    if owner.kind == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    Some(schema.clone()),
                    owner.name.clone(),
                );
                for x in cols
                    .get(&owner.name.to_ascii_lowercase())
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    if x.hidden == 1 {
                        continue;
                    }
                    let comment = parsed
                        .as_ref()
                        .and_then(|p| p.column(&x.name))
                        .and_then(|c| c.comment.clone());
                    let hit = matches(&x.name) || comment.as_deref().is_some_and(&matches);
                    if hit {
                        let mut info = ObjectInfo::new(ObjectRef::child(
                            ObjectKind::Column,
                            &parent,
                            x.name.clone(),
                        ));
                        info.detail = Some(if x.data_type.is_empty() {
                            "ANY".into()
                        } else {
                            x.data_type.clone()
                        });
                        info.comment = comment;
                        out.push(info);
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- snapshot

pub(crate) fn schema_snapshot(
    w: &Worker,
    schema: Option<&str>,
    opts: &SnapshotOptions,
) -> DbResult<SchemaSnapshot> {
    let schema = schema_or_main(&w.conn, schema)?;
    let loaded = load_tables(w, &schema, None, opts)?;
    let mut tables = loaded.tables;
    if opts.constraints {
        let mut by_target: BTreeMap<String, Vec<ForeignKeyDef>> = BTreeMap::new();
        for t in &tables {
            for fk in &t.foreign_keys {
                by_target
                    .entry(fk.ref_table.to_ascii_lowercase())
                    .or_default()
                    .push(fk.clone());
            }
        }
        for t in &mut tables {
            t.referenced_by = by_target
                .get(&t.name.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default();
        }
    }
    let mut views = Vec::new();
    if opts.views {
        let rows = schema_rows(&w.conn, &schema)?;
        let view_rows: Vec<&SchemaRow> = rows.iter().filter(|r| r.kind == "view").collect();
        let names: Vec<String> = view_rows.iter().map(|r| r.name.clone()).collect();
        let cols = if opts.columns {
            load_columns(&w.conn, &schema, &names)
        } else {
            HashMap::new()
        };
        for r in view_rows {
            let mut v = view_def(w, &schema, r, false, opts.view_definitions);
            if opts.columns {
                for (i, x) in cols
                    .get(&r.name.to_ascii_lowercase())
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                {
                    v.columns.push(ColumnDef {
                        name: x.name.clone(),
                        data_type: x.data_type.clone(),
                        kind: declared_kind(&x.data_type),
                        nullable: !x.not_null,
                        ordinal: u32::try_from(i + 1).unwrap_or(u32::MAX),
                        ..Default::default()
                    });
                }
            }
            views.push(v);
        }
    }
    Ok(SchemaSnapshot {
        kind: Some(DbKind::Sqlite),
        database: None,
        schema: Some(schema),
        tables,
        views,
        sequences: Vec::new(),
        routines: Vec::new(),
        types: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(case_sensitive_wildcard("us*", "users"));
        assert!(!case_sensitive_wildcard("US*", "users"));
        assert!(case_sensitive_wildcard("u?ers", "users"));
    }

    #[test]
    fn temp_keyword() {
        assert_eq!(
            with_temp_keyword("CREATE TABLE t(a)", "temp"),
            "CREATE TEMP TABLE t(a)"
        );
        assert_eq!(
            with_temp_keyword("CREATE TABLE t(a)", "main"),
            "CREATE TABLE t(a)"
        );
        assert_eq!(
            with_temp_keyword("CREATE INDEX i ON t(a)", "temp"),
            "CREATE INDEX i ON t(a)"
        );
    }

    #[test]
    fn names_in_sql() {
        let n = mentioned_names("SELECT a FROM \"Order Items\" JOIN users u ON u.id = 1");
        assert!(n.contains("order items"));
        assert!(n.contains("users"));
    }
}
