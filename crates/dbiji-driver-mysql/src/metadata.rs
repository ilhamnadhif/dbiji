//! Metadata from `information_schema` (plus `SHOW CREATE ...` for DDL).
//!
//! Table definitions and schema snapshots share one loader that issues a
//! fixed number of batched queries per schema (or per table), so snapshots of
//! schemas with thousands of tables need only a handful of round trips.

use crate::convert::read;
use crate::ddl::{self, MySqlDdl, option_keys, qualified, quote};
use crate::dialect::type_kind;
use crate::flavor::ServerProfile;
use crate::session::MySqlSession;
use crate::sqltext;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{DbError, DbResult, Dialect};
use mysql_async::{Row as MyRow, Value as MyValue};
use std::collections::{BTreeMap, HashMap};

/// Schemas created by the server itself.
pub const SYSTEM_SCHEMAS: &[&str] = &["mysql", "information_schema", "performance_schema", "sys"];

/// Is `schema` a system schema?
pub fn is_system_schema(schema: &str) -> bool {
    SYSTEM_SCHEMAS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(schema))
}

/// Character set of a collation name (`utf8mb4_0900_ai_ci` → `utf8mb4`).
pub fn charset_of_collation(collation: &str) -> String {
    collation.split('_').next().unwrap_or(collation).to_string()
}

const NUMERIC_TYPES: &[&str] = &[
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
    "decimal",
    "numeric",
    "dec",
    "fixed",
    "float",
    "double",
    "real",
    "year",
    "bit",
    "bool",
    "boolean",
    "serial",
];

const BINARY_TYPES: &[&str] = &[
    "binary",
    "varbinary",
    "tinyblob",
    "blob",
    "mediumblob",
    "longblob",
];

/// Normalise `COLUMN_DEFAULT` into an SQL expression.
///
/// MariaDB 10.2.7+ already reports SQL expressions (`'abc'`, `NULL`,
/// `current_timestamp()`); MySQL reports raw values (`abc`, `0x6162`) and
/// marks expression defaults with `DEFAULT_GENERATED` in `EXTRA`.
pub fn normalize_default(
    profile: &ServerProfile,
    data_type: &str,
    raw: Option<String>,
    extra: &str,
) -> Option<String> {
    let raw = raw?;
    if profile.defaults_are_expressions() {
        if raw.eq_ignore_ascii_case("NULL") {
            return None;
        }
        return Some(raw);
    }
    let extra_l = extra.to_ascii_lowercase();
    let dt = data_type.trim().to_ascii_lowercase();
    let base = dt.split(['(', ' ']).next().unwrap_or("");
    if extra_l.contains("default_generated") {
        if ddl::is_now_default(&raw) {
            return Some(raw);
        }
        return Some(format!("({raw})"));
    }
    if NUMERIC_TYPES.contains(&base) {
        return Some(raw);
    }
    if matches!(base, "timestamp" | "datetime") && ddl::is_now_default(&raw) {
        return Some(raw);
    }
    if BINARY_TYPES.contains(&base) && raw.starts_with("0x") {
        return Some(raw);
    }
    Some(ddl::string_literal(&raw))
}

/// `ON UPDATE` expression from `EXTRA` (`DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)`).
pub fn on_update_from_extra(extra: &str) -> Option<String> {
    let lower = extra.to_ascii_lowercase();
    let i = lower.find("on update ")?;
    let rest = extra[i + "on update ".len()..].trim();
    let expr = rest.split_whitespace().next()?;
    Some(expr.to_string())
}

/// Translate a `*` / `?` wildcard pattern (plain text = contains) into LIKE.
pub fn like_pattern(pattern: &str) -> String {
    let mut out = String::new();
    let wild = pattern.contains(['*', '?']);
    if !wild {
        out.push('%');
    }
    for c in pattern.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '*' if wild => out.push('%'),
            '?' if wild => out.push('_'),
            c => out.push(c),
        }
    }
    if !wild {
        out.push('%');
    }
    out
}

/// Snapshot options with everything off except what `f` enables (comments on).
fn only(f: impl FnOnce(&mut SnapshotOptions)) -> SnapshotOptions {
    let mut o = SnapshotOptions {
        columns: false,
        constraints: false,
        indexes: false,
        triggers: false,
        partitions: false,
        views: false,
        view_definitions: false,
        sequences: false,
        routines: false,
        types: false,
        comments: true,
        statistics: false,
    };
    f(&mut o);
    o
}

fn obj(kind: ObjectKind, schema: &str, name: impl Into<String>) -> ObjectRef {
    ObjectRef::new(kind, Some(schema.to_string()), name)
}

/// Rows of the catalog for one schema, optionally restricted to one table.
struct Scope<'a> {
    schema: &'a str,
    table: Option<&'a str>,
}

impl Scope<'_> {
    fn filter(&self, table_col: &str) -> String {
        match self.table {
            Some(_) => format!(" AND {table_col} = ?"),
            None => String::new(),
        }
    }
}

/// Result of the batched loader.
#[derive(Default)]
struct Loaded {
    tables: Vec<TableDef>,
    views: Vec<ViewDef>,
    sequences: Vec<SequenceDef>,
}

fn add_index_column(t: &TableDef, idx: &mut IndexDef, row: &MyRow, functional: bool) {
    let column = read::non_empty(row, 3);
    let expression = if functional {
        read::non_empty(row, 8)
    } else {
        None
    };
    let descending = read::text(row, 4).as_deref() == Some("D");
    let sub_part = read::int(row, 5);
    let method = idx.method.clone().unwrap_or_default().to_ascii_uppercase();
    let ic = match (column, expression) {
        (Some(c), _) => {
            let name = match sub_part {
                Some(n) if n > 0 && method != "FULLTEXT" && method != "SPATIAL" => {
                    format!("{c}({n})")
                }
                _ => c,
            };
            IndexColumn {
                name,
                is_expression: false,
                descending,
            }
        }
        (None, Some(e)) => IndexColumn {
            name: e,
            is_expression: true,
            descending,
        },
        (None, None) => IndexColumn {
            name: String::new(),
            is_expression: true,
            descending,
        },
    };
    let _ = t;
    idx.columns.push(ic);
}

impl MySqlSession {
    fn ddl_gen(&self) -> MySqlDdl {
        MySqlDdl::new(self.profile.clone())
    }

