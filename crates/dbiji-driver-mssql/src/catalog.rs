//! Metadata access through the SQL Server catalog views.
//!
//! Every query is qualified with the target database (`[db].sys.tables`),
//! so objects of any database on the server can be read without changing
//! the session's current database. Database scoped functions such as
//! `SCHEMA_NAME()` or `OBJECT_DEFINITION()` are avoided for that reason;
//! the queries join the catalog views instead.

use crate::ddl::{MsSqlDdl, default_constraint_key, identity_key};
use crate::dialect::{nstring, qualified, quote};
use crate::session::SqlServerSession;
use crate::sqltext::{strip_outer_parens, view_select};
use crate::types::kind_of_type_name;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{DbError, DbKind, DbResult, Row, Value};
use std::collections::BTreeMap;

// ------------------------------------------------------------ value helpers

fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_text().into_owned(),
    }
}

fn opt(v: Option<&Value>) -> Option<String> {
    Some(text(v)).filter(|s| !s.is_empty())
}

fn int(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64)
}

fn truthy(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool).unwrap_or(false)
}

/// `[db].` prefix for catalog views (empty for the current database).
fn dbp(db: Option<&str>) -> String {
    match db {
        Some(d) if !d.is_empty() => format!("{}.", quote(d)),
        _ => String::new(),
    }
}

fn schema_name(schema: Option<&str>) -> String {
    schema.filter(|s| !s.is_empty()).unwrap_or("dbo").to_string()
}

const MS_DESC: &str = "N'MS_Description'";

/// Full type text from catalog columns.
pub fn format_type(type_name: &str, type_schema: &str, is_user_defined: bool, max_length: i64, precision: i64, scale: i64) -> String {
    if is_user_defined {
        return if type_schema.is_empty() || type_schema == "sys" { quote(type_name) } else { format!("{}.{}", quote(type_schema), quote(type_name)) };
    }
    let lower = type_name.to_ascii_lowercase();
    let len = |divide: i64| -> String { if max_length == -1 { "max".into() } else { (max_length / divide).to_string() } };
    match lower.as_str() {
        "varchar" | "char" | "varbinary" | "binary" => format!("{lower}({})", len(1)),
        "nvarchar" | "nchar" => format!("{lower}({})", len(2)),
        "decimal" | "numeric" => format!("{lower}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{lower}({scale})"),
        "float" if precision > 0 && precision != 53 => format!("float({precision})"),
        _ => lower,
    }
}

fn type_from_row(r: &Row, at: usize) -> String {
    // at: type name, type schema, is_user_defined, max_length, precision, scale
    format_type(&text(r.get(at)), &text(r.get(at + 1)), truthy(r.get(at + 2)), int(r.get(at + 3)).unwrap_or(0), int(r.get(at + 4)).unwrap_or(0), int(r.get(at + 5)).unwrap_or(0))
}

fn kind_for_type_code(code: &str) -> Option<ObjectKind> {
    Some(match code.trim() {
        "U" => ObjectKind::Table,
        "V" => ObjectKind::View,
        "P" | "PC" | "X" | "RF" => ObjectKind::Procedure,
        "FN" | "IF" | "TF" | "FS" | "FT" => ObjectKind::Function,
        "AF" => ObjectKind::Aggregate,
        "TR" | "TA" => ObjectKind::Trigger,
        "SO" => ObjectKind::Sequence,
        "SN" => ObjectKind::Synonym,
        _ => return None,
    })
}

fn type_codes(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "'U'",
        ObjectKind::View => "'V'",
        ObjectKind::Procedure => "'P', 'PC', 'X', 'RF'",
        ObjectKind::Function => "'FN', 'IF', 'TF', 'FS', 'FT'",
        ObjectKind::Aggregate => "'AF'",
        ObjectKind::Trigger => "'TR', 'TA'",
        ObjectKind::Sequence => "'SO'",
        ObjectKind::Synonym => "'SN'",
        _ => "",
    }
}

fn routine_detail(code: &str) -> &'static str {
    match code.trim() {
        "P" => "procedure",
        "PC" => "CLR procedure",
        "X" => "extended procedure",
        "RF" => "replication filter",
        "FN" => "scalar function",
        "IF" => "inline table-valued function",
        "TF" => "table-valued function",
        "FS" => "CLR scalar function",
        "FT" => "CLR table-valued function",
        "AF" => "CLR aggregate",
        _ => "",
    }
}

/// Parameters signature of routines (`@a int, @b nvarchar`).
fn args_subquery(db: &str) -> String {
    format!(
        "STUFF((SELECT N', ' + pa.name + N' ' + ty.name + CASE WHEN pa.is_output = 1 THEN N' OUTPUT' ELSE N'' END \
         FROM {db}sys.parameters pa JOIN {db}sys.types ty ON ty.user_type_id = pa.user_type_id \
         WHERE pa.object_id = o.object_id AND pa.parameter_id > 0 ORDER BY pa.parameter_id \
         FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')"
    )
}

// ------------------------------------------------------------ navigator

pub async fn list_databases(s: &mut SqlServerSession) -> DbResult<Vec<DatabaseInfo>> {
    let rows = s
        .rows(
            "SELECT d.name, CAST(CASE WHEN d.database_id <= 4 THEN 1 ELSE 0 END AS bit), SUSER_SNAME(d.owner_sid), \
             d.collation_name, d.state_desc, CAST(CASE WHEN d.name = DB_NAME() THEN 1 ELSE 0 END AS bit) \
             FROM sys.databases d ORDER BY d.name",
            &[],
        )
        .await?;
    // sizes need sys.master_files, which is unavailable on Azure SQL Database
    let sizes: BTreeMap<String, i64> = if s.is_azure_db() {
        BTreeMap::new()
    } else {
        s.rows("SELECT DB_NAME(database_id), SUM(CAST(size AS bigint)) * 8192 FROM sys.master_files GROUP BY database_id", &[])
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| Some((text(r.first()), int(r.get(1))?)))
            .collect()
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = text(r.first());
            let state = text(r.get(4));
            DatabaseInfo {
                is_current: truthy(r.get(5)),
                is_system: truthy(r.get(1)),
                owner: opt(r.get(2)),
                comment: if state.is_empty() || state == "ONLINE" { None } else { Some(state) },
                encoding: opt(r.get(3)),
                size_bytes: sizes.get(&name).copied(),
                name,
            }
        })
        .collect())
}

pub async fn list_schemas(s: &mut SqlServerSession, db: Option<&str>) -> DbResult<Vec<SchemaInfo>> {
    let d = dbp(db);
    let rows = s
        .rows(
            &format!(
                "SELECT sc.name, CAST(CASE WHEN sc.name IN ('sys', 'INFORMATION_SCHEMA', 'guest') OR sc.schema_id >= 16384 THEN 1 ELSE 0 END AS bit), \
                 p.name, CAST(ep.value AS nvarchar(max)), CAST(CASE WHEN sc.name = SCHEMA_NAME() THEN 1 ELSE 0 END AS bit) \
                 FROM {d}sys.schemas sc \
                 LEFT JOIN {d}sys.database_principals p ON p.principal_id = sc.principal_id \
                 LEFT JOIN {d}sys.extended_properties ep ON ep.class = 3 AND ep.major_id = sc.schema_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                 ORDER BY sc.name"
            ),
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| SchemaInfo { name: text(r.first()), is_system: truthy(r.get(1)), owner: opt(r.get(2)), comment: opt(r.get(3)), is_current: truthy(r.get(4)) && db.is_none() })
        .collect())
}

