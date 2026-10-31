//! Catalog queries over the `ALL_*` dictionary views. Every function runs
//! synchronously on a blocking thread (see `session::OracleSession::blocking`).

use crate::convert;
use crate::dialect::qualified;
use crate::error::OraResultExt;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{DbError, DbResult, Value};
use oracledb::{Connection, ToDbValue};
use std::collections::BTreeMap;

/// Separator of `LISTAGG` column lists (a control character no name contains).
const SEP: char = '\u{1f}';

/// Rows of `sql` as DBiji values.
pub(crate) fn query(
    conn: &Connection,
    sql: &str,
    params: &[&dyn ToDbValue],
) -> DbResult<Vec<Vec<Value>>> {
    let cursor = conn.query(sql, params).db()?;
    let reads: Vec<convert::Read> = cursor
        .columns()
        .iter()
        .map(|m| convert::column(m).1)
        .collect();
    let mut rows = Vec::new();
    for row in cursor {
        rows.push(convert::row(&row.db()?, &reads)?);
    }
    Ok(rows)
}

pub(crate) fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        other => Some(other.to_text().into_owned()),
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Decimal(s) | Value::Text(s) => s.parse().ok(),
        Value::Float(f) => Some(*f as i64),
        _ => None,
    }
}

fn col(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(text).filter(|s| !s.is_empty())
}

fn split_list(s: Option<String>) -> Vec<String> {
    s.map(|s| s.split(SEP).map(str::to_string).collect())
        .unwrap_or_default()
}

/// The schema a reference points at (its owner), or an error.
fn owner(schema: Option<&str>) -> DbResult<String> {
    schema
        .map(str::to_string)
        .ok_or_else(|| DbError::query("The object has no schema"))
}

// ------------------------------------------------------------------ schemas

pub(crate) fn current_schema(conn: &Connection) -> DbResult<Option<String>> {
    let rows = query(
        conn,
        "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual",
        &[],
    )?;
    Ok(rows.first().and_then(|r| col(r, 0)))
}

pub(crate) fn list_schemas(conn: &Connection) -> DbResult<Vec<SchemaInfo>> {
    let current = current_schema(conn)?;
    let rows = query(
        conn,
        "SELECT u.username, u.oracle_maintained FROM all_users u \
         WHERE u.username = USER OR u.username = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') \
            OR EXISTS (SELECT 1 FROM all_objects o WHERE o.owner = u.username) \
         ORDER BY u.username",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let name = col(r, 0)?;
            Some(SchemaInfo {
                is_current: current.as_deref() == Some(name.as_str()),
                is_system: col(r, 1).as_deref() == Some("Y"),
                name,
                owner: None,
                comment: None,
            })
        })
        .collect())
}

// ------------------------------------------------------------------ objects