    /// Temporarily disable MySQL 8's `information_schema` statistics cache so
    /// row counts / AUTO_INCREMENT are current. Returns the previous value.
    async fn fresh_stats_begin(&mut self) -> Option<i64> {
        if !self.profile.has_stats_expiry() {
            return None;
        }
        let old = self
            .try_rows(
                "SELECT @@SESSION.information_schema_stats_expiry",
                Vec::new(),
            )
            .await
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|r| read::int(r, 0)))?;
        if old != 0
            && self
                .exec_internal("SET @@SESSION.information_schema_stats_expiry = 0")
                .await
                .is_ok()
        {
            Some(old)
        } else {
            None
        }
    }

    async fn fresh_stats_end(&mut self, old: Option<i64>) {
        if let Some(v) = old {
            let _ = self
                .exec_internal(&format!(
                    "SET @@SESSION.information_schema_stats_expiry = {v}"
                ))
                .await;
        }
    }

    fn scope_params(&self, scope: &Scope<'_>) -> Vec<MyValue> {
        let mut p = vec![self.p(scope.schema)];
        if let Some(t) = scope.table {
            p.push(self.p(t));
        }
        p
    }

    // ------------------------------------------------------------------ schemas

    pub(crate) async fn meta_list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        let current = self.current_db().await?;
        let comment_col = if self.profile.has_schema_comment() {
            "SCHEMA_COMMENT"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME, {comment_col} \
             FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME"
        );
        let rows = self.rows(&sql, Vec::new()).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let name = read::text_or_empty(r, 0);
                SchemaInfo {
                    is_current: current.as_deref() == Some(name.as_str()),
                    is_system: is_system_schema(&name),
                    owner: None,
                    comment: read::non_empty(r, 3),
                    name,
                }
            })
            .collect())
    }

    // ------------------------------------------------------------------ objects

    pub(crate) async fn meta_list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let s = self.schema_or_current(schema.schema.as_deref()).await?;
        let system = is_system_schema(&s);
        let p = vec![self.p(&s)];
        let mut out = Vec::new();
        match kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence => {
                let types = match kind {
                    ObjectKind::Table => "'BASE TABLE','SYSTEM VERSIONED','TEMPORARY'",
                    ObjectKind::View => "'VIEW','SYSTEM VIEW'",
                    _ => "'SEQUENCE'",
                };
                let sql = format!(
                    "SELECT TABLE_NAME, ENGINE, TABLE_ROWS, TABLE_COMMENT, CREATE_OPTIONS, TABLE_TYPE \
                     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN ({types}) \
                     ORDER BY TABLE_NAME"
                );
                for r in self.rows(&sql, p).await? {
                    let name = read::text_or_empty(&r, 0);
                    let mut info = ObjectInfo::new(obj(kind, &s, name));
                    info.is_system = system;
                    if kind == ObjectKind::Table {
                        let mut detail = read::text_or_empty(&r, 1);
                        if read::text_or_empty(&r, 4)
                            .to_ascii_lowercase()
                            .contains("partitioned")
                        {
                            detail.push_str(" (partitioned)");
                        }
                        if read::text_or_empty(&r, 5) == "SYSTEM VERSIONED" {
                            detail.push_str(" (system versioned)");
                        }
                        if !detail.trim().is_empty() {
                            info.detail = Some(detail.trim().to_string());
                        }
                        info.row_estimate = read::int(&r, 2);
                        info = info.with_comment(read::non_empty(&r, 3));
                    }
                    out.push(info);
                }
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let rtype = if kind == ObjectKind::Function {
                    "FUNCTION"
                } else {
                    "PROCEDURE"
                };
                let sql = "SELECT r.ROUTINE_NAME, r.DTD_IDENTIFIER, r.ROUTINE_COMMENT, \
                    (SELECT GROUP_CONCAT(CONCAT_WS(' ', p.PARAMETER_MODE, p.PARAMETER_NAME, p.DTD_IDENTIFIER) \
                        ORDER BY p.ORDINAL_POSITION SEPARATOR ', ') \
                     FROM information_schema.PARAMETERS p WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA \
                       AND p.SPECIFIC_NAME = r.SPECIFIC_NAME AND p.ROUTINE_TYPE = r.ROUTINE_TYPE AND p.ORDINAL_POSITION > 0) \
                    FROM information_schema.ROUTINES r WHERE r.ROUTINE_SCHEMA = ? AND r.ROUTINE_TYPE = ? \
                    ORDER BY r.ROUTINE_NAME";
                let params = vec![self.p(&s), self.p(rtype)];
                for r in self.rows(sql, params).await? {
                    let name = read::text_or_empty(&r, 0);
                    let args = read::text_or_empty(&r, 3);
                    let mut detail = format!("({args})");
                    if kind == ObjectKind::Function
                        && let Some(ret) = read::non_empty(&r, 1)
                    {
                        detail.push_str(&format!(" RETURNS {ret}"));
                    }
                    let mut info = ObjectInfo::new(obj(kind, &s, name))
                        .with_detail(detail)
                        .with_comment(read::non_empty(&r, 2));
                    info.is_system = system;
                    out.push(info);
                }
            }
            ObjectKind::Trigger => {
                let sql = "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION \
                    FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? ORDER BY TRIGGER_NAME";
                for r in self.rows(sql, p).await? {
                    let table = read::text_or_empty(&r, 1);
                    let mut o = obj(ObjectKind::Trigger, &s, read::text_or_empty(&r, 0));
                    o.table = Some(table.clone());
                    let detail = format!(
                        "{} {} ON {table}",
                        read::text_or_empty(&r, 2),
                        read::text_or_empty(&r, 3)
                    );
                    let mut info = ObjectInfo::new(o).with_detail(detail);
                    info.is_system = system;
                    out.push(info);
                }
            }
            ObjectKind::Event => {
                let sql = "SELECT EVENT_NAME, EVENT_TYPE, STATUS, EVENT_COMMENT, INTERVAL_VALUE, INTERVAL_FIELD, EXECUTE_AT \
                    FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME";
                for r in self.rows(sql, p).await? {
                    let schedule = if read::text_or_empty(&r, 1).eq_ignore_ascii_case("RECURRING") {
                        format!(
                            "EVERY {} {}",
                            read::text_or_empty(&r, 4),
                            read::text_or_empty(&r, 5)
                        )
                    } else {
                        format!("AT {}", read::text_or_empty(&r, 6))
                    };
                    let detail = format!("{schedule} ({})", read::text_or_empty(&r, 2));
                    let mut info =
                        ObjectInfo::new(obj(ObjectKind::Event, &s, read::text_or_empty(&r, 0)))
                            .with_detail(detail)
                            .with_comment(read::non_empty(&r, 3));
                    info.is_system = system;
                    out.push(info);
                }
            }
            ObjectKind::Schema | ObjectKind::Database => {
                for sch in self.meta_list_schemas().await? {
                    let mut info =
                        ObjectInfo::new(ObjectRef::new(ObjectKind::Schema, None, sch.name.clone()));
                    info.is_system = sch.is_system;
                    out.push(info.with_comment(sch.comment));
                }
            }
            _ => {}
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ children

    pub(crate) async fn meta_list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let s = self.schema_or_current(parent.schema.as_deref()).await?;
        let table_name = if parent.kind.is_table_child() {
            parent.table.clone().unwrap_or_default()
        } else {
            parent.name.clone()
        };
        let mut parent_ref = ObjectRef::table(Some(s.clone()), table_name.clone());
        parent_ref.kind = if parent.kind == ObjectKind::View {
            ObjectKind::View
        } else {
            ObjectKind::Table
        };
        let scope = Scope {
            schema: &s,
            table: Some(&table_name),
        };
        let mut out = Vec::new();
        match kind {
            ObjectKind::Column => {
                let sql = "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, EXTRA, COLUMN_COMMENT, COLUMN_KEY \
                    FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION";
                for r in self.rows(sql, self.scope_params(&scope)).await? {
                    let mut detail = read::text_or_empty(&r, 1);
                    if !read::flag(&r, 2) {
                        detail.push_str(" NOT NULL");
                    }
                    let extra = read::text_or_empty(&r, 3);
                    if extra.to_ascii_lowercase().contains("auto_increment") {
                        detail.push_str(" auto_increment");
                    }
                    if read::text_or_empty(&r, 5) == "PRI" {
                        detail.push_str(" PK");
                    }
                    out.push(
                        ObjectInfo::new(ObjectRef::child(
                            ObjectKind::Column,
                            &parent_ref,
                            read::text_or_empty(&r, 0),
                        ))
                        .with_detail(detail)
                        .with_comment(read::non_empty(&r, 4)),
                    );
                }
            }
            ObjectKind::Index => {
                let t = self.load_scope(&scope, &only(|o| o.indexes = true)).await?;
                if let Some(td) = t.tables.into_iter().next() {
                    for idx in &td.indexes {
                        let cols = idx
                            .columns
                            .iter()
                            .map(|c| {
                                let mut s = if c.is_expression {
                                    format!("({})", c.name)
                                } else {
                                    c.name.clone()
                                };
                                if c.descending {
                                    s.push_str(" DESC");
                                }
                                s
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let mut detail = format!("({cols})");
                        if idx.unique {
                            detail.push_str(" UNIQUE");
                        }
                        if let Some(m) = &idx.method {
                            detail.push(' ');
                            detail.push_str(m);
                        }
                        out.push(
                            ObjectInfo::new(ObjectRef::child(
                                ObjectKind::Index,
                                &parent_ref,
                                idx.name.clone(),
                            ))
                            .with_detail(detail)
                            .with_comment(idx.comment.clone()),
                        );
                    }
                }
            }
            ObjectKind::PrimaryKey
            | ObjectKind::UniqueKey
            | ObjectKind::ForeignKey
            | ObjectKind::CheckConstraint => {
                let t = self
                    .load_scope(&scope, &only(|o| o.constraints = true))
                    .await?;
                if let Some(td) = t.tables.into_iter().next() {
                    match kind {
                        ObjectKind::PrimaryKey => {
                            if let Some(pk) = &td.primary_key {
                                out.push(
                                    ObjectInfo::new(ObjectRef::child(
                                        ObjectKind::PrimaryKey,
                                        &parent_ref,
                                        "PRIMARY",
                                    ))
                                    .with_detail(format!("({})", pk.columns.join(", "))),
                                );
                            }
                        }
                        ObjectKind::UniqueKey => {
                            for u in &td.unique_keys {
                                out.push(
                                    ObjectInfo::new(ObjectRef::child(
                                        ObjectKind::UniqueKey,
                                        &parent_ref,
                                        u.name.clone().unwrap_or_default(),
                                    ))
                                    .with_detail(format!("({})", u.columns.join(", "))),
                                );
                            }
                        }
                        ObjectKind::ForeignKey => {
                            for fk in &td.foreign_keys {
                                let target = match &fk.ref_schema {
                                    Some(rs) => format!("{rs}.{}", fk.ref_table),
                                    None => fk.ref_table.clone(),
                                };
                                let mut detail = format!(
                                    "({}) → {target}({})",
                                    fk.columns.join(", "),
                                    fk.ref_columns.join(", ")
                                );
                                if fk.on_delete != FkAction::NoAction {
                                    detail.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
                                }
                                if fk.on_update != FkAction::NoAction {
                                    detail.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
                                }
                                out.push(
                                    ObjectInfo::new(ObjectRef::child(
                                        ObjectKind::ForeignKey,
                                        &parent_ref,
                                        fk.name.clone().unwrap_or_default(),
                                    ))
                                    .with_detail(detail),
                                );
                            }
                        }
                        _ => {
                            for c in &td.checks {
                                out.push(
                                    ObjectInfo::new(ObjectRef::child(
                                        ObjectKind::CheckConstraint,
                                        &parent_ref,
                                        c.name.clone().unwrap_or_default(),
                                    ))
                                    .with_detail(c.expression.clone()),
                                );
                            }
                        }
                    }
                }
            }
            ObjectKind::Trigger => {
                let sql = "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION FROM information_schema.TRIGGERS \
                    WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER";
                for r in self.rows(sql, self.scope_params(&scope)).await? {
                    let detail = format!(
                        "{} {}",
                        read::text_or_empty(&r, 1),
                        read::text_or_empty(&r, 2)
                    );
                    out.push(
                        ObjectInfo::new(ObjectRef::child(
                            ObjectKind::Trigger,
                            &parent_ref,
                            read::text_or_empty(&r, 0),
                        ))
                        .with_detail(detail),
                    );
                }
            }
            ObjectKind::Partition => {
                let sql = "SELECT PARTITION_NAME, PARTITION_METHOD, PARTITION_DESCRIPTION, TABLE_ROWS, PARTITION_COMMENT \
                    FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND PARTITION_NAME IS NOT NULL \
                    AND (SUBPARTITION_ORDINAL_POSITION IS NULL OR SUBPARTITION_ORDINAL_POSITION = 1) \
                    ORDER BY PARTITION_ORDINAL_POSITION";
                for r in self.rows(sql, self.scope_params(&scope)).await? {
                    let method = read::text_or_empty(&r, 1);
                    let bound = partition_bound(&method, read::text(&r, 2).as_deref());
                    let mut info = ObjectInfo::new(ObjectRef::child(
                        ObjectKind::Partition,
                        &parent_ref,
                        read::text_or_empty(&r, 0),
                    ))
                    .with_detail(if bound.is_empty() { method } else { bound })
                    .with_comment(read::non_empty(&r, 4));
                    info.row_estimate = read::int(&r, 3);
                    out.push(info);
                }
            }
            _ => {}
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ loader

    async fn load_scope(&mut self, scope: &Scope<'_>, opts: &SnapshotOptions) -> DbResult<Loaded> {
        let profile = self.profile.clone();
        let params = self.scope_params(scope);
        // tables
        let sql = format!(
            "SELECT TABLE_NAME, TABLE_TYPE, ENGINE, TABLE_COLLATION, AUTO_INCREMENT, CREATE_OPTIONS, TABLE_COMMENT, \
             TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?{} ORDER BY TABLE_NAME",
            scope.filter("TABLE_NAME")
        );
        let table_rows = self.rows(&sql, params.clone()).await?;
        let mut tables: Vec<TableDef> = Vec::new();
        let mut views: Vec<ViewDef> = Vec::new();
        let mut sequences: Vec<SequenceDef> = Vec::new();
        let mut table_index: HashMap<String, usize> = HashMap::new();
        let mut view_index: HashMap<String, usize> = HashMap::new();
        for r in &table_rows {
            let name = read::text_or_empty(r, 0);
            let ttype = read::text_or_empty(r, 1).to_ascii_uppercase();
            match ttype.as_str() {
                "VIEW" | "SYSTEM VIEW" => {
                    view_index.insert(name.clone(), views.len());
                    views.push(ViewDef {
                        schema: Some(scope.schema.to_string()),
                        name,
                        ..Default::default()
                    });
                }
                "SEQUENCE" => {
                    sequences.push(SequenceDef {
                        schema: Some(scope.schema.to_string()),
                        name,
                        comment: read::non_empty(r, 6),
                        ..Default::default()
                    });
                }
                _ => {
                    let mut t = TableDef::new(Some(scope.schema.to_string()), name.clone());
                    if let Some(engine) = read::non_empty(r, 2) {
                        t.options.insert(option_keys::ENGINE.into(), engine);
                    }
                    if let Some(coll) = read::non_empty(r, 3) {
                        t.options
                            .insert(option_keys::CHARSET.into(), charset_of_collation(&coll));
                        t.options.insert(option_keys::COLLATION.into(), coll);
                    }
                    if let Some(ai) = read::int(r, 4) {
                        t.options
                            .insert(option_keys::AUTO_INCREMENT.into(), ai.to_string());
                    }
                    let create_options = read::text_or_empty(r, 5);
                    for (k, v) in parse_create_options(&create_options) {
                        t.options.insert(k, v);
                    }
                    if create_options.to_ascii_lowercase().contains("partitioned") {
                        t.table_kind = TableKind::PartitionedTable;
                    }
                    if opts.comments {
                        t.comment = read::non_empty(r, 6);
                    }
                    t.row_estimate = read::int(r, 7);
                    let data = read::int(r, 8);
                    let index = read::int(r, 9);
                    t.size_bytes = match (data, index) {
                        (None, None) => None,
                        (d, i) => Some(d.unwrap_or(0) + i.unwrap_or(0)),
                    };
                    if !opts.statistics && scope.table.is_none() {
                        t.row_estimate = None;
                        t.size_bytes = None;
                    }
                    table_index.insert(name, tables.len());
                    tables.push(t);
                }
            }
        }

        // columns (tables and views)
        if opts.columns {
            let sql = format!(
                "SELECT TABLE_NAME, COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, COLUMN_TYPE, \
                 COLLATION_NAME, EXTRA, COLUMN_COMMENT, GENERATION_EXPRESSION \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ?{} ORDER BY TABLE_NAME, ORDINAL_POSITION",
                scope.filter("TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let tname = read::text_or_empty(&r, 0);
                let data_type = read::text_or_empty(&r, 5);
                let extra = read::text_or_empty(&r, 7);
                let extra_l = extra.to_ascii_lowercase();
                let name = read::text_or_empty(&r, 1);
                let mut c = ColumnDef::new(name.clone(), data_type.clone());
                c.kind = type_kind(&data_type);
                c.ordinal = read::int(&r, 2).unwrap_or(0) as u32;
                c.nullable = read::flag(&r, 4);
                c.auto_increment = extra_l.contains("auto_increment");
                c.collation = read::non_empty(&r, 6);
                if opts.comments {
                    c.comment = read::non_empty(&r, 8);
                }
                let generated = read::non_empty(&r, 9).filter(|g| !g.eq_ignore_ascii_case("NULL"));
                let is_generated = extra_l.contains("generated")
                    && !extra_l.contains("default_generated")
                    || generated.is_some();
                let mut column_options: Vec<(String, String)> = Vec::new();
                if is_generated {
                    if let Some(g) = generated {
                        c.generated = Some(sqltext::strip_outer_parens(&g).to_string());
                    }
                    let storage = if extra_l.contains("stored") || extra_l.contains("persistent") {
                        "STORED"
                    } else {
                        "VIRTUAL"
                    };
                    column_options.push((
                        format!("{}{name}", option_keys::GENERATED_PREFIX),
                        storage.to_string(),
                    ));
                } else {
                    c.default = normalize_default(&profile, &data_type, read::text(&r, 3), &extra);
                }
                if extra_l.split_whitespace().any(|w| w == "invisible") {
                    column_options.push((
                        format!("{}{name}", option_keys::INVISIBLE_PREFIX),
                        "1".into(),
                    ));
                }
                if let Some(u) = on_update_from_extra(&extra) {
                    column_options.push((format!("{}{name}", option_keys::ON_UPDATE_PREFIX), u));
                }
                if let Some(&i) = table_index.get(&tname) {
                    let t = &mut tables[i];
                    for (k, v) in column_options {
                        t.options.insert(k, v);
                    }
                    t.columns.push(c);
                } else if let Some(&i) = view_index.get(&tname) {
                    views[i].columns.push(c);
                }
            }
        }

        // indexes
        if opts.indexes {
            let functional = profile.has_functional_indexes();
            let sql = format!(
                "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME, COLLATION, SUB_PART, INDEX_TYPE, INDEX_COMMENT{} \
                 FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ?{} \
                 ORDER BY TABLE_NAME, INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
                if functional { ", EXPRESSION" } else { "" },
                scope.filter("TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let tname = read::text_or_empty(&r, 0);
                let Some(&ti) = table_index.get(&tname) else {
                    continue;
                };
                let iname = read::text_or_empty(&r, 1);
                let t = &mut tables[ti];
                let pos = match t.indexes.iter().position(|i| i.name == iname) {
                    Some(p) => p,
                    None => {
                        t.indexes.push(IndexDef {
                            name: iname.clone(),
                            unique: read::int(&r, 2) == Some(0),
                            primary: iname == "PRIMARY",
                            method: read::non_empty(&r, 6),
                            comment: if opts.comments {
                                read::non_empty(&r, 7)
                            } else {
                                None
                            },
                            ..Default::default()
                        });
                        t.indexes.len() - 1
                    }
                };
                let mut idx = std::mem::take(&mut t.indexes[pos]);
                add_index_column(t, &mut idx, &r, functional);
                t.indexes[pos] = idx;
            }
        }

        // constraints
        if opts.constraints {
            let sql = format!(
                "SELECT tc.TABLE_NAME, tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, \
                 k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
                 FROM information_schema.TABLE_CONSTRAINTS tc \
                 JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                   AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND k.TABLE_SCHEMA = tc.TABLE_SCHEMA AND k.TABLE_NAME = tc.TABLE_NAME \
                 LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS rc ON rc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                   AND rc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND rc.TABLE_NAME = tc.TABLE_NAME \
                 WHERE tc.TABLE_SCHEMA = ?{} AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY','UNIQUE','FOREIGN KEY') \
                 ORDER BY tc.TABLE_NAME, tc.CONSTRAINT_TYPE, tc.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                scope.filter("tc.TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let tname = read::text_or_empty(&r, 0);
                let Some(&ti) = table_index.get(&tname) else {
                    continue;
                };
                let cname = read::text_or_empty(&r, 1);
                let ctype = read::text_or_empty(&r, 2).to_ascii_uppercase();
                let col = read::text_or_empty(&r, 3);
                let t = &mut tables[ti];
                match ctype.as_str() {
                    "PRIMARY KEY" => t
                        .primary_key
                        .get_or_insert_with(|| KeyDef {
                            name: None,
                            columns: Vec::new(),
                        })
                        .columns
                        .push(col),
                    "UNIQUE" => match t
                        .unique_keys
                        .iter_mut()
                        .find(|u| u.name.as_deref() == Some(cname.as_str()))
                    {
                        Some(u) => u.columns.push(col),
                        None => t.unique_keys.push(KeyDef {
                            name: Some(cname),
                            columns: vec![col],
                        }),
                    },
                    _ => {
                        let ref_schema = read::non_empty(&r, 4).filter(|rs| rs != scope.schema);
                        match t
                            .foreign_keys
                            .iter_mut()
                            .find(|f| f.name.as_deref() == Some(cname.as_str()))
                        {
                            Some(fk) => {
                                fk.columns.push(col);
                                fk.ref_columns.push(read::text_or_empty(&r, 6));
                            }
                            None => t.foreign_keys.push(ForeignKeyDef {
                                name: Some(cname),
                                schema: Some(scope.schema.to_string()),
                                table: tname.clone(),
                                columns: vec![col],
                                ref_schema,
                                ref_table: read::text_or_empty(&r, 5),
                                ref_columns: vec![read::text_or_empty(&r, 6)],
                                on_update: FkAction::parse(&read::text_or_empty(&r, 7)),
                                on_delete: FkAction::parse(&read::text_or_empty(&r, 8)),
                                deferrable: false,
                            }),
                        }
                    }
                }
            }
            // check constraints
            if profile.has_check_constraints() {
                let sql = if profile.check_constraints_have_table() {
                    format!(
                        "SELECT TABLE_NAME, CONSTRAINT_NAME, CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS \
                         WHERE CONSTRAINT_SCHEMA = ?{} ORDER BY TABLE_NAME, CONSTRAINT_NAME",
                        scope.filter("TABLE_NAME")
                    )
                } else {
                    format!(
                        "SELECT tc.TABLE_NAME, cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.TABLE_CONSTRAINTS tc \
                         JOIN information_schema.CHECK_CONSTRAINTS cc ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                           AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                         WHERE tc.TABLE_SCHEMA = ?{} AND tc.CONSTRAINT_TYPE = 'CHECK' ORDER BY tc.TABLE_NAME, cc.CONSTRAINT_NAME",
                        scope.filter("tc.TABLE_NAME")
                    )
                };
                if let Some(rows) = self.try_rows(&sql, params.clone()).await? {
                    for r in rows {
                        let tname = read::text_or_empty(&r, 0);
                        let Some(&ti) = table_index.get(&tname) else {
                            continue;
                        };
                        let name = read::text_or_empty(&r, 1);
                        let clause = read::text_or_empty(&r, 2);
                        let expression = sqltext::strip_outer_parens(&clause).to_string();
                        let t = &mut tables[ti];
                        // MariaDB JSON columns are LONGTEXT with a json_valid() check.
                        if profile.is_mariadb()
                            && expression.to_ascii_lowercase().starts_with("json_valid(")
                            && let Some(c) = t.columns.iter_mut().find(|c| c.name == name)
                        {
                            c.kind = DataKind::Json;
                        }
                        t.checks.push(CheckDef {
                            name: Some(name),
                            expression,
                        });
                    }
                }
            }
            // foreign keys of other tables referencing these tables
            let sql = format!(
                "SELECT k.REFERENCED_TABLE_NAME, k.CONSTRAINT_NAME, k.TABLE_SCHEMA, k.TABLE_NAME, k.COLUMN_NAME, \
                 k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
                 FROM information_schema.KEY_COLUMN_USAGE k \
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
                   AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND rc.TABLE_NAME = k.TABLE_NAME \
                 WHERE k.REFERENCED_TABLE_SCHEMA = ?{} \
                 ORDER BY k.REFERENCED_TABLE_NAME, k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                scope.filter("k.REFERENCED_TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let target = read::text_or_empty(&r, 0);
                let Some(&ti) = table_index.get(&target) else {
                    continue;
                };
                let cname = read::text_or_empty(&r, 1);
                let owner_schema = read::text_or_empty(&r, 2);
                let owner = read::text_or_empty(&r, 3);
                let t = &mut tables[ti];
                match t.referenced_by.iter_mut().find(|f| {
                    f.name.as_deref() == Some(cname.as_str())
                        && f.table == owner
                        && f.schema.as_deref() == Some(owner_schema.as_str())
                }) {
                    Some(fk) => {
                        fk.columns.push(read::text_or_empty(&r, 4));
                        fk.ref_columns.push(read::text_or_empty(&r, 5));
                    }
                    None => t.referenced_by.push(ForeignKeyDef {
                        name: Some(cname),
                        schema: Some(owner_schema),
                        table: owner,
                        columns: vec![read::text_or_empty(&r, 4)],
                        ref_schema: Some(scope.schema.to_string()),
                        ref_table: target.clone(),
                        ref_columns: vec![read::text_or_empty(&r, 5)],
                        on_update: FkAction::parse(&read::text_or_empty(&r, 6)),
                        on_delete: FkAction::parse(&read::text_or_empty(&r, 7)),
                        deferrable: false,
                    }),
                }
            }
        }

        // triggers
        if opts.triggers {
            let sql = format!(
                "SELECT EVENT_OBJECT_TABLE, TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT \
                 FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ?{} \
                 ORDER BY EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER",
                scope.filter("EVENT_OBJECT_TABLE")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let tname = read::text_or_empty(&r, 0);
                let Some(&ti) = table_index.get(&tname) else {
                    continue;
                };
                let name = read::text_or_empty(&r, 1);
                let timing = read::text_or_empty(&r, 2);
                let event = read::text_or_empty(&r, 3);
                let body = read::text_or_empty(&r, 4);
                let definition = format!(
                    "CREATE TRIGGER {} {timing} {event} ON {} FOR EACH ROW {body}",
                    qualified(Some(scope.schema), &name),
                    qualified(Some(scope.schema), &tname)
                );
                tables[ti].triggers.push(TriggerDef {
                    name,
                    timing,
                    events: vec![event],
                    for_each_row: true,
                    enabled: true,
                    definition,
                    body: Some(body),
                    comment: None,
                });
            }
        }

        // partitions
        if opts.partitions {
            let sql = format!(
                "SELECT TABLE_NAME, PARTITION_NAME, PARTITION_METHOD, PARTITION_EXPRESSION, PARTITION_DESCRIPTION, TABLE_ROWS \
                 FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = ?{} AND PARTITION_NAME IS NOT NULL \
                 AND (SUBPARTITION_ORDINAL_POSITION IS NULL OR SUBPARTITION_ORDINAL_POSITION = 1) \
                 ORDER BY TABLE_NAME, PARTITION_ORDINAL_POSITION",
                scope.filter("TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let tname = read::text_or_empty(&r, 0);
                let Some(&ti) = table_index.get(&tname) else {
                    continue;
                };
                let method = read::text_or_empty(&r, 2);
                let t = &mut tables[ti];
                if t.partitioning.is_none() {
                    t.partitioning = Some(PartitioningDef {
                        strategy: method.clone(),
                        key: read::text_or_empty(&r, 3),
                    });
                    t.table_kind = TableKind::PartitionedTable;
                }
                t.partitions.push(PartitionDef {
                    name: read::text_or_empty(&r, 1),
                    bound: partition_bound(&method, read::text(&r, 4).as_deref()),
                    row_estimate: read::int(&r, 5),
                });
            }
        }

        // views
        if !views.is_empty() && opts.views {
            let algo = if profile.is_mariadb() {
                "ALGORITHM"
            } else {
                "NULL"
            };
            let sql = format!(
                "SELECT TABLE_NAME, VIEW_DEFINITION, CHECK_OPTION, IS_UPDATABLE, DEFINER, SECURITY_TYPE, {algo} \
                 FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ?{} ORDER BY TABLE_NAME",
                scope.filter("TABLE_NAME")
            );
            for r in self.rows(&sql, params.clone()).await? {
                let vname = read::text_or_empty(&r, 0);
                let Some(&vi) = view_index.get(&vname) else {
                    continue;
                };
                let v = &mut views[vi];
                if opts.view_definitions {
                    v.definition =
                        sqltext::strip_schema_qualifier(&read::text_or_empty(&r, 1), scope.schema);
                }
                if let Some(co) = read::non_empty(&r, 2) {
                    v.options.insert("check_option".into(), co);
                }
                v.options
                    .insert("updatable".into(), read::text_or_empty(&r, 3));
                if let Some(d) = read::non_empty(&r, 4) {
                    v.options.insert("definer".into(), d);
                }
                if let Some(sec) = read::non_empty(&r, 5) {
                    v.options.insert("security".into(), sec);
                }
                if let Some(a) = read::non_empty(&r, 6) {
                    v.options.insert("algorithm".into(), a);
                }
            }
        }

        // sequences (MariaDB)
        if !sequences.is_empty() && opts.sequences {
            self.load_sequences(scope.schema, &mut sequences).await?;
        }

        if !opts.comments {
            for v in views.iter_mut() {
                v.comment = None;
            }
        }
        Ok(Loaded {
            tables,
            views,
            sequences,
        })
    }

    async fn load_sequences(
        &mut self,
        schema: &str,
        sequences: &mut [SequenceDef],
    ) -> DbResult<()> {
        if self.profile.has_sequences_view() {
            let sql = "SELECT SEQUENCE_NAME, DATA_TYPE, START_VALUE, MINIMUM_VALUE, MAXIMUM_VALUE, INCREMENT, CYCLE_OPTION \
                FROM information_schema.SEQUENCES WHERE SEQUENCE_SCHEMA = ?";
            if let Some(rows) = self.try_rows(sql, vec![self.p(schema)]).await? {
                for r in rows {
                    let name = read::text_or_empty(&r, 0);
                    if let Some(s) = sequences.iter_mut().find(|s| s.name == name) {
                        s.data_type = read::non_empty(&r, 1);
                        s.start = read::int(&r, 2);
                        s.min_value = read::int(&r, 3);
                        s.max_value = read::int(&r, 4);
                        s.increment = read::int(&r, 5);
                        s.cycle = read::flag(&r, 6);
                    }
                }
            }
        }
        for s in sequences.iter_mut() {
            let sql = format!(
                "SELECT start_value, minimum_value, maximum_value, increment, cache_size, cycle_option, next_not_cached_value FROM {}",
                qualified(Some(schema), &s.name)
            );
            if let Some(rows) = self.try_rows(&sql, Vec::new()).await?
                && let Some(r) = rows.first()
            {
                s.start = read::int(r, 0).or(s.start);
                s.min_value = read::int(r, 1).or(s.min_value);
                s.max_value = read::int(r, 2).or(s.max_value);
                s.increment = read::int(r, 3).or(s.increment);
                s.cache = read::int(r, 4);
                s.cycle = read::flag(r, 5);
                s.last_value = read::int(r, 6);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- definitions

    pub(crate) async fn meta_table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let s = self.schema_or_current(table.schema.as_deref()).await?;
        let name = if table.kind.is_table_child() {
            table.table.clone().unwrap_or_default()
        } else {
            table.name.clone()
        };
        let old = self.fresh_stats_begin().await;
        let res = self
            .load_scope(
                &Scope {
                    schema: &s,
                    table: Some(&name),
                },
                &SnapshotOptions::full(),
            )
            .await;
        self.fresh_stats_end(old).await;
        let loaded = res?;
        loaded.tables.into_iter().next().ok_or_else(|| {
            DbError::query(format!("Table {s}.{name} does not exist")).with_code("1146")
        })
    }

    pub(crate) async fn meta_view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let s = self.schema_or_current(view.schema.as_deref()).await?;
        let mut loaded = self
            .load_scope(
                &Scope {
                    schema: &s,
                    table: Some(&view.name),
                },
                &SnapshotOptions::full(),
            )
            .await?;
        let mut v = loaded.views.pop().ok_or_else(|| {
            DbError::query(format!("View {s}.{} does not exist", view.name)).with_code("1146")
        })?;
        if self.profile.is_mysql() {
            let sql = format!("SHOW CREATE VIEW {}", qualified(Some(&s), &view.name));
            if let Some(rows) = self.try_rows(&sql, Vec::new()).await?
                && let Some(text) = rows.first().and_then(|r| read::text(r, 1))
                && let Some(a) = text
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("ALGORITHM="))
            {
                v.options.insert("algorithm".into(), a.to_string());
            }
        }
        Ok(v)
    }

    pub(crate) async fn meta_routine_definition(
        &mut self,
        routine: &ObjectRef,
    ) -> DbResult<RoutineDef> {
        let s = self.schema_or_current(routine.schema.as_deref()).await?;
        let rtype = match routine.kind {
            ObjectKind::Procedure => "PROCEDURE",
            _ => "FUNCTION",
        };
        let mut routines = self.load_routines(&s, Some((&routine.name, rtype))).await?;
        let mut r = routines.pop().ok_or_else(|| {
            DbError::query(format!(
                "{} {s}.{} does not exist",
                rtype.to_ascii_lowercase(),
                routine.name
            ))
            .with_code("1305")
        })?;
        let sql = format!("SHOW CREATE {rtype} {}", qualified(Some(&s), &routine.name));
        if let Some(rows) = self.try_rows(&sql, Vec::new()).await?
            && let Some(text) = rows.first().and_then(|row| read::non_empty(row, 2))
        {
            r.definition = text;
        }
        Ok(r)
    }

    /// Routines of a schema (one routine when `only` is given), with
    /// definitions reconstructed from `information_schema` (no DEFINER).
    async fn load_routines(
        &mut self,
        schema: &str,
        only: Option<(&str, &str)>,
    ) -> DbResult<Vec<RoutineDef>> {
        let mut params = vec![self.p(schema)];
        let filter = match only {
            Some((name, rtype)) => {
                params.push(self.p(name));
                params.push(self.p(rtype));
                " AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?"
            }
            None => " AND ROUTINE_TYPE IN ('FUNCTION','PROCEDURE')",
        };
        let sql = format!(
            "SELECT SPECIFIC_NAME, ROUTINE_NAME, ROUTINE_TYPE, DTD_IDENTIFIER, ROUTINE_DEFINITION, IS_DETERMINISTIC, \
             SQL_DATA_ACCESS, SECURITY_TYPE, ROUTINE_COMMENT, ROUTINE_BODY \
             FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ?{filter} ORDER BY ROUTINE_NAME"
        );
        let rows = self.rows(&sql, params.clone()).await?;
        let psql = format!(
            "SELECT SPECIFIC_NAME, ROUTINE_TYPE, PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER FROM information_schema.PARAMETERS \
             WHERE SPECIFIC_SCHEMA = ? AND ORDINAL_POSITION > 0{} ORDER BY SPECIFIC_NAME, ORDINAL_POSITION",
            if only.is_some() {
                " AND SPECIFIC_NAME = ? AND ROUTINE_TYPE = ?"
            } else {
                ""
            }
        );
        let prows = self.try_rows(&psql, params).await?.unwrap_or_default();
        let mut args: HashMap<(String, String), Vec<String>> = HashMap::new();
        for p in &prows {
            let key = (read::text_or_empty(p, 0), read::text_or_empty(p, 1));
            let mut a = String::new();
            if let Some(mode) = read::non_empty(p, 2)
                && key.1 == "PROCEDURE"
            {
                a.push_str(&mode);
                a.push(' ');
            }
            a.push_str(&quote(&read::text_or_empty(p, 3)));
            a.push(' ');
            a.push_str(&read::text_or_empty(p, 4));
            args.entry(key).or_default().push(a);
        }
        let mut out = Vec::new();
        for r in rows {
            let specific = read::text_or_empty(&r, 0);
            let name = read::text_or_empty(&r, 1);
            let rtype = read::text_or_empty(&r, 2).to_ascii_uppercase();
            let kind = if rtype == "PROCEDURE" {
                ObjectKind::Procedure
            } else {
                ObjectKind::Function
            };
            let arguments = args
                .get(&(specific, rtype.clone()))
                .map(|v| v.join(", "))
                .unwrap_or_default();
            let returns = if kind == ObjectKind::Function {
                read::non_empty(&r, 3)
            } else {
                None
            };
            let body = read::text(&r, 4);
            let comment = read::non_empty(&r, 8);
            let definition = match &body {
                Some(b) => {
                    let mut d = format!("CREATE {rtype} {}({arguments})", quote(&name));
                    if let Some(ret) = &returns {
                        d.push_str(&format!(" RETURNS {ret}"));
                    }
                    if let Some(c) = &comment {
                        d.push_str(&format!("\n    COMMENT {}", ddl::string_literal(c)));
                    }
                    if read::flag(&r, 5) {
                        d.push_str("\n    DETERMINISTIC");
                    }
                    if let Some(access) =
                        read::non_empty(&r, 6).filter(|a| !a.eq_ignore_ascii_case("CONTAINS SQL"))
                    {
                        d.push_str(&format!("\n    {access}"));
                    }
                    if read::text_or_empty(&r, 7).eq_ignore_ascii_case("INVOKER") {
                        d.push_str("\n    SQL SECURITY INVOKER");
                    }
                    d.push('\n');
                    d.push_str(b);
                    d
                }
                None => String::new(),
            };
            out.push(RoutineDef {
                schema: Some(schema.to_string()),
                name,
                kind: Some(kind),
                arguments,
                returns,
                language: Some(read::non_empty(&r, 9).unwrap_or_else(|| "SQL".into())),
                definition,
                comment,
            });
        }
        Ok(out)
    }

    pub(crate) async fn meta_sequence_definition(
        &mut self,
        sequence: &ObjectRef,
    ) -> DbResult<SequenceDef> {
        let s = self.schema_or_current(sequence.schema.as_deref()).await?;
        if !self.profile.has_sequences() {
            return Err(DbError::unsupported(
                "Sequences require MariaDB 10.3 or later",
            ));
        }
        let mut loaded = self
            .load_scope(
                &Scope {
                    schema: &s,
                    table: Some(&sequence.name),
                },
                &SnapshotOptions::full(),
            )
            .await?;
        loaded.sequences.pop().ok_or_else(|| {
            DbError::query(format!("Sequence {s}.{} does not exist", sequence.name))
                .with_code("1146")
        })
    }

    // --------------------------------------------------------------------- DDL

    async fn show_create(&mut self, what: &str, target: &str, col: usize) -> DbResult<String> {
        let sql = format!("SHOW CREATE {what} {target}");
        let row = self.row(&sql, Vec::new()).await?;
        row.and_then(|r| read::non_empty(&r, col)).ok_or_else(|| {
            DbError::query(format!(
                "The server returned no definition for {target} (missing privileges to view it?)"
            ))
        })
    }

    pub(crate) async fn meta_object_ddl(&mut self, o: &ObjectRef) -> DbResult<String> {
        let s = match o.kind {
            ObjectKind::Database | ObjectKind::Schema => String::new(),
            _ => self.schema_or_current(o.schema.as_deref()).await?,
        };
        let name = qualified(Some(&s), &o.name);
        Ok(match o.kind {
            ObjectKind::Table | ObjectKind::ForeignTable => {
                format!("{};", self.show_create("TABLE", &name, 1).await?)
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                format!("{};", self.show_create("VIEW", &name, 1).await?)
            }
            ObjectKind::Procedure => self.show_create("PROCEDURE", &name, 2).await?,
            ObjectKind::Function | ObjectKind::Aggregate => {
                self.show_create("FUNCTION", &name, 2).await?
            }
            ObjectKind::Trigger => self.show_create("TRIGGER", &name, 2).await?,
            ObjectKind::Event => self.show_create("EVENT", &name, 3).await?,
            ObjectKind::Sequence => format!("{};", self.show_create("SEQUENCE", &name, 1).await?),
            ObjectKind::Database | ObjectKind::Schema => {
                format!(
                    "{};",
                    self.show_create(
                        "DATABASE",
                        &quote(o.schema.as_deref().unwrap_or(&o.name)),
                        1
                    )
                    .await?
                )
            }
            ObjectKind::Column
            | ObjectKind::Index
            | ObjectKind::PrimaryKey
            | ObjectKind::UniqueKey
            | ObjectKind::CheckConstraint
            | ObjectKind::ForeignKey
            | ObjectKind::Partition => {
                let table_name = o.table.clone().unwrap_or_default();
                let t = self
                    .meta_table_definition(&ObjectRef::table(Some(s.clone()), table_name))
                    .await?;
                let g = self.ddl_gen();
                let stmt = match o.kind {
                    ObjectKind::Column => t.column(&o.name).map(|c| {
                        format!(
                            "ALTER TABLE {} ADD COLUMN {}",
                            qualified(Some(&s), &t.name),
                            g.column_definition(&t, c)
                        )
                    }),
                    ObjectKind::Index => t.indexes.iter().find(|i| i.name == o.name).map(|i| {
                        if i.primary {
                            t.primary_key
                                .as_ref()
                                .map(|pk| g.add_primary_key(&t, pk))
                                .unwrap_or_default()
                        } else {
                            g.create_index(&t, i)
                        }
                    }),
                    ObjectKind::PrimaryKey => {
                        t.primary_key.as_ref().map(|pk| g.add_primary_key(&t, pk))
                    }
                    ObjectKind::UniqueKey => t
                        .unique_keys
                        .iter()
                        .find(|u| u.name.as_deref() == Some(o.name.as_str()))
                        .map(|u| g.add_unique_key(&t, u)),
                    ObjectKind::CheckConstraint => t
                        .checks
                        .iter()
                        .find(|c| c.name.as_deref() == Some(o.name.as_str()))
                        .map(|c| g.add_check(&t, c)),
                    ObjectKind::ForeignKey => t
                        .foreign_keys
                        .iter()
                        .find(|f| f.name.as_deref() == Some(o.name.as_str()))
                        .map(|f| g.add_foreign_key(&t, f)),
                    _ => g
                        .partition_clause(&t)
                        .map(|p| format!("ALTER TABLE {} {p}", qualified(Some(&s), &t.name))),
                };
                format!(
                    "{};",
                    stmt.filter(|x| !x.is_empty())
                        .ok_or_else(|| DbError::query(format!(
                            "{} {} not found",
                            o.kind.label(),
                            o.display_path()
                        )))?
                )
            }
            other => {
                return Err(DbError::unsupported(format!(
                    "DDL of {} objects is not available for MySQL",
                    other.label()
                )));
            }
        })
    }

    // -------------------------------------------------------------- properties

    async fn props_from_row(
        &mut self,
        sql: &str,
        params: Vec<MyValue>,
        labels: &[(&str, &str)],
    ) -> DbResult<Vec<Property>> {
        let Some(row) = self.row(sql, params).await? else {
            return Ok(Vec::new());
        };
        Ok(labels
            .iter()
            .enumerate()
            .filter_map(|(i, (group, label))| {
                read::text(&row, i).map(|v| {
                    if group.is_empty() {
                        Property::new(*label, v)
                    } else {
                        Property::grouped(*group, *label, v)
                    }
                })
            })
            .collect())
    }

    pub(crate) async fn meta_object_properties(
        &mut self,
        o: &ObjectRef,
    ) -> DbResult<Vec<Property>> {
        let s = match o.kind {
            ObjectKind::Database | ObjectKind::Schema => {
                o.schema.clone().unwrap_or_else(|| o.name.clone())
            }
            _ => self.schema_or_current(o.schema.as_deref()).await?,
        };
        match o.kind {
            ObjectKind::Table | ObjectKind::Partition | ObjectKind::Sequence => {
                let old = self.fresh_stats_begin().await;
                let table = if o.kind == ObjectKind::Partition { o.table.clone().unwrap_or_default() } else { o.name.clone() };
                let res = self
                    .props_from_row(
                        "SELECT TABLE_TYPE, ENGINE, ROW_FORMAT, TABLE_COLLATION, TABLE_ROWS, AVG_ROW_LENGTH, DATA_LENGTH, \
                         INDEX_LENGTH, DATA_FREE, AUTO_INCREMENT, CREATE_TIME, UPDATE_TIME, CHECK_TIME, CREATE_OPTIONS, TABLE_COMMENT \
                         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                        vec![self.p(&s), self.p(&table)],
                        &[
                            ("General", "Type"),
                            ("General", "Engine"),
                            ("Storage", "Row format"),
                            ("General", "Collation"),
                            ("Statistics", "Rows (estimate)"),
                            ("Statistics", "Average row length"),
                            ("Storage", "Data length"),
                            ("Storage", "Index length"),
                            ("Storage", "Data free"),
                            ("General", "Auto increment"),
                            ("Dates", "Created"),
                            ("Dates", "Updated"),
                            ("Dates", "Checked"),
                            ("General", "Create options"),
                            ("General", "Comment"),
                        ],
                    )
                    .await;
                self.fresh_stats_end(old).await;
                res
            }
            ObjectKind::View => {
                let algo = if self.profile.is_mariadb() { "ALGORITHM" } else { "NULL" };
                let sql = format!(
                    "SELECT DEFINER, SECURITY_TYPE, CHECK_OPTION, IS_UPDATABLE, CHARACTER_SET_CLIENT, COLLATION_CONNECTION, {algo} \
                     FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?"
                );
                self.props_from_row(
                    &sql,
                    vec![self.p(&s), self.p(&o.name)],
                    &[
                        ("", "Definer"),
                        ("", "Security"),
                        ("", "Check option"),
                        ("", "Updatable"),
                        ("", "Client character set"),
                        ("", "Connection collation"),
                        ("", "Algorithm"),
                    ],
                )
                .await
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let rtype = if o.kind == ObjectKind::Procedure { "PROCEDURE" } else { "FUNCTION" };
                self.props_from_row(
                    "SELECT ROUTINE_TYPE, DTD_IDENTIFIER, DEFINER, SECURITY_TYPE, SQL_DATA_ACCESS, IS_DETERMINISTIC, CREATED, \
                     LAST_ALTERED, SQL_MODE, ROUTINE_COMMENT, CHARACTER_SET_CLIENT, COLLATION_CONNECTION, DATABASE_COLLATION \
                     FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?",
                    vec![self.p(&s), self.p(&o.name), self.p(rtype)],
                    &[
                        ("", "Type"),
                        ("", "Returns"),
                        ("", "Definer"),
                        ("", "Security"),
                        ("", "SQL data access"),
                        ("", "Deterministic"),
                        ("", "Created"),
                        ("", "Last altered"),
                        ("", "SQL mode"),
                        ("", "Comment"),
                        ("", "Client character set"),
                        ("", "Connection collation"),
                        ("", "Database collation"),
                    ],
                )
                .await
            }
            ObjectKind::Trigger => {
                self.props_from_row(
                    "SELECT EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER, DEFINER, SQL_MODE, CREATED, \
                     CHARACTER_SET_CLIENT, COLLATION_CONNECTION FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? AND TRIGGER_NAME = ?",
                    vec![self.p(&s), self.p(&o.name)],
                    &[
                        ("", "Table"),
                        ("", "Timing"),
                        ("", "Event"),
                        ("", "Order"),
                        ("", "Definer"),
                        ("", "SQL mode"),
                        ("", "Created"),
                        ("", "Client character set"),
                        ("", "Connection collation"),
                    ],
                )
                .await
            }
            ObjectKind::Event => {
                self.props_from_row(
                    "SELECT DEFINER, TIME_ZONE, EVENT_TYPE, EXECUTE_AT, INTERVAL_VALUE, INTERVAL_FIELD, STARTS, ENDS, STATUS, \
                     ON_COMPLETION, CREATED, LAST_ALTERED, LAST_EXECUTED, EVENT_COMMENT, ORIGINATOR \
                     FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? AND EVENT_NAME = ?",
                    vec![self.p(&s), self.p(&o.name)],
                    &[
                        ("", "Definer"),
                        ("", "Time zone"),
                        ("", "Type"),
                        ("", "Execute at"),
                        ("", "Interval value"),
                        ("", "Interval field"),
                        ("", "Starts"),
                        ("", "Ends"),
                        ("", "Status"),
                        ("", "On completion"),
                        ("", "Created"),
                        ("", "Last altered"),
                        ("", "Last executed"),
                        ("", "Comment"),
                        ("", "Originator"),
                    ],
                )
                .await
            }
            ObjectKind::Database | ObjectKind::Schema => {
                let mut props = self
                    .props_from_row(
                        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
                        vec![self.p(&s)],
                        &[("", "Default character set"), ("", "Default collation")],
                    )
                    .await?;
                props.extend(
                    self.props_from_row(
                        "SELECT COUNT(*), SUM(TABLE_TYPE = 'VIEW'), SUM(IFNULL(DATA_LENGTH, 0) + IFNULL(INDEX_LENGTH, 0)) \
                         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
                        vec![self.p(&s)],
                        &[("Statistics", "Tables and views"), ("Statistics", "Views"), ("Statistics", "Size (bytes)")],
                    )
                    .await?,
                );
                Ok(props)
            }
            ObjectKind::Column => {
                let table = o.table.clone().unwrap_or_default();
                self.props_from_row(
                    "SELECT COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_KEY, CHARACTER_SET_NAME, COLLATION_NAME, \
                     GENERATION_EXPRESSION, PRIVILEGES, COLUMN_COMMENT, ORDINAL_POSITION FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
                    vec![self.p(&s), self.p(&table), self.p(&o.name)],
                    &[
                        ("", "Type"),
                        ("", "Nullable"),
                        ("", "Default"),
                        ("", "Extra"),
                        ("", "Key"),
                        ("", "Character set"),
                        ("", "Collation"),
                        ("", "Generation expression"),
                        ("", "Privileges"),
                        ("", "Comment"),
                        ("", "Position"),
                    ],
                )
                .await
            }
            ObjectKind::Index | ObjectKind::PrimaryKey | ObjectKind::UniqueKey => {
                let table = o.table.clone().unwrap_or_default();
                let iname = if o.kind == ObjectKind::PrimaryKey { "PRIMARY".to_string() } else { o.name.clone() };
                let rows = self
                    .rows(
                        "SELECT INDEX_TYPE, NON_UNIQUE, COLUMN_NAME, CARDINALITY, SUB_PART, NULLABLE, INDEX_COMMENT \
                         FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? ORDER BY SEQ_IN_INDEX",
                        vec![self.p(&s), self.p(&table), self.p(&iname)],
                    )
                    .await?;
                let mut props = Vec::new();
                if let Some(first) = rows.first() {
                    props.push(Property::new("Type", read::text_or_empty(first, 0)));
                    props.push(Property::new("Unique", if read::int(first, 1) == Some(0) { "YES" } else { "NO" }));
                    if let Some(c) = read::non_empty(first, 6) {
                        props.push(Property::new("Comment", c));
                    }
                }
                for r in &rows {
                    let col = read::text_or_empty(r, 2);
                    let mut v = format!("cardinality {}", read::text(r, 3).unwrap_or_else(|| "?".into()));
                    if let Some(sp) = read::int(r, 4) {
                        v.push_str(&format!(", prefix {sp}"));
                    }
                    props.push(Property::grouped("Columns", col, v));
                }
                Ok(props)
            }
            ObjectKind::ForeignKey => {
                let table = o.table.clone().unwrap_or_default();
                self.props_from_row(
                    "SELECT REFERENCED_TABLE_NAME, UNIQUE_CONSTRAINT_NAME, MATCH_OPTION, UPDATE_RULE, DELETE_RULE \
                     FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_NAME = ?",
                    vec![self.p(&s), self.p(&table), self.p(&o.name)],
                    &[("", "Referenced table"), ("", "Referenced key"), ("", "Match"), ("", "On update"), ("", "On delete")],
                )
                .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub(crate) async fn meta_object_statistics(
        &mut self,
        o: &ObjectRef,
    ) -> DbResult<Vec<Property>> {
        let s = match o.kind {
            ObjectKind::Database | ObjectKind::Schema => {
                o.schema.clone().unwrap_or_else(|| o.name.clone())
            }
            _ => self.schema_or_current(o.schema.as_deref()).await?,
        };
        match o.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence => {
                let pattern = o.name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                let literal = self.mysql_dialect().string_literal(&pattern);
                let sql = format!("SHOW TABLE STATUS FROM {} LIKE {literal}", quote(&s));
                let old = self.fresh_stats_begin().await;
                let res = self.rows_with_columns(&sql).await;
                self.fresh_stats_end(old).await;
                let (cols, rows) = res?;
                let mut props = Vec::new();
                if let Some(r) = rows.first() {
                    for (i, c) in cols.iter().enumerate() {
                        if let Some(v) = read::text(r, i) {
                            props.push(Property::grouped("Table status", c.replace('_', " "), v));
                        }
                    }
                    let data = cols.iter().position(|c| c == "Data_length").and_then(|i| read::int(r, i)).unwrap_or(0);
                    let index = cols.iter().position(|c| c == "Index_length").and_then(|i| read::int(r, i)).unwrap_or(0);
                    props.push(Property::grouped("Size", "Total size", human_bytes(data + index)));
                    props.push(Property::grouped("Size", "Data size", human_bytes(data)));
                    props.push(Property::grouped("Size", "Index size", human_bytes(index)));
                }
                Ok(props)
            }
            ObjectKind::Database | ObjectKind::Schema => {
                self.props_from_row(
                    "SELECT SUM(TABLE_TYPE IN ('BASE TABLE','SYSTEM VERSIONED')), SUM(TABLE_TYPE = 'VIEW'), SUM(IFNULL(TABLE_ROWS, 0)), \
                     SUM(IFNULL(DATA_LENGTH, 0)), SUM(IFNULL(INDEX_LENGTH, 0)) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
                    vec![self.p(&s)],
                    &[
                        ("", "Tables"),
                        ("", "Views"),
                        ("", "Rows (estimate)"),
                        ("", "Data size (bytes)"),
                        ("", "Index size (bytes)"),
                    ],
                )
                .await
            }
            ObjectKind::Index => {
                let table = o.table.clone().unwrap_or_default();
                let rows = self
                    .rows(
                        "SELECT COLUMN_NAME, CARDINALITY FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                         AND INDEX_NAME = ? ORDER BY SEQ_IN_INDEX",
                        vec![self.p(&s), self.p(&table), self.p(&o.name)],
                    )
                    .await?;
                Ok(rows
                    .iter()
                    .map(|r| Property::grouped("Cardinality", read::text_or_empty(r, 0), read::text(r, 1).unwrap_or_else(|| "?".into())))
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    // ------------------------------------------------------------ dependencies

    pub(crate) async fn meta_object_dependencies(
        &mut self,
        o: &ObjectRef,
    ) -> DbResult<Vec<Dependency>> {
        let s = self.schema_or_current(o.schema.as_deref()).await?;
        let mut out = Vec::new();
        match o.kind {
            ObjectKind::Table => {
                let t = self
                    .load_scope(
                        &Scope {
                            schema: &s,
                            table: Some(&o.name),
                        },
                        &only(|o| {
                            o.constraints = true;
                            o.triggers = true;
                        }),
                    )
                    .await?;
                if let Some(td) = t.tables.first() {
                    for fk in &td.foreign_keys {
                        out.push(Dependency {
                            direction: DependencyDirection::DependsOn,
                            target: ObjectRef::table(
                                Some(fk.ref_schema.clone().unwrap_or_else(|| s.clone())),
                                fk.ref_table.clone(),
                            ),
                            kind: Some("foreign key".into()),
                        });
                    }
                    for fk in &td.referenced_by {
                        out.push(Dependency {
                            direction: DependencyDirection::UsedBy,
                            target: ObjectRef::table(fk.schema.clone(), fk.table.clone()),
                            kind: Some("foreign key".into()),
                        });
                    }
                    for tr in &td.triggers {
                        let mut r = obj(ObjectKind::Trigger, &s, tr.name.clone());
                        r.table = Some(o.name.clone());
                        out.push(Dependency {
                            direction: DependencyDirection::UsedBy,
                            target: r,
                            kind: Some("trigger".into()),
                        });
                    }
                }
                for (vs, vn) in self.views_using(&s, &o.name).await? {
                    out.push(Dependency {
                        direction: DependencyDirection::UsedBy,
                        target: obj(ObjectKind::View, &vs, vn),
                        kind: Some("view".into()),
                    });
                }
            }
            ObjectKind::View => {
                for (ts, tn, ttype) in self.tables_used_by_view(&s, &o.name).await? {
                    let kind = if ttype.contains("VIEW") {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    };
                    out.push(Dependency {
                        direction: DependencyDirection::DependsOn,
                        target: obj(kind, &ts, tn),
                        kind: Some("view".into()),
                    });
                }
                for (vs, vn) in self.views_using(&s, &o.name).await? {
                    out.push(Dependency {
                        direction: DependencyDirection::UsedBy,
                        target: obj(ObjectKind::View, &vs, vn),
                        kind: Some("view".into()),
                    });
                }
            }
            ObjectKind::Trigger => {
                let row = self
                    .row(
                        "SELECT EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? AND TRIGGER_NAME = ?",
                        vec![self.p(&s), self.p(&o.name)],
                    )
                    .await?;
                if let Some(t) = row.and_then(|r| read::non_empty(&r, 0)) {
                    out.push(Dependency {
                        direction: DependencyDirection::DependsOn,
                        target: ObjectRef::table(Some(s.clone()), t),
                        kind: Some("trigger".into()),
                    });
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// Views (schema, name) referencing `schema.name`.
    async fn views_using(&mut self, schema: &str, name: &str) -> DbResult<Vec<(String, String)>> {
        let rows = if self.profile.has_view_table_usage() {
            self.try_rows(
                "SELECT VIEW_SCHEMA, VIEW_NAME FROM information_schema.VIEW_TABLE_USAGE WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY VIEW_SCHEMA, VIEW_NAME",
                vec![self.p(schema), self.p(name)],
            )
            .await?
        } else {
            None
        };
        let rows = match rows {
            Some(r) => r,
            None => {
                let needle = format!("{}.{}", quote(schema), quote(name));
                self.rows(
                    "SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.VIEWS WHERE LOCATE(?, VIEW_DEFINITION) > 0 \
                     AND NOT (TABLE_SCHEMA = ? AND TABLE_NAME = ?) ORDER BY TABLE_SCHEMA, TABLE_NAME",
                    vec![self.p(&needle), self.p(schema), self.p(name)],
                )
                .await?
            }
        };
        Ok(rows
            .iter()
            .map(|r| (read::text_or_empty(r, 0), read::text_or_empty(r, 1)))
            .collect())
    }

    /// Tables / views (schema, name, type) a view reads from.
    async fn tables_used_by_view(
        &mut self,
        schema: &str,
        view: &str,
    ) -> DbResult<Vec<(String, String, String)>> {
        if self.profile.has_view_table_usage()
            && let Some(rows) = self
                .try_rows(
                    "SELECT u.TABLE_SCHEMA, u.TABLE_NAME, IFNULL(t.TABLE_TYPE, '') FROM information_schema.VIEW_TABLE_USAGE u \
                     LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = u.TABLE_SCHEMA AND t.TABLE_NAME = u.TABLE_NAME \
                     WHERE u.VIEW_SCHEMA = ? AND u.VIEW_NAME = ? ORDER BY u.TABLE_SCHEMA, u.TABLE_NAME",
                    vec![self.p(schema), self.p(view)],
                )
                .await?
            {
                return Ok(rows.iter().map(|r| (read::text_or_empty(r, 0), read::text_or_empty(r, 1), read::text_or_empty(r, 2))).collect());
            }
        let rows = self
            .rows(
                "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, t.TABLE_TYPE FROM information_schema.VIEWS v \
                 JOIN information_schema.TABLES t ON LOCATE(CONCAT('`', t.TABLE_SCHEMA, '`.`', t.TABLE_NAME, '`'), v.VIEW_DEFINITION) > 0 \
                 WHERE v.TABLE_SCHEMA = ? AND v.TABLE_NAME = ? AND NOT (t.TABLE_SCHEMA = v.TABLE_SCHEMA AND t.TABLE_NAME = v.TABLE_NAME) \
                 ORDER BY t.TABLE_SCHEMA, t.TABLE_NAME",
                vec![self.p(schema), self.p(view)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    read::text_or_empty(r, 0),
                    read::text_or_empty(r, 1),
                    read::text_or_empty(r, 2),
                )
            })
            .collect())
    }

    // ------------------------------------------------------------------ search

    pub(crate) async fn meta_search(
        &mut self,
        search: &MetadataSearch,
    ) -> DbResult<Vec<ObjectInfo>> {
        if search.pattern.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = like_pattern(search.pattern.trim());
        let limit = search.limit.clamp(1, 100_000);
        let cmp = |col: &str| {
            if search.case_sensitive {
                format!("CONVERT({col} USING utf8mb4) COLLATE utf8mb4_bin LIKE ?")
            } else {
                format!("LOWER(CONVERT({col} USING utf8mb4)) LIKE LOWER(?)")
            }
        };
        let mut out: Vec<ObjectInfo> = Vec::new();
        for kind in &search.kinds {
            if out.len() >= limit {
                break;
            }
            let (sql_base, name_col, comment_col, schema_col): (String, &str, Option<&str>, &str) = match kind {
                ObjectKind::Table => (
                    "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_COMMENT, '' FROM information_schema.TABLES WHERE TABLE_TYPE IN ('BASE TABLE','SYSTEM VERSIONED')".into(),
                    "TABLE_NAME",
                    Some("TABLE_COMMENT"),
                    "TABLE_SCHEMA",
                ),
                ObjectKind::View => (
                    "SELECT TABLE_SCHEMA, TABLE_NAME, '', '' FROM information_schema.TABLES WHERE TABLE_TYPE = 'VIEW'".into(),
                    "TABLE_NAME",
                    None,
                    "TABLE_SCHEMA",
                ),
                ObjectKind::Sequence => (
                    "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_COMMENT, '' FROM information_schema.TABLES WHERE TABLE_TYPE = 'SEQUENCE'".into(),
                    "TABLE_NAME",
                    Some("TABLE_COMMENT"),
                    "TABLE_SCHEMA",
                ),
                ObjectKind::Column => (
                    "SELECT TABLE_SCHEMA, COLUMN_NAME, COLUMN_COMMENT, TABLE_NAME, COLUMN_TYPE FROM information_schema.COLUMNS WHERE 1=1".into(),
                    "COLUMN_NAME",
                    Some("COLUMN_COMMENT"),
                    "TABLE_SCHEMA",
                ),
                ObjectKind::Function | ObjectKind::Procedure => (
                    format!(
                        "SELECT ROUTINE_SCHEMA, ROUTINE_NAME, ROUTINE_COMMENT, '' FROM information_schema.ROUTINES WHERE ROUTINE_TYPE = '{}'",
                        if *kind == ObjectKind::Function { "FUNCTION" } else { "PROCEDURE" }
                    ),
                    "ROUTINE_NAME",
                    Some("ROUTINE_COMMENT"),
                    "ROUTINE_SCHEMA",
                ),
                ObjectKind::Trigger => (
                    "SELECT TRIGGER_SCHEMA, TRIGGER_NAME, '', EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE 1=1".into(),
                    "TRIGGER_NAME",
                    None,
                    "TRIGGER_SCHEMA",
                ),
                ObjectKind::Event => (
                    "SELECT EVENT_SCHEMA, EVENT_NAME, EVENT_COMMENT, '' FROM information_schema.EVENTS WHERE 1=1".into(),
                    "EVENT_NAME",
                    Some("EVENT_COMMENT"),
                    "EVENT_SCHEMA",
                ),
                ObjectKind::Index => (
                    "SELECT DISTINCT TABLE_SCHEMA, INDEX_NAME, '', TABLE_NAME FROM information_schema.STATISTICS WHERE 1=1".into(),
                    "INDEX_NAME",
                    None,
                    "TABLE_SCHEMA",
                ),
                ObjectKind::ForeignKey | ObjectKind::UniqueKey | ObjectKind::CheckConstraint => (
                    format!(
                        "SELECT CONSTRAINT_SCHEMA, CONSTRAINT_NAME, '', TABLE_NAME FROM information_schema.TABLE_CONSTRAINTS WHERE CONSTRAINT_TYPE = '{}'",
                        match kind {
                            ObjectKind::ForeignKey => "FOREIGN KEY",
                            ObjectKind::UniqueKey => "UNIQUE",
                            _ => "CHECK",
                        }
                    ),
                    "CONSTRAINT_NAME",
                    None,
                    "CONSTRAINT_SCHEMA",
                ),
                ObjectKind::Schema | ObjectKind::Database => (
                    "SELECT SCHEMA_NAME, SCHEMA_NAME, '', '' FROM information_schema.SCHEMATA WHERE 1=1".into(),
                    "SCHEMA_NAME",
                    None,
                    "SCHEMA_NAME",
                ),
                _ => continue,
            };
            let mut sql = sql_base;
            let mut params = Vec::new();
            match (search.search_comments, comment_col) {
                (true, Some(cc)) => {
                    sql.push_str(&format!(" AND ({} OR {})", cmp(name_col), cmp(cc)));
                    params.push(self.p(&pattern));
                    params.push(self.p(&pattern));
                }
                _ => {
                    sql.push_str(&format!(" AND {}", cmp(name_col)));
                    params.push(self.p(&pattern));
                }
            }
            if search.schemas.is_empty() {
                sql.push_str(&format!(
                    " AND {schema_col} NOT IN ({})",
                    SYSTEM_SCHEMAS
                        .iter()
                        .map(|s| format!("'{s}'"))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            } else {
                sql.push_str(&format!(
                    " AND {schema_col} IN ({})",
                    vec!["?"; search.schemas.len()].join(",")
                ));
                for s in &search.schemas {
                    params.push(self.p(s));
                }
            }
            sql.push_str(&format!(" ORDER BY 1, 2 LIMIT {}", limit - out.len()));
            for r in self.rows(&sql, params).await? {
                let schema = read::text_or_empty(&r, 0);
                let name = read::text_or_empty(&r, 1);
                let comment = read::non_empty(&r, 2);
                let mut info = match kind {
                    ObjectKind::Column => {
                        let table = read::text_or_empty(&r, 3);
                        let parent = ObjectRef::table(Some(schema.clone()), table);
                        ObjectInfo::new(ObjectRef::child(ObjectKind::Column, &parent, name))
                            .with_detail(read::text_or_empty(&r, 4))
                    }
                    ObjectKind::Trigger
                    | ObjectKind::Index
                    | ObjectKind::ForeignKey
                    | ObjectKind::UniqueKey
                    | ObjectKind::CheckConstraint => {
                        let table = read::text_or_empty(&r, 3);
                        let parent = ObjectRef::table(Some(schema.clone()), table.clone());
                        ObjectInfo::new(ObjectRef::child(*kind, &parent, name))
                            .with_detail(format!("on {table}"))
                    }
                    ObjectKind::Schema | ObjectKind::Database => {
                        ObjectInfo::new(ObjectRef::new(ObjectKind::Schema, None, name))
                    }
                    k => ObjectInfo::new(obj(*k, &schema, name)),
                };
                info.is_system = is_system_schema(&schema);
                out.push(info.with_comment(comment));
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    // ---------------------------------------------------------------- snapshot

    pub(crate) async fn meta_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let s = self.schema_or_current(schema.schema.as_deref()).await?;
        let old = if opts.statistics {
            self.fresh_stats_begin().await
        } else {
            None
        };
        let res = self
            .load_scope(
                &Scope {
                    schema: &s,
                    table: None,
                },
                opts,
            )
            .await;
        self.fresh_stats_end(old).await;
        let loaded = res?;
        let routines = if opts.routines {
            self.load_routines(&s, None).await?
        } else {
            Vec::new()
        };
        let mut snap = SchemaSnapshot {
            kind: Some(self.profile.flavor.kind()),
            database: None,
            schema: Some(s),
            tables: loaded.tables,
            views: if opts.views { loaded.views } else { Vec::new() },
            sequences: if opts.sequences {
                loaded.sequences
            } else {
                Vec::new()
            },
            routines,
            types: Vec::new(),
        };
        if !opts.comments {
            for r in snap.routines.iter_mut() {
                r.comment = None;
            }
        }
        Ok(snap)
    }
}

/// Bound clause of a partition (`VALUES LESS THAN (10)`, `VALUES IN (1,2)`).
pub fn partition_bound(method: &str, description: Option<&str>) -> String {
    let m = method.to_ascii_uppercase();
    match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) if m.starts_with("RANGE") => {
            if d.eq_ignore_ascii_case("MAXVALUE") {
                "VALUES LESS THAN MAXVALUE".into()
            } else {
                format!("VALUES LESS THAN ({d})")
            }
        }
        Some(d) if m.starts_with("LIST") => format!("VALUES IN ({d})"),
        _ => String::new(),
    }
}

/// `key=value` pairs of `CREATE_OPTIONS` mapped to [`TableDef::options`] keys.
pub fn parse_create_options(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for part in s.split_whitespace() {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        let known = matches!(
            key.as_str(),
            "row_format"
                | "key_block_size"
                | "avg_row_length"
                | "max_rows"
                | "min_rows"
                | "pack_keys"
                | "checksum"
                | "delay_key_write"
                | "stats_persistent"
                | "stats_auto_recalc"
                | "stats_sample_pages"
                | "compression"
                | "encryption"
                | "page_compressed"
                | "page_compression_level"
                | "transactional"
                | "insert_method"
        );
        if known && !value.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

/// `1536` → `1.5 KiB`.
pub fn human_bytes(n: i64) -> String {
    let units = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} bytes")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;

    #[test]
    fn defaults_mysql() {
        let p = ServerProfile::new(Flavor::MySql, (8, 0, 46));
        let n = |t: &str, raw: Option<&str>, extra: &str| {
            normalize_default(&p, t, raw.map(str::to_string), extra)
        };
        assert_eq!(n("varchar(50)", Some("it's"), ""), Some("'it''s'".into()));
        assert_eq!(
            n("varbinary(10)", Some("0x6162"), ""),
            Some("0x6162".into())
        );
        assert_eq!(n("tinyint(1)", Some("1"), ""), Some("1".into()));
        assert_eq!(n("decimal(10,2)", Some("1.50"), ""), Some("1.50".into()));
        assert_eq!(
            n("timestamp", Some("CURRENT_TIMESTAMP"), "DEFAULT_GENERATED"),
            Some("CURRENT_TIMESTAMP".into())
        );
        assert_eq!(
            n(
                "datetime(3)",
                Some("CURRENT_TIMESTAMP(3)"),
                "DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)"
            ),
            Some("CURRENT_TIMESTAMP(3)".into())
        );
        assert_eq!(n("bit(5)", Some("b'101'"), ""), Some("b'101'".into()));
        assert_eq!(n("enum('a','b')", Some("a"), ""), Some("'a'".into()));
        assert_eq!(n("varchar(5)", None, ""), None);
        assert_eq!(
            n("double", Some("rand()"), "DEFAULT_GENERATED"),
            Some("(rand())".into())
        );
        assert_eq!(
            n("date", Some("2024-01-02"), ""),
            Some("'2024-01-02'".into())
        );
        // MySQL 5.7: CURRENT_TIMESTAMP without DEFAULT_GENERATED
        let p57 = ServerProfile::new(Flavor::MySql, (5, 7, 44));
        assert_eq!(
            normalize_default(
                &p57,
                "timestamp",
                Some("CURRENT_TIMESTAMP".into()),
                "on update CURRENT_TIMESTAMP"
            ),
            Some("CURRENT_TIMESTAMP".into())
        );
    }

    #[test]
    fn defaults_mariadb() {
        let p = ServerProfile::new(Flavor::MariaDb, (11, 8, 9));
        let n = |t: &str, raw: Option<&str>| normalize_default(&p, t, raw.map(str::to_string), "");
        assert_eq!(n("varchar(50)", Some("'it''s'")), Some("'it''s'".into()));
        assert_eq!(n("varchar(5)", Some("NULL")), None);
        assert_eq!(
            n("timestamp", Some("current_timestamp()")),
            Some("current_timestamp()".into())
        );
        assert_eq!(n("varbinary(10)", Some("x'6162'")), Some("x'6162'".into()));
        assert_eq!(n("double", Some("rand()")), Some("rand()".into()));
    }

    #[test]
    fn extra_parsing() {
        assert_eq!(
            on_update_from_extra("DEFAULT_GENERATED on update CURRENT_TIMESTAMP(3)").as_deref(),
            Some("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(
            on_update_from_extra("on update current_timestamp()").as_deref(),
            Some("current_timestamp()")
        );
        assert_eq!(on_update_from_extra("auto_increment"), None);
    }

    #[test]
    fn helpers() {
        assert_eq!(like_pattern("user"), "%user%");
        assert_eq!(like_pattern("us*r?"), "us%r_");
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
        assert_eq!(charset_of_collation("utf8mb4_0900_ai_ci"), "utf8mb4");
        assert_eq!(charset_of_collation("binary"), "binary");
        assert_eq!(
            partition_bound("RANGE", Some("2024")),
            "VALUES LESS THAN (2024)"
        );
        assert_eq!(
            partition_bound("RANGE COLUMNS", Some("MAXVALUE")),
            "VALUES LESS THAN MAXVALUE"
        );
        assert_eq!(partition_bound("LIST", Some("1,2")), "VALUES IN (1,2)");
        assert_eq!(partition_bound("HASH", None), "");
        let o = parse_create_options(
            "row_format=COMPRESSED KEY_BLOCK_SIZE=8 partitioned stats_persistent=0 COMPRESSION=\"zlib\"",
        );
        assert_eq!(o.get("row_format").map(String::as_str), Some("COMPRESSED"));
        assert_eq!(o.get("key_block_size").map(String::as_str), Some("8"));
        assert_eq!(o.get("compression").map(String::as_str), Some("zlib"));
        assert!(!o.contains_key("partitioned"));
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert!(is_system_schema("INFORMATION_SCHEMA") && !is_system_schema("app"));
    }
}