pub async fn list_objects(s: &mut SqlServerSession, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
    let d = dbp(schema.database.as_deref());
    let sc = schema_name(schema.schema.as_deref());
    let params = [Value::text(sc.clone())];
    let mk = |kind: ObjectKind, name: String| ObjectRef::new(kind, Some(sc.clone()), name).with_database(schema.database.clone());
    match kind {
        ObjectKind::Table | ObjectKind::View => {
            let view = kind == ObjectKind::View;
            let src = if view { "sys.views" } else { "sys.tables" };
            let rows_expr =
                if view { "NULL".to_string() } else { format!("(SELECT SUM(p.rows) FROM {d}sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1))") };
            let rows = s
                .rows(
                    &format!(
                        "SELECT t.name, CAST(ep.value AS nvarchar(max)), t.is_ms_shipped, {rows_expr} \
                         FROM {d}{src} t JOIN {d}sys.schemas sc ON sc.schema_id = t.schema_id \
                         LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = t.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                         WHERE sc.name = @P1 ORDER BY t.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let mut info = ObjectInfo::new(mk(kind, text(r.first()))).with_comment(opt(r.get(1)));
                    info.is_system = truthy(r.get(2));
                    info.row_estimate = int(r.get(3));
                    info
                })
                .collect())
        }
        ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Aggregate => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT o.name, CAST(ep.value AS nvarchar(max)), o.is_ms_shipped, o.type, {} \
                         FROM {d}sys.objects o JOIN {d}sys.schemas sc ON sc.schema_id = o.schema_id \
                         LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                         WHERE sc.name = @P1 AND o.type IN ({}) ORDER BY o.name",
                        args_subquery(&d),
                        type_codes(kind)
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let args = text(r.get(4));
                    let mut info = ObjectInfo::new(mk(kind, text(r.first())).with_signature(format!("({args})")))
                        .with_comment(opt(r.get(1)))
                        .with_detail(format!("{} ({args})", routine_detail(&text(r.get(3)))));
                    info.is_system = truthy(r.get(2));
                    info
                })
                .collect())
        }
        ObjectKind::Sequence => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT sq.name, ty.name, TRY_CAST(sq.increment AS bigint), CAST(ep.value AS nvarchar(max)) \
                         FROM {d}sys.sequences sq JOIN {d}sys.schemas sc ON sc.schema_id = sq.schema_id \
                         JOIN {d}sys.types ty ON ty.user_type_id = sq.user_type_id \
                         LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = sq.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                         WHERE sc.name = @P1 ORDER BY sq.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| ObjectInfo::new(mk(kind, text(r.first()))).with_detail(format!("{} increment {}", text(r.get(1)), text(r.get(2)))).with_comment(opt(r.get(3))))
                .collect())
        }
        ObjectKind::Synonym => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT sy.name, sy.base_object_name FROM {d}sys.synonyms sy \
                         JOIN {d}sys.schemas sc ON sc.schema_id = sy.schema_id WHERE sc.name = @P1 ORDER BY sy.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows.into_iter().map(|r| ObjectInfo::new(mk(kind, text(r.first()))).with_detail(text(r.get(1)))).collect())
        }
        ObjectKind::Type => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT ty.name, ty.is_table_type, bt.name, ty.max_length, ty.precision, ty.scale, ty.is_nullable, ty.is_assembly_type \
                         FROM {d}sys.types ty JOIN {d}sys.schemas sc ON sc.schema_id = ty.schema_id \
                         LEFT JOIN {d}sys.types bt ON bt.user_type_id = ty.system_type_id AND ty.is_table_type = 0 AND ty.is_assembly_type = 0 \
                         WHERE ty.is_user_defined = 1 AND sc.name = @P1 ORDER BY ty.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let detail = if truthy(r.get(1)) {
                        "table type".to_string()
                    } else if truthy(r.get(7)) {
                        "CLR type".to_string()
                    } else {
                        format!("alias of {}", format_type(&text(r.get(2)), "sys", false, int(r.get(3)).unwrap_or(0), int(r.get(4)).unwrap_or(0), int(r.get(5)).unwrap_or(0)))
                    };
                    ObjectInfo::new(mk(kind, text(r.first()))).with_detail(detail)
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// `WHERE` part selecting the parent object by schema and name.
const PARENT_FILTER: &str = "ps.name = @P1 AND po.name = @P2";

fn parent_params(parent: &ObjectRef) -> Vec<Value> {
    vec![Value::text(schema_name(parent.schema.as_deref())), Value::text(parent.name.clone())]
}

pub async fn list_children(s: &mut SqlServerSession, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
    let d = dbp(parent.database.as_deref());
    let params = parent_params(parent);
    match kind {
        ObjectKind::Column => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT c.name, ty.name, ts.name, ty.is_user_defined, c.max_length, c.precision, c.scale, \
                         c.is_nullable, c.is_identity, c.is_computed, CAST(ep.value AS nvarchar(max)) \
                         FROM {d}sys.columns c JOIN {d}sys.objects po ON po.object_id = c.object_id \
                         JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id \
                         JOIN {d}sys.types ty ON ty.user_type_id = c.user_type_id \
                         JOIN {d}sys.schemas ts ON ts.schema_id = ty.schema_id \
                         LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = {MS_DESC} \
                         WHERE {PARENT_FILTER} ORDER BY c.column_id"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let mut detail = type_from_row(&r, 1);
                    detail.push_str(if truthy(r.get(7)) { " NULL" } else { " NOT NULL" });
                    if truthy(r.get(8)) {
                        detail.push_str(" IDENTITY");
                    }
                    if truthy(r.get(9)) {
                        detail.push_str(" (computed)");
                    }
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, text(r.first()))).with_detail(detail).with_comment(opt(r.get(10)))
                })
                .collect())
        }
        ObjectKind::Index => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT i.name, i.type_desc, i.is_unique, i.filter_definition, i.is_disabled, \
                         STUFF((SELECT N', ' + QUOTENAME(col.name) + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END \
                           FROM {d}sys.index_columns ic JOIN {d}sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                           WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0 \
                           ORDER BY ic.key_ordinal, ic.index_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N''), \
                         STUFF((SELECT N', ' + QUOTENAME(col.name) \
                           FROM {d}sys.index_columns ic JOIN {d}sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                           WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 1 \
                           ORDER BY ic.index_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'') \
                         FROM {d}sys.indexes i JOIN {d}sys.objects po ON po.object_id = i.object_id \
                         JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id \
                         WHERE {PARENT_FILTER} AND i.type > 0 AND i.is_hypothetical = 0 ORDER BY i.index_id"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let mut detail = text(r.get(1));
                    if truthy(r.get(2)) {
                        detail.push_str(" UNIQUE");
                    }
                    detail.push_str(&format!(" ({})", text(r.get(5))));
                    if let Some(inc) = opt(r.get(6)) {
                        detail.push_str(&format!(" INCLUDE ({inc})"));
                    }
                    if let Some(f) = opt(r.get(3)) {
                        detail.push_str(&format!(" WHERE {}", strip_outer_parens(&f)));
                    }
                    if truthy(r.get(4)) {
                        detail.push_str(" [disabled]");
                    }
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Index, parent, text(r.first()))).with_detail(detail)
                })
                .collect())
        }
        ObjectKind::PrimaryKey | ObjectKind::UniqueKey => {
            let ty = if kind == ObjectKind::PrimaryKey { "PK" } else { "UQ" };
            let rows = s
                .rows(
                    &format!(
                        "SELECT kc.name, i.type_desc, \
                         STUFF((SELECT N', ' + QUOTENAME(col.name) FROM {d}sys.index_columns ic \
                           JOIN {d}sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                           WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 \
                           ORDER BY ic.key_ordinal FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'') \
                         FROM {d}sys.key_constraints kc JOIN {d}sys.objects po ON po.object_id = kc.parent_object_id \
                         JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id \
                         JOIN {d}sys.indexes i ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
                         WHERE {PARENT_FILTER} AND kc.type = '{ty}' ORDER BY kc.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows.into_iter().map(|r| ObjectInfo::new(ObjectRef::child(kind, parent, text(r.first()))).with_detail(format!("{} ({})", text(r.get(1)), text(r.get(2))))).collect())
        }
        ObjectKind::ForeignKey => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT fk.name, rs.name, rt.name, fk.delete_referential_action_desc, fk.update_referential_action_desc, fk.is_disabled, \
                         STUFF((SELECT N', ' + QUOTENAME(pc.name) FROM {d}sys.foreign_key_columns fkc \
                           JOIN {d}sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                           WHERE fkc.constraint_object_id = fk.object_id ORDER BY fkc.constraint_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N''), \
                         STUFF((SELECT N', ' + QUOTENAME(rc.name) FROM {d}sys.foreign_key_columns fkc \
                           JOIN {d}sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                           WHERE fkc.constraint_object_id = fk.object_id ORDER BY fkc.constraint_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'') \
                         FROM {d}sys.foreign_keys fk JOIN {d}sys.objects po ON po.object_id = fk.parent_object_id \
                         JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id \
                         JOIN {d}sys.objects rt ON rt.object_id = fk.referenced_object_id \
                         JOIN {d}sys.schemas rs ON rs.schema_id = rt.schema_id \
                         WHERE {PARENT_FILTER} ORDER BY fk.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let mut detail = format!("({}) → {}.{} ({})", text(r.get(6)), text(r.get(1)), text(r.get(2)), text(r.get(7)));
                    let del = text(r.get(3));
                    if del != "NO_ACTION" && !del.is_empty() {
                        detail.push_str(&format!(" ON DELETE {}", del.replace('_', " ")));
                    }
                    if truthy(r.get(5)) {
                        detail.push_str(" [disabled]");
                    }
                    ObjectInfo::new(ObjectRef::child(ObjectKind::ForeignKey, parent, text(r.first()))).with_detail(detail)
                })
                .collect())
        }
        ObjectKind::CheckConstraint => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT cc.name, cc.definition, cc.is_disabled FROM {d}sys.check_constraints cc \
                         JOIN {d}sys.objects po ON po.object_id = cc.parent_object_id JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id \
                         WHERE {PARENT_FILTER} ORDER BY cc.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| ObjectInfo::new(ObjectRef::child(ObjectKind::CheckConstraint, parent, text(r.first()))).with_detail(strip_outer_parens(&text(r.get(1)))))
                .collect())
        }
        ObjectKind::Trigger => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT tr.name, tr.is_instead_of_trigger, tr.is_disabled, \
                         STUFF((SELECT N', ' + te.type_desc FROM {d}sys.trigger_events te WHERE te.object_id = tr.object_id \
                           FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'') \
                         FROM {d}sys.triggers tr JOIN {d}sys.objects po ON po.object_id = tr.parent_id \
                         JOIN {d}sys.schemas ps ON ps.schema_id = po.schema_id WHERE {PARENT_FILTER} ORDER BY tr.name"
                    ),
                    &params,
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let timing = if truthy(r.get(1)) { "INSTEAD OF" } else { "AFTER" };
                    let mut detail = format!("{timing} {}", text(r.get(3)));
                    if truthy(r.get(2)) {
                        detail.push_str(" [disabled]");
                    }
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Trigger, parent, text(r.first()))).with_detail(detail)
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