pub(crate) fn list_objects(
    conn: &Connection,
    schema: &str,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let s = Some(schema.to_string());
    let obj = |k: ObjectKind, name: String| ObjectInfo::new(ObjectRef::new(k, s.clone(), name));
    Ok(match kind {
        ObjectKind::Table => query(
            conn,
            "SELECT t.table_name, c.comments, t.num_rows, t.temporary FROM all_tables t \
             LEFT JOIN all_tab_comments c ON c.owner = t.owner AND c.table_name = t.table_name \
             WHERE t.owner = :1 AND t.nested = 'NO' AND t.secondary = 'N' AND t.table_name NOT LIKE 'BIN$%' \
               AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name) \
             ORDER BY t.table_name",
            &[&schema],
        )?
        .into_iter()
        .filter_map(|r| {
            let mut o = obj(ObjectKind::Table, col(&r, 0)?).with_comment(col(&r, 1));
            o.row_estimate = r.get(2).and_then(int);
            if col(&r, 3).as_deref() == Some("Y") {
                o.detail = Some("temporary".into());
            }
            Some(o)
        })
        .collect(),
        ObjectKind::View => query(
            conn,
            "SELECT v.view_name, c.comments FROM all_views v \
             LEFT JOIN all_tab_comments c ON c.owner = v.owner AND c.table_name = v.view_name \
             WHERE v.owner = :1 ORDER BY v.view_name",
            &[&schema],
        )?
        .into_iter()
        .filter_map(|r| Some(obj(ObjectKind::View, col(&r, 0)?).with_comment(col(&r, 1))))
        .collect(),
        ObjectKind::MaterializedView => query(conn, "SELECT mview_name FROM all_mviews WHERE owner = :1 ORDER BY mview_name", &[&schema])?
            .into_iter()
            .filter_map(|r| Some(obj(ObjectKind::MaterializedView, col(&r, 0)?)))
            .collect(),
        ObjectKind::Sequence => query(conn, "SELECT sequence_name, last_number FROM all_sequences WHERE sequence_owner = :1 AND sequence_name NOT LIKE 'ISEQ$$%' ORDER BY sequence_name", &[&schema])?
            .into_iter()
            .filter_map(|r| Some(obj(ObjectKind::Sequence, col(&r, 0)?)))
            .collect(),
        ObjectKind::Function | ObjectKind::Procedure => {
            let types = if kind == ObjectKind::Function { "'FUNCTION'" } else { "'PROCEDURE', 'PACKAGE'" };
            query(conn, &format!("SELECT object_name, object_type, status FROM all_objects WHERE owner = :1 AND object_type IN ({types}) ORDER BY object_name"), &[&schema])?
                .into_iter()
                .filter_map(|r| {
                    let mut o = obj(kind, col(&r, 0)?);
                    let mut detail = Vec::new();
                    if col(&r, 1).as_deref() == Some("PACKAGE") {
                        o.obj.signature = Some("PACKAGE".into());
                        detail.push("package");
                    }
                    if col(&r, 2).as_deref() == Some("INVALID") {
                        detail.push("invalid");
                    }
                    if !detail.is_empty() {
                        o.detail = Some(detail.join(", "));
                    }
                    Some(o)
                })
                .collect()
        }
        ObjectKind::Type => query(conn, "SELECT type_name, typecode FROM all_types WHERE owner = :1 ORDER BY type_name", &[&schema])?
            .into_iter()
            .filter_map(|r| {
                let mut o = obj(ObjectKind::Type, col(&r, 0)?);
                o.detail = col(&r, 1).map(|c| c.to_lowercase());
                Some(o)
            })
            .collect(),
        ObjectKind::Synonym => query(conn, "SELECT synonym_name, table_owner, table_name, db_link FROM all_synonyms WHERE owner = :1 ORDER BY synonym_name", &[&schema])?
            .into_iter()
            .filter_map(|r| {
                let mut o = obj(ObjectKind::Synonym, col(&r, 0)?);
                let target = match (col(&r, 1), col(&r, 2)) {
                    (Some(o), Some(t)) => format!("{o}.{t}"),
                    (None, Some(t)) => t,
                    _ => String::new(),
                };
                o.detail = Some(match col(&r, 3) {
                    Some(link) => format!("{target}@{link}"),
                    None => target,
                })
                .filter(|d| !d.is_empty());
                Some(o)
            })
            .collect(),
        ObjectKind::Trigger => query(conn, "SELECT trigger_name, table_name FROM all_triggers WHERE owner = :1 ORDER BY trigger_name", &[&schema])?
            .into_iter()
            .filter_map(|r| {
                let mut o = obj(ObjectKind::Trigger, col(&r, 0)?);
                o.obj.table = col(&r, 1);
                Some(o)
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// `VARCHAR2(100 CHAR)`, `NUMBER(10,2)`, `RAW(16)` ... from `ALL_TAB_COLUMNS`.
fn type_text(row: &[Value], base: usize) -> String {
    let data_type = col(row, base).unwrap_or_default();
    let data_length = row.get(base + 1).and_then(int);
    let char_length = row.get(base + 2).and_then(int);
    let char_used = col(row, base + 3);
    let precision = row.get(base + 4).and_then(int);
    let scale = row.get(base + 5).and_then(int);
    match data_type.as_str() {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "VARCHAR" => {
            let n = char_length.filter(|n| *n > 0).or(data_length).unwrap_or(1);
            if char_used.as_deref() == Some("C") && data_type != "NVARCHAR2" && data_type != "NCHAR"
            {
                format!("{data_type}({n} CHAR)")
            } else {
                format!("{data_type}({n})")
            }
        }
        "RAW" => format!("RAW({})", data_length.unwrap_or(1)),
        "NUMBER" => match (precision, scale) {
            (None, Some(0)) => "INTEGER".into(),
            (None, _) => "NUMBER".into(),
            (Some(p), Some(0) | None) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
        },
        "FLOAT" => match precision {
            Some(p) if p != 126 => format!("FLOAT({p})"),
            _ => "FLOAT".into(),
        },
        _ => data_type,
    }
}

const COLUMN_COLS: &str = "c.column_name, c.data_type, c.data_length, c.char_length, c.char_used, c.data_precision, c.data_scale, c.nullable";

pub(crate) fn list_children(
    conn: &Connection,
    parent: &ObjectRef,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let schema = owner(parent.schema.as_deref())?;
    let table = parent.name.as_str();
    let child = |k: ObjectKind, name: String| ObjectInfo::new(ObjectRef::child(k, parent, name));
    Ok(match kind {
        ObjectKind::Column => query(conn, &format!("SELECT {COLUMN_COLS} FROM all_tab_columns c WHERE c.owner = :1 AND c.table_name = :2 ORDER BY c.column_id"), &[&schema, &table])?
            .into_iter()
            .filter_map(|r| {
                let mut detail = type_text(&r, 1);
                if col(&r, 7).as_deref() == Some("N") {
                    detail.push_str(" NOT NULL");
                }
                Some(child(ObjectKind::Column, col(&r, 0)?).with_detail(detail))
            })
            .collect(),
        ObjectKind::Index => query(conn, "SELECT index_name, uniqueness, index_type FROM all_indexes WHERE table_owner = :1 AND table_name = :2 AND index_type <> 'LOB' ORDER BY index_name", &[&schema, &table])?
            .into_iter()
            .filter_map(|r| {
                let mut o = child(ObjectKind::Index, col(&r, 0)?);
                let mut d = Vec::new();
                if col(&r, 1).as_deref() == Some("UNIQUE") {
                    d.push("unique".to_string());
                }
                if let Some(t) = col(&r, 2).filter(|t| t != "NORMAL") {
                    d.push(t.to_lowercase());
                }
                o.detail = (!d.is_empty()).then(|| d.join(", "));
                Some(o)
            })
            .collect(),
        ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::ForeignKey | ObjectKind::CheckConstraint => {
            let ty = match kind {
                ObjectKind::PrimaryKey => "P",
                ObjectKind::UniqueKey => "U",
                ObjectKind::ForeignKey => "R",
                _ => "C",
            };
            query(
                conn,
                "SELECT c.constraint_name, c.search_condition_vc, \
                   (SELECT LISTAGG(cc.column_name, ', ') WITHIN GROUP (ORDER BY cc.position) FROM all_cons_columns cc \
                     WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name), \
                   rc.table_name \
                 FROM all_constraints c LEFT JOIN all_constraints rc ON rc.owner = c.r_owner AND rc.constraint_name = c.r_constraint_name \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = :3 ORDER BY c.constraint_name",
                &[&schema, &table, &ty],
            )?
            .into_iter()
            .filter(|r| ty != "C" || !is_not_null_check(col(r, 1).as_deref()))
            .filter_map(|r| {
                let detail = match ty {
                    "C" => col(&r, 1),
                    "R" => Some(format!("({}) → {}", col(&r, 2).unwrap_or_default(), col(&r, 3).unwrap_or_default())),
                    _ => col(&r, 2).map(|c| format!("({c})")),
                };
                let mut o = child(kind, col(&r, 0)?);
                o.detail = detail;
                Some(o)
            })
            .collect()
        }
        ObjectKind::Trigger => query(conn, "SELECT trigger_name, trigger_type, triggering_event, status FROM all_triggers WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name", &[&schema, &table])?
            .into_iter()
            .filter_map(|r| {
                let mut o = child(ObjectKind::Trigger, col(&r, 0)?);
                let mut d = format!("{} {}", col(&r, 1).unwrap_or_default(), col(&r, 2).unwrap_or_default()).to_lowercase();
                if col(&r, 3).as_deref() == Some("DISABLED") {
                    d.push_str(", disabled");
                }
                o.detail = Some(d);
                Some(o)
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// Oracle keeps NOT NULL as check constraints `"COL" IS NOT NULL`.
fn is_not_null_check(cond: Option<&str>) -> bool {
    cond.is_some_and(|c| {
        let c = c.trim();
        c.starts_with('"') && c.ends_with("\" IS NOT NULL") && c.matches('"').count() == 2
    })
}

// ------------------------------------------------------------- definitions

/// Definitions of the tables of `schema` (only `only` when given), in one
/// query per kind of information.
pub(crate) fn table_definitions(
    conn: &Connection,
    schema: &str,
    only: Option<&str>,
    opts: &SnapshotOptions,
) -> DbResult<Vec<TableDef>> {
    let filter = if only.is_some() {
        " AND t.table_name = :2"
    } else {
        ""
    };
    let bind = |extra: &[&'static str]| -> Vec<String> {
        let _ = extra;
        let mut v = vec![schema.to_string()];
        if let Some(t) = only {
            v.push(t.to_string());
        }
        v
    };
    let owned = bind(&[]);
    let params: Vec<&dyn ToDbValue> = owned.iter().map(|s| s as &dyn ToDbValue).collect();

    let mut tables: BTreeMap<String, TableDef> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for r in query(
        conn,
        &format!(
            "SELECT t.table_name, c.comments, t.num_rows, t.temporary FROM all_tables t \
             LEFT JOIN all_tab_comments c ON c.owner = t.owner AND c.table_name = t.table_name \
             WHERE t.owner = :1{filter} AND t.nested = 'NO' AND t.secondary = 'N' AND t.table_name NOT LIKE 'BIN$%' \
               AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name) \
             ORDER BY t.table_name"
        ),
        &params,
    )? {
        let Some(name) = col(&r, 0) else { continue };
        let mut t = TableDef::new(Some(schema.to_string()), name.clone());
        t.comment = col(&r, 1);
        t.row_estimate = r.get(2).and_then(int);
        if col(&r, 3).as_deref() == Some("Y") {
            t.table_kind = TableKind::Temporary;
        }
        order.push(name.clone());
        tables.insert(name, t);
    }
    if tables.is_empty() {
        return Ok(Vec::new());
    }

    if opts.columns {
        for r in query(
            conn,
            &format!(
                "SELECT c.table_name, {COLUMN_COLS}, c.data_default, c.identity_column, c.virtual_column, cc.comments \
                 FROM all_tab_cols c JOIN all_tables t ON t.owner = c.owner AND t.table_name = c.table_name \
                 LEFT JOIN all_col_comments cc ON cc.owner = c.owner AND cc.table_name = c.table_name AND cc.column_name = c.column_name \
                 WHERE c.owner = :1{filter} AND c.hidden_column = 'NO' ORDER BY c.table_name, c.column_id"
            ),
            &params,
        )? {
            let (Some(table), Some(name)) = (col(&r, 0), col(&r, 1)) else {
                continue;
            };
            let Some(t) = tables.get_mut(&table) else {
                continue;
            };
            let data_type = type_text(&r, 2);
            let mut c = ColumnDef::new(name, data_type.clone());
            c.kind = kind_of(&data_type);
            c.nullable = col(&r, 8).as_deref() != Some("N");
            let default = col(&r, 9)
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("NULL"));
            c.auto_increment = col(&r, 10).as_deref() == Some("YES");
            if col(&r, 11).as_deref() == Some("YES") {
                c.generated = default;
            } else if !c.auto_increment {
                c.default = default;
            }
            c.comment = if opts.comments { col(&r, 12) } else { None };
            c.ordinal = t.columns.len() as u32 + 1;
            t.columns.push(c);
        }
    }

    if opts.constraints || opts.indexes {
        // Constraints: P, U, C and R with their (and the referenced) columns.
        let mut pk_index: BTreeMap<String, String> = BTreeMap::new();
        for r in query(
            conn,
            &format!(
                "SELECT c.table_name, c.constraint_name, c.constraint_type, c.search_condition_vc, c.delete_rule, c.deferrable, c.index_name, \
                   (SELECT LISTAGG(cc.column_name, CHR(31)) WITHIN GROUP (ORDER BY cc.position) FROM all_cons_columns cc \
                     WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name), \
                   rc.owner, rc.table_name, \
                   (SELECT LISTAGG(rcc.column_name, CHR(31)) WITHIN GROUP (ORDER BY rcc.position) FROM all_cons_columns rcc \
                     WHERE rcc.owner = c.r_owner AND rcc.constraint_name = c.r_constraint_name) \
                 FROM all_constraints c JOIN all_tables t ON t.owner = c.owner AND t.table_name = c.table_name \
                 LEFT JOIN all_constraints rc ON rc.owner = c.r_owner AND rc.constraint_name = c.r_constraint_name \
                 WHERE c.owner = :1{filter} AND c.constraint_type IN ('P', 'U', 'C', 'R') ORDER BY c.table_name, c.constraint_name"
            ),
            &params,
        )? {
            let (Some(table), Some(name), Some(ty)) = (col(&r, 0), col(&r, 1), col(&r, 2)) else {
                continue;
            };
            let Some(t) = tables.get_mut(&table) else {
                continue;
            };
            let columns = split_list(col(&r, 7));
            match ty.as_str() {
                "P" => {
                    if let Some(idx) = col(&r, 6) {
                        pk_index.insert(table.clone(), idx);
                    }
                    t.primary_key = Some(KeyDef {
                        name: Some(name),
                        columns,
                    });
                }
                "U" => t.unique_keys.push(KeyDef {
                    name: Some(name),
                    columns,
                }),
                "C" => {
                    let cond = col(&r, 3);
                    if !is_not_null_check(cond.as_deref())
                        && let Some(expression) = cond
                    {
                        t.checks.push(CheckDef {
                            name: Some(name),
                            expression,
                        });
                    }
                }
                "R" => t.foreign_keys.push(ForeignKeyDef {
                    name: Some(name),
                    schema: Some(schema.to_string()),
                    table: table.clone(),
                    columns,
                    ref_schema: col(&r, 8),
                    ref_table: col(&r, 9).unwrap_or_default(),
                    ref_columns: split_list(col(&r, 10)),
                    on_update: FkAction::NoAction,
                    on_delete: match col(&r, 4).as_deref() {
                        Some("CASCADE") => FkAction::Cascade,
                        Some("SET NULL") => FkAction::SetNull,
                        _ => FkAction::NoAction,
                    },
                    deferrable: col(&r, 5).as_deref() == Some("DEFERRABLE"),
                }),
                _ => {}
            }
        }

        if opts.indexes || only.is_some() {
            for r in query(
                conn,
                &format!(
                    "SELECT i.table_name, i.index_name, i.uniqueness, i.index_type, \
                       (SELECT LISTAGG(ic.column_name || CASE WHEN ic.descend = 'DESC' THEN ' DESC' END, CHR(31)) WITHIN GROUP (ORDER BY ic.column_position) \
                         FROM all_ind_columns ic WHERE ic.index_owner = i.owner AND ic.index_name = i.index_name) \
                     FROM all_indexes i JOIN all_tables t ON t.owner = i.table_owner AND t.table_name = i.table_name \
                     WHERE i.table_owner = :1{filter} AND i.index_type NOT IN ('LOB', 'IOT - TOP') ORDER BY i.table_name, i.index_name"
                ),
                &params,
            )? {
                let (Some(table), Some(name)) = (col(&r, 0), col(&r, 1)) else {
                    continue;
                };
                let Some(t) = tables.get_mut(&table) else {
                    continue;
                };
                let columns = split_list(col(&r, 4))
                    .into_iter()
                    .map(|c| {
                        let (name, descending) = match c.strip_suffix(" DESC") {
                            Some(n) => (n.to_string(), true),
                            None => (c, false),
                        };
                        IndexColumn {
                            is_expression: name.starts_with("SYS_NC"),
                            name,
                            descending,
                        }
                    })
                    .collect();
                let index_type = col(&r, 3).unwrap_or_default();
                t.indexes.push(IndexDef {
                    primary: pk_index.get(&table) == Some(&name),
                    unique: col(&r, 2).as_deref() == Some("UNIQUE"),
                    method: Some(if index_type.starts_with("BITMAP") {
                        "bitmap".into()
                    } else {
                        "btree".into()
                    }),
                    name,
                    columns,
                    ..Default::default()
                });
            }
        }

        if opts.constraints {
            // Foreign keys of other tables pointing at these tables.
            for r in query(
                conn,
                &format!(
                    "SELECT p.table_name, c.owner, c.table_name, c.constraint_name, c.delete_rule, \
                       (SELECT LISTAGG(cc.column_name, CHR(31)) WITHIN GROUP (ORDER BY cc.position) FROM all_cons_columns cc \
                         WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name), \
                       (SELECT LISTAGG(pc.column_name, CHR(31)) WITHIN GROUP (ORDER BY pc.position) FROM all_cons_columns pc \
                         WHERE pc.owner = p.owner AND pc.constraint_name = p.constraint_name) \
                     FROM all_constraints c JOIN all_constraints p ON p.owner = c.r_owner AND p.constraint_name = c.r_constraint_name \
                     WHERE c.constraint_type = 'R' AND p.owner = :1{} ORDER BY c.owner, c.table_name",
                    filter.replace("t.table_name", "p.table_name")
                ),
                &params,
            )? {
                let Some(t) = col(&r, 0).and_then(|p| tables.get_mut(&p)) else {
                    continue;
                };
                t.referenced_by.push(ForeignKeyDef {
                    name: col(&r, 3),
                    schema: col(&r, 1),
                    table: col(&r, 2).unwrap_or_default(),
                    columns: split_list(col(&r, 5)),
                    ref_schema: Some(schema.to_string()),
                    ref_table: t.name.clone(),
                    ref_columns: split_list(col(&r, 6)),
                    on_delete: if col(&r, 4).as_deref() == Some("CASCADE") {
                        FkAction::Cascade
                    } else {
                        FkAction::NoAction
                    },
                    ..Default::default()
                });
            }
        }
    }

    if opts.triggers {
        for r in query(
            conn,
            &format!(
                "SELECT t.table_name, t.trigger_name, t.trigger_type, t.triggering_event, t.status, t.description, t.trigger_body \
                 FROM all_triggers t WHERE t.table_owner = :1{filter} AND t.base_object_type = 'TABLE' ORDER BY t.table_name, t.trigger_name"
            ),
            &params,
        )? {
            let (Some(table), Some(name)) = (col(&r, 0), col(&r, 1)) else {
                continue;
            };
            let Some(t) = tables.get_mut(&table) else {
                continue;
            };
            let ty = col(&r, 2).unwrap_or_default();
            let body = col(&r, 6).unwrap_or_default();
            t.triggers.push(TriggerDef {
                name,
                timing: ty.split_whitespace().next().unwrap_or("").to_string(),
                events: col(&r, 3)
                    .unwrap_or_default()
                    .split(" OR ")
                    .map(|e| e.trim().to_string())
                    .collect(),
                for_each_row: ty.contains("EACH ROW"),
                enabled: col(&r, 4).as_deref() == Some("ENABLED"),
                definition: format!(
                    "CREATE OR REPLACE TRIGGER {}{}",
                    col(&r, 5).unwrap_or_default().trim_end(),
                    format_args!("\n{}", body.trim_end())
                ),
                body: Some(body),
                comment: None,
            });
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|n| tables.remove(&n))
        .collect())
}

pub(crate) fn kind_of(type_text: &str) -> DataKind {
    let t = type_text.to_ascii_uppercase();
    if t.starts_with("NUMBER(") && !t.contains(',') || t == "INTEGER" {
        DataKind::Int
    } else if t.starts_with("NUMBER") || t.starts_with("FLOAT") {
        DataKind::Decimal
    } else if t.starts_with("BINARY_") {
        DataKind::Float
    } else if t == "DATE" || t.starts_with("TIMESTAMP") && !t.contains("ZONE") {
        DataKind::DateTime
    } else if t.starts_with("TIMESTAMP") {
        DataKind::DateTimeTz
    } else if t.starts_with("INTERVAL") {
        DataKind::Interval
    } else if t.starts_with("RAW") || t == "BLOB" || t == "LONG RAW" || t == "BFILE" {
        DataKind::Binary
    } else if t == "JSON" {
        DataKind::Json
    } else if t == "XMLTYPE" {
        DataKind::Xml
    } else if t == "BOOLEAN" {
        DataKind::Bool
    } else {
        DataKind::Text
    }
}

pub(crate) fn table_definition(conn: &Connection, table: &ObjectRef) -> DbResult<TableDef> {
    let schema = owner(table.schema.as_deref())?;
    let opts = SnapshotOptions {
        indexes: true,
        triggers: true,
        ..SnapshotOptions::full()
    };
    table_definitions(conn, &schema, Some(&table.name), &opts)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            DbError::query(format!(
                "Table {} not found",
                qualified(Some(&schema), &table.name)
            ))
        })
}

pub(crate) fn view_definition(conn: &Connection, view: &ObjectRef) -> DbResult<ViewDef> {
    let schema = owner(view.schema.as_deref())?;
    let name = view.name.as_str();
    let materialized = view.kind == ObjectKind::MaterializedView;
    let sql = if materialized {
        "SELECT query, NULL FROM all_mviews m WHERE owner = :1 AND mview_name = :2"
    } else {
        "SELECT v.text, c.comments FROM all_views v LEFT JOIN all_tab_comments c ON c.owner = v.owner AND c.table_name = v.view_name WHERE v.owner = :1 AND v.view_name = :2"
    };
    let rows = query(conn, sql, &[&schema, &name])?;
    let row = rows.first().ok_or_else(|| {
        DbError::query(format!("View {} not found", qualified(Some(&schema), name)))
    })?;
    let columns = query(conn, &format!("SELECT {COLUMN_COLS} FROM all_tab_columns c WHERE c.owner = :1 AND c.table_name = :2 ORDER BY c.column_id"), &[&schema, &name])?
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let data_type = type_text(&r, 1);
            let mut c = ColumnDef::new(col(&r, 0)?, data_type.clone());
            c.kind = kind_of(&data_type);
            c.nullable = col(&r, 7).as_deref() != Some("N");
            c.ordinal = i as u32 + 1;
            Some(c)
        })
        .collect();
    Ok(ViewDef {
        schema: Some(schema),
        name: name.to_string(),
        materialized,
        definition: col(row, 0).unwrap_or_default().trim().to_string(),
        columns,
        comment: col(row, 1),
        options: Default::default(),
    })
}

/// `ALL_SOURCE` text of a routine as a `CREATE OR REPLACE` statement (a
/// package: specification and body, separated by a `/` line).
pub(crate) fn source(
    conn: &Connection,
    schema: &str,
    name: &str,
    ty: &str,
) -> DbResult<Option<String>> {
    let rows = query(
        conn,
        "SELECT text FROM all_source WHERE owner = :1 AND name = :2 AND type = :3 ORDER BY line",
        &[&schema, &name, &ty],
    )?;
    if rows.is_empty() {
        return Ok(None);
    }
    let text: String = rows
        .iter()
        .filter_map(|r| r.first().and_then(text))
        .collect();
    Ok(Some(format!("CREATE OR REPLACE {}", text.trim_end())))
}

pub(crate) fn routine_definition(conn: &Connection, routine: &ObjectRef) -> DbResult<RoutineDef> {
    let schema = owner(routine.schema.as_deref())?;
    let package = routine.signature.as_deref() == Some("PACKAGE");
    let ty = if package {
        "PACKAGE"
    } else if routine.kind == ObjectKind::Procedure {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    let mut definition = source(conn, &schema, &routine.name, ty)?.ok_or_else(|| {
        DbError::query(format!(
            "{} not found",
            qualified(Some(&schema), &routine.name)
        ))
    })?;
    if package && let Some(body) = source(conn, &schema, &routine.name, "PACKAGE BODY")? {
        definition = format!("{definition}\n/\n{body}");
    }
    let args = query(
        conn,
        "SELECT argument_name, in_out, data_type FROM all_arguments WHERE owner = :1 AND object_name = :2 AND package_name IS NULL AND data_level = 0 ORDER BY position",
        &[&schema, &routine.name.as_str()],
    )
    .unwrap_or_default();
    let mut returns = None;
    let mut arguments = Vec::new();
    for a in &args {
        match col(a, 0) {
            None => returns = col(a, 2),
            Some(name) => arguments.push(format!(
                "{name} {} {}",
                col(a, 1).unwrap_or_default(),
                col(a, 2).unwrap_or_default()
            )),
        }
    }
    Ok(RoutineDef {
        schema: Some(schema),
        name: routine.name.clone(),
        kind: Some(routine.kind),
        arguments: if package {
            String::new()
        } else {
            arguments.join(", ")
        },
        returns,
        language: Some("PL/SQL".into()),
        definition,
        comment: None,
    })
}

pub(crate) fn sequence_definition(conn: &Connection, seq: &ObjectRef) -> DbResult<SequenceDef> {
    let schema = owner(seq.schema.as_deref())?;
    let rows = query(
        conn,
        "SELECT min_value, max_value, increment_by, cycle_flag, cache_size, last_number FROM all_sequences WHERE sequence_owner = :1 AND sequence_name = :2",
        &[&schema, &seq.name.as_str()],
    )?;
    let r = rows.first().ok_or_else(|| {
        DbError::query(format!(
            "Sequence {} not found",
            qualified(Some(&schema), &seq.name)
        ))
    })?;
    let big = |v: Option<&Value>| v.and_then(int);
    Ok(SequenceDef {
        schema: Some(schema),
        name: seq.name.clone(),
        data_type: None,
        start: big(r.get(5)),
        increment: big(r.get(2)),
        min_value: big(r.first()),
        // NOMAXVALUE is 1e28 (no i64).
        max_value: big(r.get(1)),
        cache: big(r.get(4)),
        cycle: col(r, 3).as_deref() == Some("Y"),
        owned_by: None,
        last_value: big(r.get(5)),
        comment: None,
    })
}

/// DBMS_METADATA object type of a navigator object.
pub(crate) fn metadata_type(obj: &ObjectRef) -> Option<&'static str> {
    Some(match obj.kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure if obj.signature.as_deref() == Some("PACKAGE") => "PACKAGE",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Index => "INDEX",
        ObjectKind::Synonym => "SYNONYM",
        ObjectKind::Type => "TYPE",
        ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::CheckConstraint => {
            "CONSTRAINT"
        }
        ObjectKind::ForeignKey => "REF_CONSTRAINT",
        _ => return None,
    })
}

/// `DBMS_METADATA.GET_DDL`, with the dependent DDL of a table (comments,
/// indexes, triggers) when there is any.
pub(crate) fn metadata_ddl(conn: &Connection, obj: &ObjectRef) -> DbResult<String> {
    let ty = metadata_type(obj).ok_or_else(|| DbError::unsupported("DDL of this object"))?;
    let schema = owner(obj.schema.as_deref())?;
    // Without storage clauses, as a script.
    let _ = conn.execute(
        "BEGIN \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
         END;",
        &[],
    );
    let name = obj.name.as_str();
    let rows = query(
        conn,
        "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual",
        &[&ty, &name, &schema],
    )?;
    let mut out = rows
        .first()
        .and_then(|r| col(r, 0))
        .unwrap_or_default()
        .trim()
        .to_string();
    if ty == "TABLE" {
        for dep in ["COMMENT", "INDEX", "TRIGGER"] {
            // ORA-31608: the table has none of them.
            if let Ok(rows) = query(
                conn,
                "SELECT DBMS_METADATA.GET_DEPENDENT_DDL(:1, :2, :3) FROM dual",
                &[&dep, &name, &schema],
            ) && let Some(text) = rows.first().and_then(|r| col(r, 0))
            {
                out.push_str("\n\n");
                out.push_str(text.trim());
            }
        }
    }
    Ok(out)
}

pub(crate) fn search(conn: &Connection, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
    let raw = search.pattern.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut pattern = String::new();
    let wildcard = raw.contains('*') || raw.contains('?');
    if !wildcard {
        pattern.push('%');
    }
    for c in raw.chars() {
        match c {
            '*' => pattern.push('%'),
            '?' => pattern.push('_'),
            '%' | '_' | '\\' => {
                pattern.push('\\');
                pattern.push(c);
            }
            c => pattern.push(c),
        }
    }
    if !wildcard {
        pattern.push('%');
    }
    let (expr, pattern) = if search.case_sensitive {
        ("o.object_name", pattern)
    } else {
        ("UPPER(o.object_name)", pattern.to_uppercase())
    };
    let mut types: Vec<&str> = Vec::new();
    for k in &search.kinds {
        match k {
            ObjectKind::Table => types.push("'TABLE'"),
            ObjectKind::View => types.push("'VIEW'"),
            ObjectKind::MaterializedView => types.push("'MATERIALIZED VIEW'"),
            ObjectKind::Sequence => types.push("'SEQUENCE'"),
            ObjectKind::Function => types.push("'FUNCTION'"),
            ObjectKind::Procedure => types.extend(["'PROCEDURE'", "'PACKAGE'"]),
            ObjectKind::Trigger => types.push("'TRIGGER'"),
            ObjectKind::Index => types.push("'INDEX'"),
            ObjectKind::Type => types.push("'TYPE'"),
            ObjectKind::Synonym => types.push("'SYNONYM'"),
            _ => {}
        }
    }
    let owners = if search.schemas.is_empty() {
        "o.owner NOT IN (SELECT username FROM all_users WHERE oracle_maintained = 'Y')".to_string()
    } else {
        format!(
            "o.owner IN ({})",
            search
                .schemas
                .iter()
                .map(|s| format!("'{}'", s.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let limit = search.limit.max(1);
    let mut out = Vec::new();
    if !types.is_empty() {
        let sql = format!(
            "SELECT o.owner, o.object_name, o.object_type FROM all_objects o WHERE o.object_type IN ({}) AND {expr} LIKE :1 ESCAPE '\\' AND {owners} \
             ORDER BY o.object_name FETCH FIRST {limit} ROWS ONLY",
            types.join(", ")
        );
        for r in query(conn, &sql, &[&pattern])? {
            let (Some(owner), Some(name), Some(ty)) = (col(&r, 0), col(&r, 1), col(&r, 2)) else {
                continue;
            };
            let kind = match ty.as_str() {
                "TABLE" => ObjectKind::Table,
                "VIEW" => ObjectKind::View,
                "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
                "SEQUENCE" => ObjectKind::Sequence,
                "FUNCTION" => ObjectKind::Function,
                "PROCEDURE" | "PACKAGE" => ObjectKind::Procedure,
                "TRIGGER" => ObjectKind::Trigger,
                "INDEX" => ObjectKind::Index,
                "TYPE" => ObjectKind::Type,
                _ => ObjectKind::Synonym,
            };
            let mut obj = ObjectRef::new(kind, Some(owner), name);
            if ty == "PACKAGE" {
                obj.signature = Some("PACKAGE".into());
            }
            out.push(ObjectInfo::new(obj));
        }
    }
    if search.kinds.contains(&ObjectKind::Column) && out.len() < limit {
        let expr = if search.case_sensitive {
            "c.column_name"
        } else {
            "UPPER(c.column_name)"
        };
        let sql = format!(
            "SELECT c.owner, c.table_name, c.column_name, c.data_type FROM all_tab_columns c \
             WHERE {expr} LIKE :1 ESCAPE '\\' AND {} ORDER BY c.column_name FETCH FIRST {} ROWS ONLY",
            owners.replace("o.owner", "c.owner"),
            limit - out.len()
        );
        for r in query(conn, &sql, &[&pattern])? {
            let (Some(owner), Some(table), Some(name)) = (col(&r, 0), col(&r, 1), col(&r, 2))
            else {
                continue;
            };
            let parent = ObjectRef::table(Some(owner), table);
            let mut info = ObjectInfo::new(ObjectRef::child(ObjectKind::Column, &parent, name));
            info.detail = col(&r, 3);
            out.push(info);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_types_read_back_as_written() {
        let row = |t: &str, len: i64, chars: i64, used: &str, p: Option<i64>, s: Option<i64>| {
            vec![
                Value::Text(t.into()),
                Value::Int(len),
                Value::Int(chars),
                Value::Text(used.into()),
                p.map_or(Value::Null, Value::Int),
                s.map_or(Value::Null, Value::Int),
            ]
        };
        assert_eq!(
            type_text(&row("VARCHAR2", 400, 100, "C", None, None), 0),
            "VARCHAR2(100 CHAR)"
        );
        assert_eq!(
            type_text(&row("VARCHAR2", 100, 100, "B", None, None), 0),
            "VARCHAR2(100)"
        );
        assert_eq!(
            type_text(&row("NUMBER", 22, 0, "", Some(10), Some(2)), 0),
            "NUMBER(10,2)"
        );
        assert_eq!(
            type_text(&row("NUMBER", 22, 0, "", Some(10), Some(0)), 0),
            "NUMBER(10)"
        );
        assert_eq!(
            type_text(&row("NUMBER", 22, 0, "", None, Some(0)), 0),
            "INTEGER"
        );
        assert_eq!(
            type_text(&row("NUMBER", 22, 0, "", None, None), 0),
            "NUMBER"
        );
        assert_eq!(type_text(&row("RAW", 16, 0, "", None, None), 0), "RAW(16)");
        assert_eq!(
            type_text(&row("TIMESTAMP(6)", 11, 0, "", None, Some(6)), 0),
            "TIMESTAMP(6)"
        );
        assert!(is_not_null_check(Some("\"ID\" IS NOT NULL")));
        assert!(!is_not_null_check(Some("price > 0")));
        assert_eq!(kind_of("NUMBER(10)"), DataKind::Int);
        assert_eq!(kind_of("NUMBER(10,2)"), DataKind::Decimal);
        assert_eq!(kind_of("TIMESTAMP(6) WITH TIME ZONE"), DataKind::DateTimeTz);
        assert_eq!(kind_of("DATE"), DataKind::DateTime);
    }
}