// ------------------------------------------------------------ table loader

/// Which objects the loader reads.
struct Scope {
    db: String,
    schema: String,
    table: Option<String>,
    /// Object type codes for the column query (`'U'` or `'V'` ...).
    object_types: &'static str,
}

impl Scope {
    fn params(&self) -> Vec<Value> {
        let mut p = vec![Value::text(self.schema.clone())];
        if let Some(t) = &self.table {
            p.push(Value::text(t.clone()));
        }
        p
    }

    /// Filter on alias `t` (object) joined with schema alias `s`.
    fn filter(&self) -> String {
        match self.table {
            Some(_) => "s.name = @P1 AND t.name = @P2".to_string(),
            None => "s.name = @P1".to_string(),
        }
    }
}

async fn database_collation(s: &mut SqlServerSession, db: Option<&str>) -> Option<String> {
    let v = match db {
        Some(d) => s.scalar("SELECT collation_name FROM sys.databases WHERE name = @P1", &[Value::text(d)]).await,
        None => s.scalar("SELECT CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128))", &[]).await,
    };
    v.ok().and_then(|v| v.as_str().map(str::to_string))
}

/// Load columns of the objects in scope, keyed by object id. Also returns
/// per-object options (default constraint names, identity specs).
async fn load_columns(s: &mut SqlServerSession, scope: &Scope, db_collation: Option<&str>) -> DbResult<BTreeMap<i64, (Vec<ColumnDef>, BTreeMap<String, String>)>> {
    let d = &scope.db;
    let sql = format!(
        "SELECT c.object_id, c.column_id, c.name, ty.name, ts.name, ty.is_user_defined, c.max_length, c.precision, c.scale, \
         c.is_nullable, c.is_identity, TRY_CAST(ic.seed_value AS bigint), TRY_CAST(ic.increment_value AS bigint), \
         cc.definition, cc.is_persisted, dc.name, dc.definition, c.collation_name, CAST(ep.value AS nvarchar(max)), bt.name \
         FROM {d}sys.columns c \
         JOIN {d}sys.objects t ON t.object_id = c.object_id \
         JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
         JOIN {d}sys.types ty ON ty.user_type_id = c.user_type_id \
         JOIN {d}sys.schemas ts ON ts.schema_id = ty.schema_id \
         LEFT JOIN {d}sys.types bt ON bt.user_type_id = ty.system_type_id AND ty.is_user_defined = 1 \
         LEFT JOIN {d}sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         LEFT JOIN {d}sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN {d}sys.default_constraints dc ON dc.object_id = c.default_object_id \
         LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = {MS_DESC} \
         WHERE {} AND t.type IN ({}) ORDER BY c.object_id, c.column_id",
        scope.filter(),
        scope.object_types
    );
    let rows = s.rows(&sql, &scope.params()).await?;
    let mut out: BTreeMap<i64, (Vec<ColumnDef>, BTreeMap<String, String>)> = BTreeMap::new();
    for r in rows {
        let Some(oid) = int(r.first()) else { continue };
        let name = text(r.get(2));
        let data_type = type_from_row(&r, 3);
        let base = opt(r.get(19)).unwrap_or_else(|| text(r.get(3)));
        let mut c = ColumnDef::new(name.clone(), data_type);
        c.kind = kind_of_type_name(&base);
        c.nullable = truthy(r.get(9));
        c.auto_increment = truthy(r.get(10));
        c.ordinal = int(r.get(1)).unwrap_or(0).max(0) as u32;
        let entry = out.entry(oid).or_default();
        if c.auto_increment {
            let seed = int(r.get(11)).unwrap_or(1);
            let inc = int(r.get(12)).unwrap_or(1);
            if (seed, inc) != (1, 1) {
                entry.1.insert(identity_key(&name), format!("{seed},{inc}"));
            }
        }
        if let Some(def) = opt(r.get(13)) {
            let mut g = strip_outer_parens(&def);
            if truthy(r.get(14)) {
                g.push_str(" PERSISTED");
            }
            c.generated = Some(g);
        }
        if let Some(dname) = opt(r.get(15)) {
            entry.1.insert(default_constraint_key(&name), dname);
        }
        c.default = opt(r.get(16)).map(|d| strip_outer_parens(&d));
        let collation = opt(r.get(17));
        if collation.as_deref() != db_collation {
            c.collation = collation;
        }
        c.comment = opt(r.get(18));
        entry.0.push(c);
    }
    Ok(out)
}

/// Batched loader of table definitions for a schema (or one table).
async fn load_tables(s: &mut SqlServerSession, db: Option<&str>, schema: &str, table: Option<&str>, opts: &SnapshotOptions) -> DbResult<Vec<TableDef>> {
    let scope = Scope { db: dbp(db), schema: schema.to_string(), table: table.map(str::to_string), object_types: "'U'" };
    let d = scope.db.clone();
    let filter = scope.filter();
    let params = scope.params();

    let rows = s
        .rows(
            &format!(
                "SELECT t.object_id, t.name, CAST(ep.value AS nvarchar(max)), \
                 (SELECT SUM(p.rows) FROM {d}sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)), \
                 (SELECT SUM(a.total_pages) * 8192 FROM {d}sys.partitions p JOIN {d}sys.allocation_units a ON a.container_id = p.partition_id WHERE p.object_id = t.object_id), \
                 ds.name, (SELECT TOP 1 p.data_compression_desc FROM {d}sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)) \
                 FROM {d}sys.tables t JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
                 LEFT JOIN {d}sys.indexes hi ON hi.object_id = t.object_id AND hi.index_id IN (0, 1) \
                 LEFT JOIN {d}sys.data_spaces ds ON ds.data_space_id = hi.data_space_id \
                 LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = t.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                 WHERE {filter} ORDER BY t.name"
            ),
            &params,
        )
        .await?;
    let mut tables: BTreeMap<i64, TableDef> = BTreeMap::new();
    let mut order: Vec<i64> = Vec::new();
    for r in rows {
        let Some(oid) = int(r.first()) else { continue };
        let mut t = TableDef::new(Some(schema.to_string()), text(r.get(1)));
        if opts.comments {
            t.comment = opt(r.get(2));
        }
        t.row_estimate = int(r.get(3));
        t.size_bytes = int(r.get(4));
        if let Some(fg) = opt(r.get(5)).filter(|f| f != "PRIMARY") {
            t.options.insert("filegroup".into(), fg);
        }
        if let Some(dc) = opt(r.get(6)).filter(|c| c != "NONE") {
            t.options.insert("data_compression".into(), dc);
        }
        order.push(oid);
        tables.insert(oid, t);
    }
    if tables.is_empty() {
        return Ok(Vec::new());
    }

    if opts.columns {
        let collation = database_collation(s, db).await;
        for (oid, (cols, options)) in load_columns(s, &scope, collation.as_deref()).await? {
            if let Some(t) = tables.get_mut(&oid) {
                t.columns = cols;
                if !opts.comments {
                    for c in &mut t.columns {
                        c.comment = None;
                    }
                }
                t.options.extend(options);
            }
        }
    }

    if opts.constraints {
        // primary / unique keys
        let rows = s
            .rows(
                &format!(
                    "SELECT kc.parent_object_id, kc.name, kc.type, col.name \
                     FROM {d}sys.key_constraints kc \
                     JOIN {d}sys.tables t ON t.object_id = kc.parent_object_id \
                     JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
                     JOIN {d}sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id AND ic.key_ordinal > 0 \
                     JOIN {d}sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                     WHERE {filter} ORDER BY kc.parent_object_id, kc.name, ic.key_ordinal"
                ),
                &params,
            )
            .await?;
        for r in rows {
            let Some(t) = int(r.first()).and_then(|o| tables.get_mut(&o)) else { continue };
            let name = text(r.get(1));
            let col = text(r.get(3));
            if text(r.get(2)).trim() == "PK" {
                let pk = t.primary_key.get_or_insert_with(|| KeyDef { name: Some(name.clone()), columns: Vec::new() });
                pk.columns.push(col);
            } else {
                match t.unique_keys.iter_mut().find(|u| u.name.as_deref() == Some(name.as_str())) {
                    Some(u) => u.columns.push(col),
                    None => t.unique_keys.push(KeyDef { name: Some(name), columns: vec![col] }),
                }
            }
        }
        // checks
        let rows = s
            .rows(
                &format!(
                    "SELECT cc.parent_object_id, cc.name, cc.definition FROM {d}sys.check_constraints cc \
                     JOIN {d}sys.tables t ON t.object_id = cc.parent_object_id JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
                     WHERE {filter} ORDER BY cc.parent_object_id, cc.name"
                ),
                &params,
            )
            .await?;
        for r in rows {
            if let Some(t) = int(r.first()).and_then(|o| tables.get_mut(&o)) {
                t.checks.push(CheckDef { name: opt(r.get(1)), expression: strip_outer_parens(&text(r.get(2))) });
            }
        }
        // foreign keys (owned and referencing)
        let (pf, rf) = match scope.table {
            Some(_) => ("ps.name = @P1 AND pt.name = @P2", "rs.name = @P1 AND rt.name = @P2"),
            None => ("ps.name = @P1", "rs.name = @P1"),
        };
        let rows = s
            .rows(
                &format!(
                    "SELECT fk.object_id, fk.name, fk.parent_object_id, ps.name, pt.name, fk.referenced_object_id, rs.name, rt.name, \
                     fk.delete_referential_action_desc, fk.update_referential_action_desc, pc.name, rc.name \
                     FROM {d}sys.foreign_keys fk \
                     JOIN {d}sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN {d}sys.tables pt ON pt.object_id = fk.parent_object_id \
                     JOIN {d}sys.schemas ps ON ps.schema_id = pt.schema_id \
                     JOIN {d}sys.tables rt ON rt.object_id = fk.referenced_object_id \
                     JOIN {d}sys.schemas rs ON rs.schema_id = rt.schema_id \
                     JOIN {d}sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                     JOIN {d}sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                     WHERE ({pf}) OR ({rf}) ORDER BY fk.object_id, fkc.constraint_column_id"
                ),
                &params,
            )
            .await?;
        let mut fks: Vec<(i64, i64, i64, ForeignKeyDef)> = Vec::new();
        for r in rows {
            let (Some(fk_id), Some(parent), Some(referenced)) = (int(r.first()), int(r.get(2)), int(r.get(5))) else {
                continue;
            };
            match fks.iter_mut().find(|(id, ..)| *id == fk_id) {
                Some((_, _, _, fk)) => {
                    fk.columns.push(text(r.get(10)));
                    fk.ref_columns.push(text(r.get(11)));
                }
                None => fks.push((
                    fk_id,
                    parent,
                    referenced,
                    ForeignKeyDef {
                        name: opt(r.get(1)),
                        schema: opt(r.get(3)),
                        table: text(r.get(4)),
                        columns: vec![text(r.get(10))],
                        ref_schema: opt(r.get(6)),
                        ref_table: text(r.get(7)),
                        ref_columns: vec![text(r.get(11))],
                        on_delete: FkAction::parse(&text(r.get(8))),
                        on_update: FkAction::parse(&text(r.get(9))),
                        deferrable: false,
                    },
                )),
            }
        }
        for (_, parent, referenced, fk) in fks {
            if let Some(t) = tables.get_mut(&parent) {
                t.foreign_keys.push(fk.clone());
            }
            if let Some(t) = tables.get_mut(&referenced) {
                t.referenced_by.push(fk);
            }
        }
    }

    if opts.indexes || opts.constraints {
        let rows = s
            .rows(
                &format!(
                    "SELECT i.object_id, i.index_id, i.name, i.type_desc, i.is_unique, i.is_primary_key, i.is_unique_constraint, \
                     i.filter_definition, col.name, ic.key_ordinal, ic.is_included_column, ic.is_descending_key, CAST(ep.value AS nvarchar(max)) \
                     FROM {d}sys.indexes i \
                     JOIN {d}sys.tables t ON t.object_id = i.object_id \
                     JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
                     LEFT JOIN {d}sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     LEFT JOIN {d}sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
                     LEFT JOIN {d}sys.extended_properties ep ON ep.class = 7 AND ep.major_id = i.object_id AND ep.minor_id = i.index_id AND ep.name = {MS_DESC} \
                     WHERE {filter} AND i.type > 0 AND i.is_hypothetical = 0 \
                     ORDER BY i.object_id, i.index_id, ic.is_included_column, CASE WHEN ic.key_ordinal = 0 THEN 1 ELSE 0 END, ic.key_ordinal, ic.index_column_id"
                ),
                &params,
            )
            .await?;
        let mut current: Option<(i64, i64)> = None;
        for r in rows {
            let (Some(oid), Some(iid)) = (int(r.first()), int(r.get(1))) else { continue };
            let Some(t) = tables.get_mut(&oid) else { continue };
            if current != Some((oid, iid)) {
                current = Some((oid, iid));
                t.indexes.push(IndexDef {
                    name: text(r.get(2)),
                    unique: truthy(r.get(4)),
                    primary: truthy(r.get(5)),
                    method: opt(r.get(3)),
                    predicate: opt(r.get(7)).map(|p| strip_outer_parens(&p)),
                    comment: if opts.comments { opt(r.get(12)) } else { None },
                    ..Default::default()
                });
            }
            let Some(idx) = t.indexes.last_mut() else { continue };
            let Some(col) = opt(r.get(8)) else { continue };
            if truthy(r.get(10)) {
                idx.include.push(col);
            } else {
                idx.columns.push(IndexColumn { name: col, is_expression: false, descending: truthy(r.get(11)) });
            }
        }
        if !opts.indexes {
            // keep only indexes backing constraints (needed for clustered-ness)
            for t in tables.values_mut() {
                let uq: Vec<String> = t.unique_keys.iter().filter_map(|u| u.name.clone()).collect();
                t.indexes.retain(|i| i.primary || uq.contains(&i.name));
            }
        }
    }

    if opts.triggers {
        let rows = s
            .rows(
                &format!(
                    "SELECT tr.parent_id, tr.name, tr.is_disabled, tr.is_instead_of_trigger, m.definition, \
                     STUFF((SELECT N',' + te.type_desc FROM {d}sys.trigger_events te WHERE te.object_id = tr.object_id \
                       FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 1, N''), CAST(ep.value AS nvarchar(max)) \
                     FROM {d}sys.triggers tr JOIN {d}sys.tables t ON t.object_id = tr.parent_id \
                     JOIN {d}sys.schemas s ON s.schema_id = t.schema_id \
                     LEFT JOIN {d}sys.sql_modules m ON m.object_id = tr.object_id \
                     LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = tr.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                     WHERE {filter} ORDER BY tr.parent_id, tr.name"
                ),
                &params,
            )
            .await?;
        for r in rows {
            if let Some(t) = int(r.first()).and_then(|o| tables.get_mut(&o)) {
                t.triggers.push(TriggerDef {
                    name: text(r.get(1)),
                    timing: if truthy(r.get(3)) { "INSTEAD OF".into() } else { "AFTER".into() },
                    events: text(r.get(5)).split(',').map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect(),
                    for_each_row: false,
                    enabled: !truthy(r.get(2)),
                    definition: text(r.get(4)).trim().to_string(),
                    body: None,
                    comment: if opts.comments { opt(r.get(6)) } else { None },
                });
            }
        }
    }

    if !opts.statistics {
        for t in tables.values_mut() {
            t.size_bytes = None;
        }
    }
    Ok(order.into_iter().filter_map(|o| tables.remove(&o)).collect())
}

pub async fn table_definition(s: &mut SqlServerSession, table: &ObjectRef) -> DbResult<TableDef> {
    let schema = schema_name(table.schema.as_deref());
    let mut opts = SnapshotOptions::full();
    opts.statistics = true;
    let mut tables = load_tables(s, table.database.as_deref(), &schema, Some(&table.name), &opts).await?;
    tables.pop().ok_or_else(|| DbError::query(format!("Table {schema}.{} does not exist", table.name)).with_code("208"))
}

// ------------------------------------------------------------ other objects

struct ModuleRow {
    object_id: i64,
    name: String,
    type_code: String,
    definition: Option<String>,
    comment: Option<String>,
}

async fn modules(s: &mut SqlServerSession, db: Option<&str>, schema: &str, name: Option<&str>, kinds: &[ObjectKind]) -> DbResult<Vec<ModuleRow>> {
    let d = dbp(db);
    let codes: Vec<&str> = kinds.iter().map(|k| type_codes(*k)).filter(|c| !c.is_empty()).collect();
    if codes.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = vec![Value::text(schema)];
    let mut filter = "s.name = @P1".to_string();
    if let Some(n) = name {
        params.push(Value::text(n));
        filter.push_str(" AND o.name = @P2");
    }
    let rows = s
        .rows(
            &format!(
                "SELECT o.object_id, o.name, o.type, m.definition, CAST(ep.value AS nvarchar(max)) \
                 FROM {d}sys.objects o JOIN {d}sys.schemas s ON s.schema_id = o.schema_id \
                 LEFT JOIN {d}sys.sql_modules m ON m.object_id = o.object_id \
                 LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                 WHERE {filter} AND o.type IN ({}) ORDER BY o.name",
                codes.join(", ")
            ),
            &params,
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| ModuleRow {
            object_id: int(r.first()).unwrap_or(0),
            name: text(r.get(1)),
            type_code: text(r.get(2)).trim().to_string(),
            definition: opt(r.get(3)),
            comment: opt(r.get(4)),
        })
        .collect())
}

pub async fn view_definition(s: &mut SqlServerSession, view: &ObjectRef) -> DbResult<ViewDef> {
    let schema = schema_name(view.schema.as_deref());
    let m = modules(s, view.database.as_deref(), &schema, Some(&view.name), &[ObjectKind::View])
        .await?
        .pop()
        .ok_or_else(|| DbError::query(format!("View {schema}.{} does not exist", view.name)).with_code("208"))?;
    let full = m.definition.clone().unwrap_or_default();
    let mut v = ViewDef {
        schema: Some(schema.clone()),
        name: view.name.clone(),
        materialized: false,
        definition: view_select(&full).map(str::to_string).unwrap_or_else(|| full.clone()),
        comment: m.comment,
        ..Default::default()
    };
    if m.definition.is_none() {
        v.options.insert("encrypted".into(), "true".into());
    }
    let scope = Scope { db: dbp(view.database.as_deref()), schema, table: Some(view.name.clone()), object_types: "'V'" };
    if let Some((cols, _)) = load_columns(s, &scope, None).await?.into_values().next() {
        v.columns = cols;
    }
    Ok(v)
}

async fn routine_params(s: &mut SqlServerSession, db: Option<&str>, object_ids: &[i64]) -> DbResult<BTreeMap<i64, Vec<Row>>> {
    let mut out: BTreeMap<i64, Vec<Row>> = BTreeMap::new();
    if object_ids.is_empty() {
        return Ok(out);
    }
    let d = dbp(db);
    let ids = object_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let rows = s
        .rows(
            &format!(
                "SELECT pa.object_id, pa.parameter_id, pa.name, ty.name, ts.name, ty.is_user_defined, pa.max_length, pa.precision, pa.scale, \
                 pa.is_output, pa.is_readonly \
                 FROM {d}sys.parameters pa JOIN {d}sys.types ty ON ty.user_type_id = pa.user_type_id \
                 JOIN {d}sys.schemas ts ON ts.schema_id = ty.schema_id \
                 WHERE pa.object_id IN ({ids}) ORDER BY pa.object_id, pa.parameter_id"
            ),
            &[],
        )
        .await?;
    for r in rows {
        if let Some(oid) = int(r.first()) {
            out.entry(oid).or_default().push(r);
        }
    }
    Ok(out)
}

fn routine_from(m: ModuleRow, schema: &str, params: Option<&Vec<Row>>) -> RoutineDef {
    let mut args = Vec::new();
    let mut returns = None;
    for p in params.into_iter().flatten() {
        let ty = type_from_row(p, 3);
        if int(p.get(1)) == Some(0) {
            returns = Some(ty);
            continue;
        }
        let mut a = format!("{} {ty}", text(p.get(2)));
        if truthy(p.get(9)) {
            a.push_str(" OUTPUT");
        }
        if truthy(p.get(10)) {
            a.push_str(" READONLY");
        }
        args.push(a);
    }
    let kind = kind_for_type_code(&m.type_code);
    if matches!(m.type_code.as_str(), "IF" | "TF" | "FT") {
        returns = Some("TABLE".into());
    }
    RoutineDef {
        schema: Some(schema.to_string()),
        name: m.name,
        kind,
        arguments: args.join(", "),
        returns,
        language: Some(if matches!(m.type_code.as_str(), "PC" | "FS" | "FT" | "AF") { "CLR".into() } else { "SQL".into() }),
        definition: m.definition.unwrap_or_default().trim().to_string(),
        comment: m.comment,
    }
}

pub async fn routine_definition(s: &mut SqlServerSession, routine: &ObjectRef) -> DbResult<RoutineDef> {
    let schema = schema_name(routine.schema.as_deref());
    let kinds = [ObjectKind::Procedure, ObjectKind::Function, ObjectKind::Aggregate];
    let m = modules(s, routine.database.as_deref(), &schema, Some(&routine.name), &kinds)
        .await?
        .pop()
        .ok_or_else(|| DbError::query(format!("Routine {schema}.{} does not exist", routine.name)).with_code("2812"))?;
    let params = routine_params(s, routine.database.as_deref(), &[m.object_id]).await?;
    let oid = m.object_id;
    Ok(routine_from(m, &schema, params.get(&oid)))
}

async fn sequences(s: &mut SqlServerSession, db: Option<&str>, schema: &str, name: Option<&str>) -> DbResult<Vec<SequenceDef>> {
    let d = dbp(db);
    let mut params = vec![Value::text(schema)];
    let mut filter = "s.name = @P1".to_string();
    if let Some(n) = name {
        params.push(Value::text(n));
        filter.push_str(" AND sq.name = @P2");
    }
    let rows = s
        .rows(
            &format!(
                "SELECT sq.name, ty.name, TRY_CAST(sq.start_value AS bigint), TRY_CAST(sq.increment AS bigint), \
                 TRY_CAST(sq.minimum_value AS bigint), TRY_CAST(sq.maximum_value AS bigint), sq.is_cycling, sq.is_cached, \
                 sq.cache_size, TRY_CAST(sq.current_value AS bigint), CAST(ep.value AS nvarchar(max)), ty.precision, ty.scale \
                 FROM {d}sys.sequences sq JOIN {d}sys.schemas s ON s.schema_id = sq.schema_id \
                 JOIN {d}sys.types ty ON ty.user_type_id = sq.user_type_id \
                 LEFT JOIN {d}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = sq.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                 WHERE {filter} ORDER BY sq.name"
            ),
            &params,
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let ty = text(r.get(1));
            let data_type = if matches!(ty.as_str(), "decimal" | "numeric") { format!("{ty}({},{})", int(r.get(11)).unwrap_or(18), int(r.get(12)).unwrap_or(0)) } else { ty };
            SequenceDef {
                schema: Some(schema.to_string()),
                name: text(r.first()),
                data_type: Some(data_type),
                start: int(r.get(2)),
                increment: int(r.get(3)),
                min_value: int(r.get(4)),
                max_value: int(r.get(5)),
                cycle: truthy(r.get(6)),
                cache: if truthy(r.get(7)) { int(r.get(8)) } else { Some(0) },
                owned_by: None,
                last_value: int(r.get(9)),
                comment: opt(r.get(10)),
            }
        })
        .collect())
}

pub async fn sequence_definition(s: &mut SqlServerSession, sequence: &ObjectRef) -> DbResult<SequenceDef> {
    let schema = schema_name(sequence.schema.as_deref());
    sequences(s, sequence.database.as_deref(), &schema, Some(&sequence.name))
        .await?
        .pop()
        .ok_or_else(|| DbError::query(format!("Sequence {schema}.{} does not exist", sequence.name)))
}

async fn user_types(s: &mut SqlServerSession, db: Option<&str>, schema: &str, name: Option<&str>) -> DbResult<Vec<TypeDef>> {
    let d = dbp(db);
    let mut params = vec![Value::text(schema)];
    let mut filter = "s.name = @P1".to_string();
    if let Some(n) = name {
        params.push(Value::text(n));
        filter.push_str(" AND ty.name = @P2");
    }
    let rows = s
        .rows(
            &format!(
                "SELECT ty.name, ty.is_table_type, ty.is_assembly_type, bt.name, ty.max_length, ty.precision, ty.scale, ty.is_nullable, \
                 tt.type_table_object_id, CAST(ep.value AS nvarchar(max)) \
                 FROM {d}sys.types ty JOIN {d}sys.schemas s ON s.schema_id = ty.schema_id \
                 LEFT JOIN {d}sys.types bt ON bt.user_type_id = ty.system_type_id AND ty.is_table_type = 0 AND ty.is_assembly_type = 0 \
                 LEFT JOIN {d}sys.table_types tt ON tt.user_type_id = ty.user_type_id \
                 LEFT JOIN {d}sys.extended_properties ep ON ep.class = 6 AND ep.major_id = ty.user_type_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                 WHERE ty.is_user_defined = 1 AND {filter} ORDER BY ty.name"
            ),
            &params,
        )
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let tname = text(r.first());
        let qname = qualified(Some(schema), &tname);
        let (category, definition, elements) = if truthy(r.get(1)) {
            let table_oid = int(r.get(8)).unwrap_or(0);
            let cols = table_type_columns(s, db, table_oid).await.unwrap_or_default();
            let tmp = TableDef { name: tname.clone(), schema: Some(schema.to_string()), columns: cols.clone(), ..Default::default() };
            let lines: Vec<String> = cols.iter().map(|c| crate::ddl::column_definition(&tmp, c)).collect();
            ("table".to_string(), format!("CREATE TYPE {qname} AS TABLE (\n    {}\n)", lines.join(",\n    ")), cols.iter().map(|c| format!("{} {}", c.name, c.data_type)).collect())
        } else if truthy(r.get(2)) {
            ("clr".to_string(), String::new(), Vec::new())
        } else {
            let base = format_type(&text(r.get(3)), "sys", false, int(r.get(4)).unwrap_or(0), int(r.get(5)).unwrap_or(0), int(r.get(6)).unwrap_or(0));
            let null = if truthy(r.get(7)) { "" } else { " NOT NULL" };
            ("alias".to_string(), format!("CREATE TYPE {qname} FROM {base}{null}"), vec![base])
        };
        out.push(TypeDef { schema: Some(schema.to_string()), name: tname, category, elements, definition, comment: opt(r.get(9)) });
    }
    Ok(out)
}

async fn table_type_columns(s: &mut SqlServerSession, db: Option<&str>, object_id: i64) -> DbResult<Vec<ColumnDef>> {
    let d = dbp(db);
    let rows = s
        .rows(
            &format!(
                "SELECT c.name, ty.name, ts.name, ty.is_user_defined, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity \
                 FROM {d}sys.columns c JOIN {d}sys.types ty ON ty.user_type_id = c.user_type_id \
                 JOIN {d}sys.schemas ts ON ts.schema_id = ty.schema_id WHERE c.object_id = @P1 ORDER BY c.column_id"
            ),
            &[Value::Int(object_id)],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut c = ColumnDef::new(text(r.first()), type_from_row(&r, 1));
            c.nullable = truthy(r.get(7));
            c.auto_increment = truthy(r.get(8));
            c
        })
        .collect())
}

fn go_join(statements: Vec<String>) -> String {
    let mut out = statements.join("\nGO\n\n");
    if !out.is_empty() {
        out.push_str("\nGO\n");
    }
    out
}

pub async fn object_ddl(s: &mut SqlServerSession, obj: &ObjectRef) -> DbResult<String> {
    let ddl = MsSqlDdl::new();
    let schema = schema_name(obj.schema.as_deref());
    let db = obj.database.as_deref();
    match obj.kind {
        ObjectKind::Table => {
            let t = table_definition(s, obj).await?;
            Ok(go_join(ddl.create_table(&t, true)))
        }
        ObjectKind::View | ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Aggregate => {
            let kinds = [obj.kind];
            let m = modules(s, db, &schema, Some(&obj.name), &kinds)
                .await?
                .pop()
                .ok_or_else(|| DbError::query(format!("{} {schema}.{} does not exist", obj.kind.label(), obj.name)))?;
            let def = m.definition.clone().ok_or_else(|| DbError::unsupported(format!("The definition of {schema}.{} is encrypted", obj.name)))?;
            let mut stmts = vec![def.trim().to_string()];
            if let Some(c) = &m.comment {
                let l1 = if obj.kind == ObjectKind::View {
                    "VIEW"
                } else if obj.kind == ObjectKind::Procedure {
                    "PROCEDURE"
                } else {
                    "FUNCTION"
                };
                stmts.push(format!(
                    "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {}, @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{l1}', @level1name = {}",
                    nstring(c),
                    nstring(&schema),
                    nstring(&obj.name)
                ));
            }
            Ok(go_join(stmts))
        }
        ObjectKind::Trigger => {
            let d = dbp(db);
            let v = s
                .scalar(
                    &format!(
                        "SELECT m.definition FROM {d}sys.triggers tr JOIN {d}sys.sql_modules m ON m.object_id = tr.object_id \
                         JOIN {d}sys.objects o ON o.object_id = tr.object_id JOIN {d}sys.schemas sc ON sc.schema_id = o.schema_id \
                         WHERE sc.name = @P1 AND tr.name = @P2"
                    ),
                    &[Value::text(schema.clone()), Value::text(obj.name.clone())],
                )
                .await?;
            match v.as_str() {
                Some(def) => Ok(go_join(vec![def.trim().to_string()])),
                None => Err(DbError::query(format!("Trigger {schema}.{} not found or encrypted", obj.name))),
            }
        }
        ObjectKind::Sequence => Ok(go_join(vec![ddl.create_sequence(&sequence_definition(s, obj).await?)])),
        ObjectKind::Type => {
            let t = user_types(s, db, &schema, Some(&obj.name)).await?.pop().ok_or_else(|| DbError::query(format!("Type {schema}.{} does not exist", obj.name)))?;
            if t.definition.is_empty() {
                return Err(DbError::unsupported("DDL of CLR types is not available"));
            }
            Ok(go_join(vec![t.definition]))
        }
        ObjectKind::Synonym => {
            let d = dbp(db);
            let v = s
                .scalar(
                    &format!(
                        "SELECT sy.base_object_name FROM {d}sys.synonyms sy JOIN {d}sys.schemas sc ON sc.schema_id = sy.schema_id \
                         WHERE sc.name = @P1 AND sy.name = @P2"
                    ),
                    &[Value::text(schema.clone()), Value::text(obj.name.clone())],
                )
                .await?;
            Ok(go_join(vec![format!("CREATE SYNONYM {} FOR {}", qualified(Some(&schema), &obj.name), v.to_text())]))
        }
        ObjectKind::Schema => {
            let d = dbp(obj.database.as_deref());
            let owner = s
                .scalar(
                    &format!("SELECT p.name FROM {d}sys.schemas sc JOIN {d}sys.database_principals p ON p.principal_id = sc.principal_id WHERE sc.name = @P1"),
                    &[Value::text(obj.name.clone())],
                )
                .await?;
            let mut stmt = format!("CREATE SCHEMA {}", quote(&obj.name));
            if let Some(o) = owner.as_str() {
                stmt.push_str(&format!(" AUTHORIZATION {}", quote(o)));
            }
            Ok(go_join(vec![stmt]))
        }
        ObjectKind::Database => {
            let collation = s.scalar("SELECT collation_name FROM sys.databases WHERE name = @P1", &[Value::text(obj.name.clone())]).await?;
            let mut stmt = format!("CREATE DATABASE {}", quote(&obj.name));
            if let Some(c) = collation.as_str() {
                stmt.push_str(&format!(" COLLATE {c}"));
            }
            Ok(go_join(vec![stmt]))
        }
        ObjectKind::Column | ObjectKind::Index | ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::ForeignKey | ObjectKind::CheckConstraint => {
            let parent = obj.parent_table().ok_or_else(|| DbError::config("Missing owning table"))?;
            let t = table_definition(s, &parent).await?;
            let not_found = || DbError::query(format!("{} {} not found", obj.kind.label(), obj.name));
            let stmt = match obj.kind {
                ObjectKind::Column => t.column(&obj.name).map(|c| ddl.add_column(&t, c)).ok_or_else(not_found)?,
                ObjectKind::Index => t.indexes.iter().find(|i| i.name == obj.name).map(|i| vec![ddl.create_index(&t, i)]).ok_or_else(not_found)?,
                ObjectKind::PrimaryKey => {
                    t.primary_key.as_ref().filter(|k| k.name.as_deref() == Some(obj.name.as_str())).map(|k| vec![ddl.add_primary_key(&t, k)]).ok_or_else(not_found)?
                }
                ObjectKind::UniqueKey => {
                    t.unique_keys.iter().find(|k| k.name.as_deref() == Some(obj.name.as_str())).map(|k| vec![ddl.add_unique_key(&t, k)]).ok_or_else(not_found)?
                }
                ObjectKind::ForeignKey => {
                    t.foreign_keys.iter().find(|k| k.name.as_deref() == Some(obj.name.as_str())).map(|k| vec![ddl.add_foreign_key(&t, k)]).ok_or_else(not_found)?
                }
                _ => t.checks.iter().find(|k| k.name.as_deref() == Some(obj.name.as_str())).map(|k| vec![ddl.add_check(&t, k)]).ok_or_else(not_found)?,
            };
            Ok(go_join(stmt))
        }
        other => Err(DbError::unsupported(format!("DDL of {} objects is not available", other.label()))),
    }
}

async fn object_id(s: &mut SqlServerSession, obj: &ObjectRef) -> DbResult<Option<i64>> {
    let d = dbp(obj.database.as_deref());
    let v = s
        .scalar(
            &format!(
                "SELECT o.object_id FROM {d}sys.objects o JOIN {d}sys.schemas sc ON sc.schema_id = o.schema_id \
                 WHERE sc.name = @P1 AND o.name = @P2"
            ),
            &[Value::text(schema_name(obj.schema.as_deref())), Value::text(obj.name.clone())],
        )
        .await?;
    Ok(v.as_i64())
}

fn date_text(v: Option<&Value>) -> Option<String> {
    opt(v)
}

pub async fn object_properties(s: &mut SqlServerSession, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    let d = dbp(obj.database.as_deref());
    match obj.kind {
        ObjectKind::Database => {
            let rows = s
                .rows(
                    "SELECT database_id, compatibility_level, recovery_model_desc, state_desc, collation_name, \
                     CONVERT(nvarchar(30), create_date, 120), is_read_only, snapshot_isolation_state_desc, is_read_committed_snapshot_on, \
                     user_access_desc, containment_desc FROM sys.databases WHERE name = @P1",
                    &[Value::text(obj.name.clone())],
                )
                .await?;
            let Some(r) = rows.first() else { return Ok(Vec::new()) };
            let labels = [
                "Database ID",
                "Compatibility level",
                "Recovery model",
                "State",
                "Collation",
                "Created",
                "Read only",
                "Snapshot isolation",
                "Read committed snapshot",
                "User access",
                "Containment",
            ];
            Ok(labels.iter().enumerate().filter_map(|(i, l)| opt(r.get(i)).map(|v| Property::new(*l, v))).collect())
        }
        ObjectKind::Schema => Ok(Vec::new()),
        k if kind_for_type_code_matches(k) => {
            let rows = s
                .rows(
                    &format!(
                        "SELECT o.object_id, o.type_desc, CONVERT(nvarchar(30), o.create_date, 120), CONVERT(nvarchar(30), o.modify_date, 120), \
                         o.is_ms_shipped, m.uses_ansi_nulls, m.uses_quoted_identifier, m.is_schema_bound, \
                         CASE WHEN o.type = 'U' THEN (SELECT t.lock_escalation_desc FROM {d}sys.tables t WHERE t.object_id = o.object_id) END \
                         FROM {d}sys.objects o JOIN {d}sys.schemas sc ON sc.schema_id = o.schema_id \
                         LEFT JOIN {d}sys.sql_modules m ON m.object_id = o.object_id \
                         WHERE sc.name = @P1 AND o.name = @P2"
                    ),
                    &[Value::text(schema_name(obj.schema.as_deref())), Value::text(obj.name.clone())],
                )
                .await?;
            let Some(r) = rows.first() else { return Ok(Vec::new()) };
            let labels = ["Object ID", "Type", "Created", "Modified", "System object", "ANSI NULLS", "QUOTED_IDENTIFIER", "Schema bound", "Lock escalation"];
            Ok(labels.iter().enumerate().filter_map(|(i, l)| if i == 2 || i == 3 { date_text(r.get(i)) } else { opt(r.get(i)) }.map(|v| Property::new(*l, v))).collect())
        }
        _ => Ok(Vec::new()),
    }
}

fn kind_for_type_code_matches(k: ObjectKind) -> bool {
    matches!(k, ObjectKind::Table | ObjectKind::View | ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Aggregate | ObjectKind::Sequence | ObjectKind::Synonym)
}

fn human_bytes(n: i64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if v.abs() < 1024.0 || unit == "TB" {
            return if unit == "B" { format!("{n} B") } else { format!("{v:.2} {unit}") };
        }
        v /= 1024.0;
    }
    format!("{n} B")
}

pub async fn object_statistics(s: &mut SqlServerSession, obj: &ObjectRef) -> DbResult<Vec<Property>> {
    if obj.kind != ObjectKind::Table {
        return Ok(Vec::new());
    }
    let d = dbp(obj.database.as_deref());
    let rows = s
        .rows(
            &format!(
                "SELECT SUM(CASE WHEN p.index_id IN (0, 1) THEN p.rows ELSE 0 END), \
                 SUM(a.total_pages) * 8192, SUM(a.used_pages) * 8192, \
                 SUM(CASE WHEN p.index_id IN (0, 1) AND a.type <> 2 THEN a.data_pages ELSE 0 END) * 8192, \
                 COUNT(DISTINCT p.partition_number), \
                 (SELECT CONVERT(nvarchar(30), MAX(STATS_DATE(st.object_id, st.stats_id)), 120) FROM {d}sys.stats st WHERE st.object_id = t.object_id) \
                 FROM {d}sys.tables t JOIN {d}sys.schemas sc ON sc.schema_id = t.schema_id \
                 JOIN {d}sys.partitions p ON p.object_id = t.object_id \
                 JOIN {d}sys.allocation_units a ON a.container_id = p.partition_id \
                 WHERE sc.name = @P1 AND t.name = @P2 GROUP BY t.object_id"
            ),
            &[Value::text(schema_name(obj.schema.as_deref())), Value::text(obj.name.clone())],
        )
        .await?;
    let Some(r) = rows.first() else { return Ok(Vec::new()) };
    let mut props = vec![Property::new("Rows", text(r.first()))];
    for (i, label) in [(1, "Reserved"), (2, "Used"), (3, "Data")] {
        if let Some(n) = int(r.get(i)) {
            props.push(Property::new(label, human_bytes(n)));
        }
    }
    if let (Some(used), Some(data)) = (int(r.get(2)), int(r.get(3))) {
        props.push(Property::new("Index size", human_bytes((used - data).max(0))));
    }
    props.push(Property::new("Partitions", text(r.get(4))));
    if let Some(sd) = opt(r.get(5)) {
        props.push(Property::new("Statistics updated", sd));
    }
    Ok(props)
}

pub async fn object_dependencies(s: &mut SqlServerSession, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
    let Some(oid) = object_id(s, obj).await? else { return Ok(Vec::new()) };
    let d = dbp(obj.database.as_deref());
    let mut out = Vec::new();
    let rows = s
        .rows(
            &format!(
                "SELECT COALESCE(rs.name, d.referenced_schema_name), d.referenced_entity_name, o.type \
                 FROM {d}sys.sql_expression_dependencies d \
                 LEFT JOIN {d}sys.objects o ON o.object_id = d.referenced_id \
                 LEFT JOIN {d}sys.schemas rs ON rs.schema_id = o.schema_id \
                 WHERE d.referencing_id = @P1 AND d.referenced_id IS NOT NULL"
            ),
            &[Value::Int(oid)],
        )
        .await?;
    for r in rows {
        let kind = kind_for_type_code(&text(r.get(2))).unwrap_or(ObjectKind::Table);
        out.push(Dependency {
            direction: DependencyDirection::DependsOn,
            target: ObjectRef::new(kind, opt(r.first()), text(r.get(1))).with_database(obj.database.clone()),
            kind: Some("reference".into()),
        });
    }
    let rows = s
        .rows(
            &format!(
                "SELECT sc.name, o.name, o.type FROM {d}sys.sql_expression_dependencies d \
                 JOIN {d}sys.objects o ON o.object_id = d.referencing_id \
                 JOIN {d}sys.schemas sc ON sc.schema_id = o.schema_id WHERE d.referenced_id = @P1"
            ),
            &[Value::Int(oid)],
        )
        .await?;
    for r in rows {
        let kind = kind_for_type_code(&text(r.get(2))).unwrap_or(ObjectKind::View);
        let mut target = ObjectRef::new(kind, opt(r.first()), text(r.get(1))).with_database(obj.database.clone());
        if kind == ObjectKind::Trigger {
            target.table = Some(obj.name.clone());
        }
        out.push(Dependency { direction: DependencyDirection::UsedBy, target, kind: Some("reference".into()) });
    }
    if obj.kind == ObjectKind::Table {
        let rows = s
            .rows(
                &format!(
                    "SELECT ps.name, pt.name, rs.name, rt.name, fk.parent_object_id FROM {d}sys.foreign_keys fk \
                     JOIN {d}sys.objects pt ON pt.object_id = fk.parent_object_id JOIN {d}sys.schemas ps ON ps.schema_id = pt.schema_id \
                     JOIN {d}sys.objects rt ON rt.object_id = fk.referenced_object_id JOIN {d}sys.schemas rs ON rs.schema_id = rt.schema_id \
                     WHERE fk.parent_object_id = @P1 OR fk.referenced_object_id = @P1"
                ),
                &[Value::Int(oid)],
            )
            .await?;
        for r in rows {
            if int(r.get(4)) == Some(oid) {
                out.push(Dependency {
                    direction: DependencyDirection::DependsOn,
                    target: ObjectRef::table(opt(r.get(2)), text(r.get(3))).with_database(obj.database.clone()),
                    kind: Some("foreign key".into()),
                });
            } else {
                out.push(Dependency {
                    direction: DependencyDirection::UsedBy,
                    target: ObjectRef::table(opt(r.first()), text(r.get(1))).with_database(obj.database.clone()),
                    kind: Some("foreign key".into()),
                });
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ search / snapshot

/// LIKE pattern with `\` escapes for a metadata search pattern.
pub fn like_pattern(pattern: &str) -> String {
    let has_wild = pattern.contains('*') || pattern.contains('?');
    let mut out = String::new();
    for c in pattern.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' | '[' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    if has_wild { out } else { format!("%{out}%") }
}

pub async fn search_metadata(s: &mut SqlServerSession, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
    let collate = if search.case_sensitive { "Latin1_General_CS_AS" } else { "Latin1_General_CI_AS" };
    let limit = search.limit.clamp(1, 100_000);
    let mut params = vec![Value::text(like_pattern(&search.pattern))];
    let mut schema_filter = String::new();
    if !search.schemas.is_empty() {
        let list: Vec<String> = search
            .schemas
            .iter()
            .map(|sc| {
                params.push(Value::text(sc.clone()));
                format!("@P{}", params.len())
            })
            .collect();
        schema_filter = format!(" AND sc.name IN ({})", list.join(", "));
    }
    let mut out = Vec::new();
    let codes: Vec<&str> = search.kinds.iter().filter(|k| **k != ObjectKind::Column).map(|k| type_codes(*k)).filter(|c| !c.is_empty()).collect();
    if !codes.is_empty() {
        let comment = if search.search_comments { format!(" OR CAST(ep.value AS nvarchar(4000)) COLLATE {collate} LIKE @P1 ESCAPE '\\'") } else { String::new() };
        let rows = s
            .rows(
                &format!(
                    "SELECT TOP ({limit}) sc.name, o.name, o.type, CAST(ep.value AS nvarchar(max)), po.name \
                     FROM sys.objects o JOIN sys.schemas sc ON sc.schema_id = o.schema_id \
                     LEFT JOIN sys.objects po ON po.object_id = o.parent_object_id \
                     LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = {MS_DESC} \
                     WHERE o.is_ms_shipped = 0 AND o.type IN ({}) AND (o.name COLLATE {collate} LIKE @P1 ESCAPE '\\'{comment}){schema_filter} \
                     ORDER BY sc.name, o.name",
                    codes.join(", ")
                ),
                &params,
            )
            .await?;
        for r in rows {
            let Some(kind) = kind_for_type_code(&text(r.get(2))) else { continue };
            let mut o = ObjectRef::new(kind, opt(r.first()), text(r.get(1)));
            if kind == ObjectKind::Trigger {
                o.table = opt(r.get(4));
            }
            out.push(ObjectInfo::new(o).with_comment(opt(r.get(3))));
        }
    }
    if search.kinds.contains(&ObjectKind::Column) && out.len() < limit {
        let comment = if search.search_comments { format!(" OR CAST(ep.value AS nvarchar(4000)) COLLATE {collate} LIKE @P1 ESCAPE '\\'") } else { String::new() };
        let rows = s
            .rows(
                &format!(
                    "SELECT TOP ({}) sc.name, o.name, c.name, ty.name, ts.name, ty.is_user_defined, c.max_length, c.precision, c.scale, \
                     CAST(ep.value AS nvarchar(max)) \
                     FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id AND o.type IN ('U', 'V') \
                     JOIN sys.schemas sc ON sc.schema_id = o.schema_id \
                     JOIN sys.types ty ON ty.user_type_id = c.user_type_id JOIN sys.schemas ts ON ts.schema_id = ty.schema_id \
                     LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = {MS_DESC} \
                     WHERE o.is_ms_shipped = 0 AND (c.name COLLATE {collate} LIKE @P1 ESCAPE '\\'{comment}){schema_filter} \
                     ORDER BY sc.name, o.name, c.column_id",
                    limit - out.len()
                ),
                &params,
            )
            .await?;
        for r in rows {
            let parent = ObjectRef::table(opt(r.first()), text(r.get(1)));
            out.push(ObjectInfo::new(ObjectRef::child(ObjectKind::Column, &parent, text(r.get(2)))).with_detail(type_from_row(&r, 3)).with_comment(opt(r.get(9))));
        }
    }
    out.truncate(limit);
    Ok(out)
}

pub async fn schema_snapshot(s: &mut SqlServerSession, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
    let sc = schema_name(schema.schema.as_deref());
    let db = schema.database.as_deref();
    let mut snap = SchemaSnapshot { kind: Some(DbKind::SqlServer), database: schema.database.clone(), schema: Some(sc.clone()), ..Default::default() };
    snap.tables = load_tables(s, db, &sc, None, opts).await?;
    if opts.views {
        let mods = modules(s, db, &sc, None, &[ObjectKind::View]).await?;
        let mut cols = if opts.columns {
            let scope = Scope { db: dbp(db), schema: sc.clone(), table: None, object_types: "'V'" };
            load_columns(s, &scope, None).await?
        } else {
            BTreeMap::new()
        };
        for m in mods {
            let full = m.definition.clone().unwrap_or_default();
            snap.views.push(ViewDef {
                schema: Some(sc.clone()),
                name: m.name.clone(),
                materialized: false,
                definition: if opts.view_definitions { view_select(&full).map(str::to_string).unwrap_or(full) } else { String::new() },
                columns: cols.remove(&m.object_id).map(|(c, _)| c).unwrap_or_default(),
                comment: if opts.comments { m.comment } else { None },
                options: Default::default(),
            });
        }
    }
    if opts.sequences {
        snap.sequences = sequences(s, db, &sc, None).await?;
    }
    if opts.routines {
        let mods = modules(s, db, &sc, None, &[ObjectKind::Procedure, ObjectKind::Function, ObjectKind::Aggregate]).await?;
        let ids: Vec<i64> = mods.iter().map(|m| m.object_id).collect();
        let params = routine_params(s, db, &ids).await?;
        for m in mods {
            let oid = m.object_id;
            snap.routines.push(routine_from(m, &sc, params.get(&oid)));
        }
    }
    if opts.types {
        snap.types = user_types(s, db, &sc, None).await?;
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_format() {
        assert_eq!(format_type("nvarchar", "sys", false, 100, 0, 0), "nvarchar(50)");
        assert_eq!(format_type("nvarchar", "sys", false, -1, 0, 0), "nvarchar(max)");
        assert_eq!(format_type("varbinary", "sys", false, -1, 0, 0), "varbinary(max)");
        assert_eq!(format_type("char", "sys", false, 10, 0, 0), "char(10)");
        assert_eq!(format_type("decimal", "sys", false, 9, 10, 2), "decimal(10,2)");
        assert_eq!(format_type("datetime2", "sys", false, 8, 27, 7), "datetime2(7)");
        assert_eq!(format_type("int", "sys", false, 4, 10, 0), "int");
        assert_eq!(format_type("float", "sys", false, 8, 53, 0), "float");
        assert_eq!(format_type("Phone", "dbo", true, 20, 0, 0), "[dbo].[Phone]");
    }

    #[test]
    fn helpers() {
        assert_eq!(like_pattern("cust"), "%cust%");
        assert_eq!(like_pattern("cu*t?"), "cu%t_");
        assert_eq!(like_pattern("a_[b]"), "%a\\_\\[b]%");
        assert_eq!(dbp(Some("my db")), "[my db].");
        assert_eq!(dbp(None), "");
        assert_eq!(kind_for_type_code("IF"), Some(ObjectKind::Function));
        assert_eq!(kind_for_type_code("U "), Some(ObjectKind::Table));
        assert_eq!(kind_for_type_code("S"), None);
        assert_eq!(go_join(vec!["A".into(), "B".into()]), "A\nGO\n\nB\nGO\n");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.00 MB");
    }

    #[test]
    fn routine_assembly() {
        let m = ModuleRow { object_id: 1, name: "f".into(), type_code: "FN".into(), definition: Some(" CREATE FUNCTION f() ".into()), comment: None };
        let p = |id: i64, name: &str, ty: &str, out: bool| -> Row {
            vec![
                Value::Int(1),
                Value::Int(id),
                Value::text(name),
                Value::text(ty),
                Value::text("sys"),
                Value::Bool(false),
                Value::Int(if ty == "nvarchar" { 20 } else { 4 }),
                Value::Int(0),
                Value::Int(0),
                Value::Bool(out),
                Value::Bool(false),
            ]
        };
        let params = vec![p(0, "", "int", false), p(1, "@a", "int", false), p(2, "@b", "nvarchar", true)];
        let r = routine_from(m, "dbo", Some(&params));
        assert_eq!(r.arguments, "@a int, @b nvarchar(10) OUTPUT");
        assert_eq!(r.returns.as_deref(), Some("int"));
        assert_eq!(r.kind, Some(ObjectKind::Function));
        assert_eq!(r.definition, "CREATE FUNCTION f()");
    }
}
