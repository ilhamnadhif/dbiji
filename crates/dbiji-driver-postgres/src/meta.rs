//! Navigator metadata from `pg_catalog`: databases, schemas, objects of every
//! kind, table children, properties, dependencies, statistics and metadata
//! search.

use crate::catalog::{self, P, RowExt};
use crate::dialect::{qualified, quote_ident};
use crate::session::PgSession;
use crate::tabledef::require_schema;
use dbiji_core::meta::*;
use dbiji_core::{DbError, DbResult, SchemaRef};

/// System schemas (hidden unless "show system objects" is on).
pub(crate) fn is_system_schema(name: &str) -> bool {
    matches!(name, "pg_catalog" | "information_schema" | "crdb_internal" | "pg_extension") || name.starts_with("pg_toast") || name.starts_with("pg_temp_")
}

const DESC_CLASS: &str = "'pg_catalog.pg_class'::regclass";

fn some_nonempty(s: Option<String>) -> Option<String> {
    s.filter(|x| !x.is_empty())
}

/// Convert a `*`/`?` wildcard pattern (or plain text = contains) into LIKE syntax.
pub(crate) fn like_pattern(pattern: &str) -> String {
    let p = pattern.trim();
    let has_wild = p.contains('*') || p.contains('?');
    let mut out = String::with_capacity(p.len() + 2);
    for c in p.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '*' => out.push('%'),
            '?' => out.push('_'),
            c => out.push(c),
        }
    }
    if has_wild { out } else { format!("%{out}%") }
}

/// Map a `pg_identify_object` type to an object kind.
fn kind_from_identity_type(t: &str) -> Option<ObjectKind> {
    Some(match t {
        "table" => ObjectKind::Table,
        "view" => ObjectKind::View,
        "materialized view" => ObjectKind::MaterializedView,
        "foreign table" => ObjectKind::ForeignTable,
        "sequence" => ObjectKind::Sequence,
        "function" => ObjectKind::Function,
        "procedure" => ObjectKind::Procedure,
        "aggregate" => ObjectKind::Aggregate,
        "type" | "composite type" => ObjectKind::Type,
        "domain" => ObjectKind::Domain,
        "index" => ObjectKind::Index,
        "trigger" => ObjectKind::Trigger,
        "extension" => ObjectKind::Extension,
        "schema" => ObjectKind::Schema,
        _ => return None,
    })
}

/// Split `schema.name(args)` style identities of routines.
fn split_routine_identity(identity: &str, schema: Option<&str>) -> (String, Option<String>) {
    let without_schema = match schema {
        Some(s) => {
            let q = format!("{}.", quote_ident(s));
            identity.strip_prefix(&q).unwrap_or(identity)
        }
        None => identity,
    };
    match without_schema.find('(') {
        Some(i) => (without_schema[..i].trim_matches('"').to_string(), Some(without_schema[i..].to_string())),
        None => (without_schema.trim_matches('"').to_string(), None),
    }
}

impl PgSession {
    pub(crate) async fn list_databases_impl(&self) -> DbResult<Vec<DatabaseInfo>> {
        let size = if self.is_cockroach() { "NULL::int8" } else { "CASE WHEN d.datname = current_database() THEN pg_catalog.pg_database_size(d.oid) END" };
        let sql = format!(
            "SELECT d.datname::text, d.datname = current_database(), pg_catalog.pg_get_userbyid(d.datdba)::text, \
             pg_catalog.shobj_description(d.oid, 'pg_database'), pg_catalog.pg_encoding_to_char(d.encoding)::text, {size} \
             FROM pg_catalog.pg_database d WHERE NOT d.datistemplate AND d.datallowconn ORDER BY d.datname"
        );
        let rows = self.q(&sql, &[]).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name = r.s(0)?;
            out.push(DatabaseInfo {
                is_system: self.is_cockroach() && matches!(name.as_str(), "system" | "postgres"),
                name,
                is_current: r.b(1)?,
                owner: r.os(2)?,
                comment: some_nonempty(r.os(3)?),
                encoding: r.os(4)?,
                size_bytes: r.oi(5)?,
            });
        }
        Ok(out)
    }

    pub(crate) async fn list_schemas_impl(&self) -> DbResult<Vec<SchemaInfo>> {
        let temp_filter = if self.is_cockroach() {
            ""
        } else {
            " WHERE (n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%') OR n.oid = pg_catalog.pg_my_temp_schema()"
        };
        let sql = format!(
            "SELECT n.nspname::text, n.nspname = current_schema(), pg_catalog.pg_get_userbyid(n.nspowner)::text, d.description \
             FROM pg_catalog.pg_namespace n \
             LEFT JOIN pg_catalog.pg_description d ON d.objoid = n.oid AND d.classoid = 'pg_catalog.pg_namespace'::regclass AND d.objsubid = 0\
             {temp_filter} ORDER BY 1"
        );
        let rows = self.q(&sql, &[]).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name = r.s(0)?;
            out.push(SchemaInfo { is_system: is_system_schema(&name), is_current: r.b(1)?, owner: r.os(2)?, comment: some_nonempty(r.os(3)?), name });
        }
        // user schemas first, then system schemas
        out.sort_by(|a, b| a.is_system.cmp(&b.is_system).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    pub(crate) async fn list_objects_impl(&self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        if kind == ObjectKind::Extension {
            return self.list_extensions(schema.schema.as_deref()).await;
        }
        if kind == ObjectKind::Schema {
            return Ok(self
                .list_schemas_impl()
                .await?
                .into_iter()
                .map(|s| {
                    let mut o = ObjectInfo::new(ObjectRef::new(ObjectKind::Schema, None, s.name).with_database(schema.database.clone()));
                    o.comment = s.comment;
                    o.is_system = s.is_system;
                    o
                })
                .collect());
        }
        let name = require_schema(schema.schema.as_deref())?;
        let system = is_system_schema(name);
        let mk = |kind: ObjectKind, obj_name: String| ObjectRef::new(kind, Some(name.to_string()), obj_name).with_database(schema.database.clone());
        match kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::ForeignTable | ObjectKind::Sequence => {
                let partition_filter = match kind {
                    ObjectKind::Table => " AND c.relkind IN ('r','p') AND NOT coalesce(c.relispartition, false)",
                    ObjectKind::View => " AND c.relkind = 'v'",
                    ObjectKind::MaterializedView => " AND c.relkind = 'm'",
                    ObjectKind::ForeignTable => " AND c.relkind = 'f'",
                    _ => " AND c.relkind = 'S'",
                };
                let detail = match kind {
                    ObjectKind::ForeignTable => {
                        "(SELECT s.srvname::text FROM pg_catalog.pg_foreign_table ft JOIN pg_catalog.pg_foreign_server s ON s.oid = ft.ftserver WHERE ft.ftrelid = c.oid)"
                    }
                    ObjectKind::Sequence if !self.is_cockroach() => "(SELECT pg_catalog.format_type(s.seqtypid, NULL) FROM pg_catalog.pg_sequence s WHERE s.seqrelid = c.oid)",
                    _ => "NULL::text",
                };
                // With its indexes and TOAST; a partitioned table: its partitions'.
                let size = if self.is_cockroach() || !matches!(kind, ObjectKind::Table | ObjectKind::MaterializedView) {
                    "NULL::int8"
                } else {
                    "CASE WHEN c.relkind = 'p' THEN (SELECT sum(pg_catalog.pg_total_relation_size(i.inhrelid))::int8 FROM pg_catalog.pg_inherits i WHERE i.inhparent = c.oid) \
                     ELSE pg_catalog.pg_total_relation_size(c.oid) END"
                };
                let sql = format!(
                    "SELECT c.relname::text, d.description, c.reltuples::int8, c.relkind::text, c.relpersistence::text, {detail}, {size} \
                     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = 0 \
                     WHERE n.nspname = $1{partition_filter} ORDER BY c.relname"
                );
                let rows = self.q(&sql, &[P::Name(name)]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let mut o = ObjectInfo::new(mk(kind, r.s(0)?)).with_comment(r.os(1)?);
                    if matches!(kind, ObjectKind::Table | ObjectKind::MaterializedView | ObjectKind::ForeignTable) {
                        o.row_estimate = r.oi(2)?.filter(|n| *n >= 0);
                        o.size_bytes = r.oi(6)?;
                    }
                    let relkind = r.s(3)?;
                    let persistence = r.s(4)?;
                    o.detail = match (kind, relkind.as_str(), persistence.as_str()) {
                        (ObjectKind::Table, "p", _) => Some("partitioned".into()),
                        (ObjectKind::Table, _, "u") => Some("unlogged".into()),
                        (ObjectKind::Table, _, "t") => Some("temporary".into()),
                        _ => r.os(5)?,
                    };
                    o.is_system = system;
                    out.push(o);
                }
                Ok(out)
            }
            ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
                let prokind = match kind {
                    ObjectKind::Function => "p.prokind IN ('f','w')",
                    ObjectKind::Procedure => "p.prokind = 'p'",
                    _ => "p.prokind = 'a'",
                };
                let sql = format!(
                    "SELECT p.proname::text, pg_catalog.oidvectortypes(p.proargtypes), pg_catalog.pg_get_function_identity_arguments(p.oid), \
                     CASE WHEN p.prokind IN ('f','w','a') THEN pg_catalog.pg_get_function_result(p.oid) END, d.description, \
                     EXISTS (SELECT 1 FROM pg_catalog.pg_depend dep WHERE dep.classid = 'pg_catalog.pg_proc'::regclass \
                             AND dep.objid = p.oid AND dep.deptype = 'e') \
                     FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = p.oid AND d.classoid = 'pg_catalog.pg_proc'::regclass AND d.objsubid = 0 \
                     WHERE n.nspname = $1 AND {prokind} ORDER BY p.proname, 2"
                );
                let rows = self.q(&sql, &[P::Name(name)]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let obj = mk(kind, r.s(0)?).with_signature(format!("({})", r.s(1)?));
                    let args = r.s(2)?;
                    let detail = match r.os(3)? {
                        Some(ret) => format!("({args}) → {ret}"),
                        None => format!("({args})"),
                    };
                    let mut o = ObjectInfo::new(obj).with_detail(detail).with_comment(r.os(4)?);
                    o.is_system = system || r.b(5)?;
                    out.push(o);
                }
                Ok(out)
            }
            ObjectKind::Type => {
                let sql = "SELECT t.typname::text, t.typtype::text, d.description, \
                     EXISTS (SELECT 1 FROM pg_catalog.pg_depend dep WHERE dep.classid = 'pg_catalog.pg_type'::regclass \
                             AND dep.objid = t.oid AND dep.deptype = 'e') \
                     FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_type'::regclass AND d.objsubid = 0 \
                     WHERE n.nspname = $1 AND t.typtype IN ('b','c','e','r') \
                     AND (t.typrelid = 0 OR EXISTS (SELECT 1 FROM pg_catalog.pg_class cl WHERE cl.oid = t.typrelid AND cl.relkind = 'c')) \
                     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid) \
                     ORDER BY 1";
                let rows = self.q(sql, &[P::Name(name)]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let cat = match r.s(1)?.as_str() {
                        "e" => "enum",
                        "c" => "composite",
                        "r" => "range",
                        _ => "base",
                    };
                    let mut o = ObjectInfo::new(mk(kind, r.s(0)?)).with_detail(cat).with_comment(r.os(2)?);
                    o.is_system = system || r.b(3)?;
                    out.push(o);
                }
                Ok(out)
            }
            ObjectKind::Domain => {
                let rows = self
                    .q(
                        "SELECT t.typname::text, pg_catalog.format_type(t.typbasetype, t.typtypmod), t.typnotnull, d.description \
                         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_type'::regclass AND d.objsubid = 0 \
                         WHERE n.nspname = $1 AND t.typtype = 'd' ORDER BY 1",
                        &[P::Name(name)],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let mut detail = r.s(1)?;
                    if r.b(2)? {
                        detail.push_str(" NOT NULL");
                    }
                    let mut o = ObjectInfo::new(mk(kind, r.s(0)?)).with_detail(detail).with_comment(r.os(3)?);
                    o.is_system = system;
                    out.push(o);
                }
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn list_extensions(&self, schema: Option<&str>) -> DbResult<Vec<ObjectInfo>> {
        let filter = if schema.is_some() { " WHERE n.nspname = $1" } else { "" };
        let sql = format!(
            "SELECT e.extname::text, e.extversion, n.nspname::text, d.description FROM pg_catalog.pg_extension e \
             JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
             LEFT JOIN pg_catalog.pg_description d ON d.objoid = e.oid AND d.classoid = 'pg_catalog.pg_extension'::regclass{filter} \
             ORDER BY 1"
        );
        let rows = match schema {
            Some(s) => self.q(&sql, &[P::Name(s)]).await?,
            None => self.q(&sql, &[]).await?,
        };
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let name = r.s(0)?;
            let obj = ObjectRef::new(ObjectKind::Extension, schema.map(str::to_string), name.clone());
            let mut o = ObjectInfo::new(obj).with_detail(format!("{} ({})", r.s(1)?, r.s(2)?)).with_comment(r.os(3)?);
            o.is_system = name == "plpgsql";
            out.push(o);
        }
        Ok(out)
    }

    pub(crate) async fn list_children_impl(&self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        let schema = require_schema(parent.schema.as_deref())?;
        let table = parent.name.as_str();
        let base = "FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace";
        match kind {
            ObjectKind::Column => {
                let generated = self.attgenerated();
                let rows = self
                    .q(
                        &format!(
                            "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, d.description, \
                             a.attidentity::text, {generated}, \
                             EXISTS (SELECT 1 FROM pg_catalog.pg_constraint pk WHERE pk.conrelid = c.oid AND pk.contype = 'p' AND a.attnum = ANY(pk.conkey)), \
                             pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) \
                             {base} JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
                             LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                             LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = a.attnum \
                             WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
                        ),
                        &[P::Name(schema), P::Name(table)],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let mut detail = r.s(1)?;
                    if r.b(2)? {
                        detail.push_str(" NOT NULL");
                    }
                    let expr = r.os(7)?;
                    match r.s(4)?.as_str() {
                        "a" => detail.push_str(" GENERATED ALWAYS AS IDENTITY"),
                        "d" => detail.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                        _ => {}
                    }
                    match (r.s(5)?.as_str(), expr) {
                        ("", Some(d)) => detail.push_str(&format!(" DEFAULT {d}")),
                        ("", None) => {}
                        (_, Some(g)) => detail.push_str(&format!(" GENERATED ALWAYS AS ({g})")),
                        (_, None) => detail.push_str(" GENERATED"),
                    }
                    if r.b(6)? {
                        detail.push_str(" (PK)");
                    }
                    out.push(ObjectInfo::new(ObjectRef::child(kind, parent, r.s(0)?)).with_detail(detail).with_comment(r.os(3)?));
                }
                Ok(out)
            }
            ObjectKind::Index => {
                let rows = self
                    .q(
                        &format!(
                            "SELECT i.relname::text, pg_catalog.pg_get_indexdef(i.oid), d.description \
                             {base} JOIN pg_catalog.pg_index x ON x.indrelid = c.oid JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
                             LEFT JOIN pg_catalog.pg_description d ON d.objoid = i.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = 0 \
                             WHERE n.nspname = $1 AND c.relname = $2 ORDER BY i.relname"
                        ),
                        &[P::Name(schema), P::Name(table)],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let def = r.s(1)?;
                    let detail = def.split_once(" USING ").map(|(_, d)| d.to_string()).unwrap_or(def);
                    out.push(ObjectInfo::new(ObjectRef::child(kind, parent, r.s(0)?)).with_detail(detail).with_comment(r.os(2)?));
                }
                Ok(out)
            }
            ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::CheckConstraint | ObjectKind::ForeignKey => {
                let types = match kind {
                    ObjectKind::PrimaryKey => "('p')",
                    ObjectKind::UniqueKey => "('u','x')",
                    ObjectKind::CheckConstraint => "('c')",
                    _ => "('f')",
                };
                let rows = self
                    .q(
                        &format!(
                            "SELECT con.conname::text, pg_catalog.pg_get_constraintdef(con.oid, true), d.description \
                             {base} JOIN pg_catalog.pg_constraint con ON con.conrelid = c.oid \
                             LEFT JOIN pg_catalog.pg_description d ON d.objoid = con.oid AND d.classoid = 'pg_catalog.pg_constraint'::regclass \
                             WHERE n.nspname = $1 AND c.relname = $2 AND con.contype IN {types} ORDER BY con.conname"
                        ),
                        &[P::Name(schema), P::Name(table)],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    out.push(ObjectInfo::new(ObjectRef::child(kind, parent, r.s(0)?)).with_detail(r.s(1)?).with_comment(r.os(2)?));
                }
                Ok(out)
            }
            ObjectKind::Trigger => {
                let rows = match self
                    .q(
                        &format!(
                            "SELECT t.tgname::text, t.tgtype::int8, t.tgenabled::text, d.description, \
                             pg_catalog.quote_ident(pn.nspname) || '.' || pg_catalog.quote_ident(p.proname) || '()', \
                             pg_catalog.pg_get_triggerdef(t.oid, true) \
                             {base} JOIN pg_catalog.pg_trigger t ON t.tgrelid = c.oid JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
                             JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
                             LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_trigger'::regclass \
                             WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal ORDER BY t.tgname"
                        ),
                        &[P::Name(schema), P::Name(table)],
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(_) if self.is_cockroach() => return Ok(Vec::new()),
                    Err(e) => return Err(e),
                };
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let name = r.s(0)?;
                    // "CREATE [CONSTRAINT] TRIGGER name BEFORE ..." -> "BEFORE ..."
                    let def = r.s(5)?;
                    let summary = def.find(&format!(" {} ", quote_ident(&name))).map(|i| def[i + quote_ident(&name).len() + 2..].to_string());
                    let mut detail = match summary {
                        Some(s) => s,
                        None => {
                            let tgtype = r.i(1)?;
                            let timing = if tgtype & 64 != 0 {
                                "INSTEAD OF"
                            } else if tgtype & 2 != 0 {
                                "BEFORE"
                            } else {
                                "AFTER"
                            };
                            let mut ev = Vec::new();
                            for (bit, e) in [(4, "INSERT"), (16, "UPDATE"), (8, "DELETE"), (32, "TRUNCATE")] {
                                if tgtype & bit != 0 {
                                    ev.push(e);
                                }
                            }
                            format!("{timing} {} FOR EACH {} EXECUTE FUNCTION {}", ev.join(" OR "), if tgtype & 1 != 0 { "ROW" } else { "STATEMENT" }, r.s(4)?)
                        }
                    };
                    if r.s(2)? == "D" {
                        detail.push_str(" (disabled)");
                    }
                    out.push(ObjectInfo::new(ObjectRef::child(kind, parent, name)).with_detail(detail).with_comment(r.os(3)?));
                }
                Ok(out)
            }
            ObjectKind::Partition => {
                let rows = self
                    .q(
                        "SELECT c.relname::text, n.nspname::text, pg_catalog.pg_get_expr(c.relpartbound, c.oid), c.reltuples::int8, d.description \
                         FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class c ON c.oid = i.inhrelid \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         JOIN pg_catalog.pg_class p ON p.oid = i.inhparent JOIN pg_catalog.pg_namespace pn ON pn.oid = p.relnamespace \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = 'pg_catalog.pg_class'::regclass AND d.objsubid = 0 \
                         WHERE pn.nspname = $1 AND p.relname = $2 AND coalesce(c.relispartition, false) ORDER BY c.relname",
                        &[P::Name(schema), P::Name(table)],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let mut obj = ObjectRef::child(kind, parent, r.s(0)?);
                    obj.schema = Some(r.s(1)?);
                    let mut o = ObjectInfo::new(obj).with_comment(r.os(4)?);
                    o.detail = r.os(2)?;
                    o.row_estimate = r.oi(3)?.filter(|n| *n >= 0);
                    out.push(o);
                }
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// `(classid, objid)` of an object for pg_depend / properties.
    pub(crate) async fn object_identity(&self, obj: &ObjectRef) -> DbResult<Option<(u32, u32)>> {
        let class = |name: &'static str| format!("'pg_catalog.{name}'::regclass::oid::int8");
        let (sql, params): (String, Vec<String>) = match obj.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::ForeignTable | ObjectKind::Sequence | ObjectKind::Partition | ObjectKind::Index => (
                format!(
                    "SELECT {}, c.oid::int8 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2",
                    class("pg_class")
                ),
                vec![require_schema(obj.schema.as_deref())?.to_string(), obj.name.clone()],
            ),
            ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
                let schema = require_schema(obj.schema.as_deref())?;
                let all = self.load_routines(schema, Some(&obj.name), false, false).await?;
                let wanted = obj.signature.as_deref().map(crate::tabledef::normalize_signature);
                let found = all.into_iter().find(|(oid, sig, _)| match &wanted {
                    None => true,
                    Some(w) => *w == crate::tabledef::normalize_signature(sig) || *w == oid.to_string(),
                });
                let cls = self.q1("SELECT 'pg_catalog.pg_proc'::regclass::oid::int8", &[]).await?;
                return Ok(match (found, cls) {
                    (Some((oid, _, _)), Some(c)) => Some((c.i(0)? as u32, oid)),
                    _ => None,
                });
            }
            ObjectKind::Type | ObjectKind::Domain => (
                format!(
                    "SELECT {}, t.oid::int8 FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                     WHERE n.nspname = $1 AND t.typname = $2",
                    class("pg_type")
                ),
                vec![require_schema(obj.schema.as_deref())?.to_string(), obj.name.clone()],
            ),
            ObjectKind::Schema => (format!("SELECT {}, n.oid::int8 FROM pg_catalog.pg_namespace n WHERE n.nspname = $1", class("pg_namespace")), vec![obj.name.clone()]),
            ObjectKind::Extension => (format!("SELECT {}, e.oid::int8 FROM pg_catalog.pg_extension e WHERE e.extname = $1", class("pg_extension")), vec![obj.name.clone()]),
            ObjectKind::Trigger => (
                format!(
                    "SELECT {}, t.oid::int8 FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2 AND t.tgname = $3",
                    class("pg_trigger")
                ),
                vec![require_schema(obj.schema.as_deref())?.to_string(), obj.table.clone().unwrap_or_default(), obj.name.clone()],
            ),
            ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::CheckConstraint | ObjectKind::ForeignKey => (
                format!(
                    "SELECT {}, con.oid::int8 FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2 AND con.conname = $3",
                    class("pg_constraint")
                ),
                vec![require_schema(obj.schema.as_deref())?.to_string(), obj.table.clone().unwrap_or_default(), obj.name.clone()],
            ),
            ObjectKind::Database => (format!("SELECT {}, d.oid::int8 FROM pg_catalog.pg_database d WHERE d.datname = $1", class("pg_database")), vec![obj.name.clone()]),
            _ => return Ok(None),
        };
        let p: Vec<P<'_>> = params.iter().map(|s| P::Name(s.as_str())).collect();
        match self.q1(&sql, &p).await? {
            Some(r) => Ok(Some((r.i(0)? as u32, r.i(1)? as u32))),
            None => Ok(None),
        }
    }

    pub(crate) async fn object_dependencies_impl(&self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        let Some((classid, objid)) = self.object_identity(obj).await? else {
            return Ok(Vec::new());
        };
        let rows = self
            .q(
                "WITH deps AS ( \
                   SELECT 'D' AS dir, d.deptype::text AS deptype, d.refclassid AS cls, d.refobjid AS oid \
                     FROM pg_catalog.pg_depend d WHERE d.classid = $1 AND d.objid = $2 AND d.deptype IN ('n','a') \
                     AND d.refobjid <> $2 AND d.refclassid NOT IN ('pg_catalog.pg_namespace'::regclass, 'pg_catalog.pg_constraint'::regclass) \
                   UNION \
                   SELECT 'D', 'n', d.refclassid, d.refobjid FROM pg_catalog.pg_rewrite r \
                     JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_rewrite'::regclass AND d.objid = r.oid \
                     WHERE $1 = 'pg_catalog.pg_class'::regclass AND r.ev_class = $2 AND d.refobjid <> $2 \
                   UNION \
                   SELECT 'U', d.deptype::text, d.classid, d.objid FROM pg_catalog.pg_depend d \
                     WHERE d.refclassid = $1 AND d.refobjid = $2 AND d.deptype IN ('n','a') \
                     AND d.classid NOT IN ('pg_catalog.pg_rewrite'::regclass, 'pg_catalog.pg_constraint'::regclass, 'pg_catalog.pg_attrdef'::regclass) \
                   UNION \
                   SELECT 'U', 'view', 'pg_catalog.pg_class'::regclass::oid, r.ev_class FROM pg_catalog.pg_depend d \
                     JOIN pg_catalog.pg_rewrite r ON r.oid = d.objid \
                     WHERE d.classid = 'pg_catalog.pg_rewrite'::regclass AND d.refclassid = $1 AND d.refobjid = $2 AND r.ev_class <> $2 \
                 ) \
                 SELECT dep.dir, dep.deptype, i.type, i.schema, i.name, i.identity \
                 FROM deps dep, LATERAL pg_catalog.pg_identify_object(dep.cls, dep.oid, 0) i \
                 WHERE COALESCE(i.schema, '') NOT IN ('pg_catalog', 'information_schema') \
                 ORDER BY 1, 3, 6",
                &[P::Oid(classid), P::Oid(objid)],
            )
            .await?;
        let mut out: Vec<Dependency> = Vec::new();
        for r in rows {
            let dir = if r.s(0)? == "D" { DependencyDirection::DependsOn } else { DependencyDirection::UsedBy };
            let deptype = r.s(1)?;
            let Some(kind) = kind_from_identity_type(&r.s(2)?) else { continue };
            let schema = r.os(3)?;
            let identity = r.s(5)?;
            let mut table = None;
            let (name, signature) = match (kind, kind.is_routine(), r.os(4)?) {
                (_, true, _) => split_routine_identity(&identity, schema.as_deref()),
                // "trg on schema.table"
                (ObjectKind::Trigger, _, _) => match identity.split_once(" on ") {
                    Some((trg, rel)) => {
                        table = Some(rel.rsplit('.').next().unwrap_or(rel).trim_matches('"').to_string());
                        (trg.trim_matches('"').to_string(), None)
                    }
                    None => (identity.clone(), None),
                },
                (_, false, Some(n)) => (n, None),
                (_, false, None) => (identity.rsplit('.').next().unwrap_or(&identity).trim_matches('"').to_string(), None),
            };
            let mut target = ObjectRef::new(kind, if kind == ObjectKind::Schema { None } else { schema }, name);
            target.signature = signature;
            target.table = table;
            let kind_text = match deptype.as_str() {
                "n" => "normal",
                "a" => "auto",
                "view" => "view",
                other => other,
            };
            let dep = Dependency { direction: dir, target, kind: Some(kind_text.to_string()) };
            if !out.contains(&dep) {
                out.push(dep);
            }
        }
        // foreign key relations of tables
        if matches!(obj.kind, ObjectKind::Table | ObjectKind::Partition | ObjectKind::ForeignTable) {
            let rows = self
                .q(
                    "SELECT 'D', fn.nspname::text, fc.relname::text, con.conname::text FROM pg_catalog.pg_constraint con \
                     JOIN pg_catalog.pg_class fc ON fc.oid = con.confrelid JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace \
                     WHERE con.conrelid = $1 AND con.contype = 'f' \
                     UNION ALL \
                     SELECT 'U', n.nspname::text, c.relname::text, con.conname::text FROM pg_catalog.pg_constraint con \
                     JOIN pg_catalog.pg_class c ON c.oid = con.conrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE con.confrelid = $1 AND con.contype = 'f' ORDER BY 1, 2, 3",
                    &[P::Oid(objid)],
                )
                .await?;
            for r in rows {
                let dir = if r.s(0)? == "D" { DependencyDirection::DependsOn } else { DependencyDirection::UsedBy };
                out.push(Dependency { direction: dir, target: ObjectRef::table(Some(r.s(1)?), r.s(2)?), kind: Some(format!("foreign key {}", r.s(3)?)) });
            }
        }
        Ok(out)
    }

    pub(crate) async fn object_properties_impl(&self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let g = |group: &str, name: &str, v: String| Property::grouped(group, name, v);
        let mut props = Vec::new();
        match obj.kind {
            ObjectKind::Table | ObjectKind::Partition | ObjectKind::ForeignTable | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::Sequence | ObjectKind::Index => {
                let schema = require_schema(obj.schema.as_deref())?;
                let sizes = if self.is_cockroach() {
                    "NULL::text, NULL::text, NULL::text, NULL::text"
                } else {
                    "pg_catalog.pg_size_pretty(pg_catalog.pg_total_relation_size(c.oid)), pg_catalog.pg_size_pretty(pg_catalog.pg_table_size(c.oid)), \
                     pg_catalog.pg_size_pretty(pg_catalog.pg_indexes_size(c.oid)), \
                     CASE WHEN c.reltoastrelid <> 0 THEN pg_catalog.pg_size_pretty(pg_catalog.pg_total_relation_size(c.reltoastrelid)) END"
                };
                let partkey = if self.is_cockroach() { "NULL::text" } else { "CASE WHEN c.relkind = 'p' THEN pg_catalog.pg_get_partkeydef(c.oid) END" };
                let am = if self.is_cockroach() { "NULL::text" } else { "(SELECT am.amname::text FROM pg_catalog.pg_am am WHERE am.oid = c.relam)" };
                let sql = format!(
                    "SELECT c.oid::int8, pg_catalog.pg_get_userbyid(c.relowner)::text, c.relkind::text, c.relpersistence::text, \
                     ts.spcname::text, {am}, array_to_string(c.reloptions, ', '), c.reltuples::int8, c.relpages::int8, \
                     c.relhasindex, c.relhastriggers, c.relrowsecurity, c.relispopulated, c.relispartition, {sizes}, \
                     c.relacl::text, d.description, {partkey}, \
                     CASE WHEN c.relispartition THEN pg_catalog.pg_get_expr(c.relpartbound, c.oid) END, c.relreplident::text \
                     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = c.reltablespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = 0 \
                     WHERE n.nspname = $1 AND c.relname = $2"
                );
                let Some(r) = self.q1(&sql, &[P::Name(schema), P::Name(&obj.name)]).await? else {
                    return Err(DbError::query(format!("{} {schema}.{} does not exist", obj.kind.label(), obj.name)));
                };
                let relkind = r.s(2)?;
                let kind_text = match relkind.as_str() {
                    "r" => "Table",
                    "p" => "Partitioned table",
                    "f" => "Foreign table",
                    "v" => "View",
                    "m" => "Materialized view",
                    "S" => "Sequence",
                    "i" => "Index",
                    "I" => "Partitioned index",
                    _ => "Relation",
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "Schema", schema.to_string()));
                props.push(g("General", "OID", r.i(0)?.to_string()));
                props.push(g("General", "Owner", r.s(1)?));
                props.push(g("General", "Type", kind_text.to_string()));
                props.push(g(
                    "General",
                    "Persistence",
                    match r.s(3)?.as_str() {
                        "u" => "unlogged",
                        "t" => "temporary",
                        _ => "permanent",
                    }
                    .to_string(),
                ));
                if let Some(pk) = r.os(20)? {
                    props.push(g("General", "Partition key", pk));
                }
                if r.b(13)? {
                    props.push(g("General", "Partition bound", r.os(21)?.unwrap_or_default()));
                }
                if let Some(c) = some_nonempty(r.os(19)?) {
                    props.push(g("General", "Comment", c));
                }
                props.push(g("Storage", "Tablespace", r.os(4)?.unwrap_or_else(|| "pg_default".into())));
                if let Some(am) = r.os(5)? {
                    props.push(g("Storage", "Access method", am));
                }
                if let Some(o) = some_nonempty(r.os(6)?) {
                    props.push(g("Storage", "Options", o));
                }
                if relkind != "v" {
                    if let Some(n) = r.oi(7)?.filter(|n| *n >= 0) {
                        props.push(g("Storage", "Estimated rows", n.to_string()));
                    }
                    props.push(g("Storage", "Pages", r.i(8)?.to_string()));
                    if let Some(s) = r.os(14)? {
                        props.push(g("Storage", "Total size", s));
                    }
                    if let Some(s) = r.os(15)? {
                        props.push(g("Storage", "Table size", s));
                    }
                    if matches!(relkind.as_str(), "r" | "p" | "m") {
                        if let Some(s) = r.os(16)? {
                            props.push(g("Storage", "Indexes size", s));
                        }
                        if let Some(s) = r.os(17)? {
                            props.push(g("Storage", "TOAST size", s));
                        }
                    }
                }
                if matches!(relkind.as_str(), "r" | "p") {
                    props.push(g("General", "Has indexes", r.b(9)?.to_string()));
                    props.push(g("General", "Has triggers", r.b(10)?.to_string()));
                    props.push(g("Security", "Row level security", r.b(11)?.to_string()));
                    props.push(g(
                        "General",
                        "Replica identity",
                        match r.s(22)?.as_str() {
                            "n" => "nothing",
                            "f" => "full",
                            "i" => "index",
                            _ => "default",
                        }
                        .to_string(),
                    ));
                }
                if relkind == "m" {
                    props.push(g("General", "Populated", r.b(12)?.to_string()));
                }
                if let Some(acl) = r.os(18)? {
                    props.push(g("Security", "Privileges", acl));
                }
                if relkind == "S" {
                    let seq = self.sequence_definition_impl(obj).await?;
                    if let Some(t) = seq.data_type {
                        props.push(g("Sequence", "Data type", t));
                    }
                    for (k, v) in [
                        ("Start", seq.start),
                        ("Increment", seq.increment),
                        ("Minimum", seq.min_value),
                        ("Maximum", seq.max_value),
                        ("Cache", seq.cache),
                        ("Last value", seq.last_value),
                    ] {
                        if let Some(v) = v {
                            props.push(g("Sequence", k, v.to_string()));
                        }
                    }
                    props.push(g("Sequence", "Cycle", seq.cycle.to_string()));
                    if let Some(o) = seq.owned_by {
                        props.push(g("Sequence", "Owned by", o));
                    }
                }
                if relkind == "f"
                    && let Some(f) = self
                        .q1(
                            "SELECT s.srvname::text, w.fdwname::text, array_to_string(ft.ftoptions, ', ') \
                             FROM pg_catalog.pg_foreign_table ft JOIN pg_catalog.pg_foreign_server s ON s.oid = ft.ftserver \
                             JOIN pg_catalog.pg_foreign_data_wrapper w ON w.oid = s.srvfdw WHERE ft.ftrelid = $1",
                            &[P::Oid(r.i(0)? as u32)],
                        )
                        .await?
                {
                    props.push(g("Foreign table", "Server", f.s(0)?));
                    props.push(g("Foreign table", "Foreign data wrapper", f.s(1)?));
                    if let Some(o) = some_nonempty(f.os(2)?) {
                        props.push(g("Foreign table", "Options", o));
                    }
                }
                if (relkind == "i" || relkind == "I")
                    && let Some(r) = self
                        .q1(
                            "SELECT pg_catalog.pg_get_indexdef(x.indexrelid), x.indisunique, x.indisprimary, x.indisexclusion, \
                             x.indisvalid, x.indisclustered, am.amname::text, t.relname::text \
                             FROM pg_catalog.pg_index x JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid \
                             JOIN pg_catalog.pg_class t ON t.oid = x.indrelid JOIN pg_catalog.pg_am am ON am.oid = i.relam \
                             WHERE x.indexrelid = $1",
                            &[P::Oid(r.i(0)? as u32)],
                        )
                        .await?
                {
                    props.push(g("Index", "Definition", r.s(0)?));
                    props.push(g("Index", "Table", r.s(7)?));
                    props.push(g("Index", "Method", r.s(6)?));
                    props.push(g("Index", "Unique", r.b(1)?.to_string()));
                    props.push(g("Index", "Primary", r.b(2)?.to_string()));
                    props.push(g("Index", "Exclusion", r.b(3)?.to_string()));
                    props.push(g("Index", "Valid", r.b(4)?.to_string()));
                    props.push(g("Index", "Clustered", r.b(5)?.to_string()));
                }
            }
            ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
                let Some((_, oid)) = self.object_identity(obj).await? else {
                    return Err(DbError::query(format!("Routine {} does not exist", obj.display_path())));
                };
                let Some(r) = self
                    .q1(
                        "SELECT p.oid::int8, pg_catalog.pg_get_userbyid(p.proowner)::text, l.lanname::text, \
                         pg_catalog.pg_get_function_arguments(p.oid), pg_catalog.pg_get_function_result(p.oid), \
                         p.provolatile::text, p.proisstrict, p.prosecdef, p.proleakproof, p.proparallel::text, \
                         p.procost::float8, p.prorows::float8, array_to_string(p.proconfig, ', '), p.proacl::text, d.description, p.proretset \
                         FROM pg_catalog.pg_proc p LEFT JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = p.oid AND d.classoid = 'pg_catalog.pg_proc'::regclass AND d.objsubid = 0 \
                         WHERE p.oid = $1",
                        &[P::Oid(oid)],
                    )
                    .await?
                else {
                    return Ok(props);
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "OID", r.i(0)?.to_string()));
                props.push(g("General", "Owner", r.s(1)?));
                props.push(g("General", "Language", r.s(2)?));
                props.push(g("General", "Arguments", r.s(3)?));
                if let Some(res) = r.os(4)? {
                    props.push(g("General", "Returns", res));
                }
                props.push(g("General", "Returns set", r.b(15)?.to_string()));
                if let Some(c) = some_nonempty(r.os(14)?) {
                    props.push(g("General", "Comment", c));
                }
                props.push(g(
                    "Options",
                    "Volatility",
                    match r.s(5)?.as_str() {
                        "i" => "IMMUTABLE",
                        "s" => "STABLE",
                        _ => "VOLATILE",
                    }
                    .to_string(),
                ));
                props.push(g("Options", "Strict", r.b(6)?.to_string()));
                props.push(g("Options", "Security definer", r.b(7)?.to_string()));
                props.push(g("Options", "Leakproof", r.b(8)?.to_string()));
                props.push(g(
                    "Options",
                    "Parallel",
                    match r.s(9)?.as_str() {
                        "s" => "SAFE",
                        "r" => "RESTRICTED",
                        _ => "UNSAFE",
                    }
                    .to_string(),
                ));
                props.push(g("Options", "Cost", format!("{}", r.try_get::<_, f64>(10).unwrap_or(0.0))));
                props.push(g("Options", "Rows", format!("{}", r.try_get::<_, f64>(11).unwrap_or(0.0))));
                if let Some(c) = some_nonempty(r.os(12)?) {
                    props.push(g("Options", "Configuration", c));
                }
                if let Some(acl) = r.os(13)? {
                    props.push(g("Security", "Privileges", acl));
                }
            }
            ObjectKind::Type | ObjectKind::Domain => {
                let schema = require_schema(obj.schema.as_deref())?;
                let Some(r) = self
                    .q1(
                        "SELECT t.oid::int8, pg_catalog.pg_get_userbyid(t.typowner)::text, t.typtype::text, \
                         pg_catalog.format_type(t.typbasetype, t.typtypmod), t.typdefault, t.typnotnull, t.typacl::text, d.description, \
                         ARRAY(SELECT e.enumlabel::text FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid ORDER BY e.enumsortorder) \
                         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_type'::regclass AND d.objsubid = 0 \
                         WHERE n.nspname = $1 AND t.typname = $2",
                        &[P::Name(schema), P::Name(&obj.name)],
                    )
                    .await?
                else {
                    return Err(DbError::query(format!("Type {schema}.{} does not exist", obj.name)));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "OID", r.i(0)?.to_string()));
                props.push(g("General", "Owner", r.s(1)?));
                let cat = match r.s(2)?.as_str() {
                    "e" => "enum",
                    "c" => "composite",
                    "d" => "domain",
                    "r" => "range",
                    "m" => "multirange",
                    "p" => "pseudo",
                    _ => "base",
                };
                props.push(g("General", "Category", cat.to_string()));
                if cat == "domain" {
                    props.push(g("General", "Base type", r.s(3)?));
                    if let Some(d) = r.os(4)? {
                        props.push(g("General", "Default", d));
                    }
                    props.push(g("General", "Not null", r.b(5)?.to_string()));
                }
                let labels = r.ss(8)?;
                if !labels.is_empty() {
                    props.push(g("General", "Labels", labels.join(", ")));
                }
                if let Some(c) = some_nonempty(r.os(7)?) {
                    props.push(g("General", "Comment", c));
                }
                if let Some(acl) = r.os(6)? {
                    props.push(g("Security", "Privileges", acl));
                }
            }
            ObjectKind::Schema => {
                let Some(r) = self
                    .q1(
                        "SELECT n.oid::int8, pg_catalog.pg_get_userbyid(n.nspowner)::text, n.nspacl::text, d.description \
                         FROM pg_catalog.pg_namespace n LEFT JOIN pg_catalog.pg_description d ON d.objoid = n.oid \
                         AND d.classoid = 'pg_catalog.pg_namespace'::regclass WHERE n.nspname = $1",
                        &[P::Name(&obj.name)],
                    )
                    .await?
                else {
                    return Err(DbError::query(format!("Schema {} does not exist", obj.name)));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "OID", r.i(0)?.to_string()));
                props.push(g("General", "Owner", r.s(1)?));
                if let Some(c) = some_nonempty(r.os(3)?) {
                    props.push(g("General", "Comment", c));
                }
                if let Some(acl) = r.os(2)? {
                    props.push(g("Security", "Privileges", acl));
                }
            }
            ObjectKind::Database => {
                let size = if self.is_cockroach() {
                    "NULL::text"
                } else {
                    "CASE WHEN pg_catalog.has_database_privilege(d.oid, 'CONNECT') THEN pg_catalog.pg_size_pretty(pg_catalog.pg_database_size(d.oid)) END"
                };
                let sql = format!(
                    "SELECT d.oid::int8, pg_catalog.pg_get_userbyid(d.datdba)::text, pg_catalog.pg_encoding_to_char(d.encoding)::text, \
                     d.datcollate::text, d.datctype::text, ts.spcname::text, d.datconnlimit::int8, d.datallowconn, d.datacl::text, \
                     pg_catalog.shobj_description(d.oid, 'pg_database'), {size} \
                     FROM pg_catalog.pg_database d LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = d.dattablespace WHERE d.datname = $1"
                );
                let Some(r) = self.q1(&sql, &[P::Name(&obj.name)]).await? else {
                    return Err(DbError::query(format!("Database {} does not exist", obj.name)));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "OID", r.i(0)?.to_string()));
                props.push(g("General", "Owner", r.s(1)?));
                props.push(g("General", "Encoding", r.s(2)?));
                props.push(g("General", "Collation", r.s(3)?));
                props.push(g("General", "Character type", r.s(4)?));
                props.push(g("Storage", "Tablespace", r.s(5)?));
                if let Some(sz) = r.os(10)? {
                    props.push(g("Storage", "Size", sz));
                }
                props.push(g("General", "Connection limit", r.i(6)?.to_string()));
                props.push(g("General", "Allow connections", r.b(7)?.to_string()));
                if let Some(c) = some_nonempty(r.os(9)?) {
                    props.push(g("General", "Comment", c));
                }
                if let Some(acl) = r.os(8)? {
                    props.push(g("Security", "Privileges", acl));
                }
            }
            ObjectKind::Extension => {
                let Some(r) = self
                    .q1(
                        "SELECT e.extversion, n.nspname::text, e.extrelocatable, pg_catalog.pg_get_userbyid(e.extowner)::text, d.description \
                         FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = e.oid AND d.classoid = 'pg_catalog.pg_extension'::regclass \
                         WHERE e.extname = $1",
                        &[P::Name(&obj.name)],
                    )
                    .await?
                else {
                    return Err(DbError::query(format!("Extension {} does not exist", obj.name)));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "Version", r.s(0)?));
                props.push(g("General", "Schema", r.s(1)?));
                props.push(g("General", "Relocatable", r.b(2)?.to_string()));
                props.push(g("General", "Owner", r.s(3)?));
                if let Some(c) = some_nonempty(r.os(4)?) {
                    props.push(g("General", "Comment", c));
                }
            }
            ObjectKind::Column => {
                let schema = require_schema(obj.schema.as_deref())?;
                let table = obj.table.as_deref().unwrap_or_default();
                let compression = if self.pg_major() >= 14 { "a.attcompression::text" } else { "NULL::text" };
                let generated = self.attgenerated();
                let sql = format!(
                    "SELECT a.attnum::int8, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                     pg_catalog.pg_get_expr(ad.adbin, ad.adrelid), a.attidentity::text, {generated}, \
                     co.collname::text, a.attstorage::text, {compression}, a.attacl::text, d.description \
                     FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                     LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation AND a.attcollation <> 0 \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = a.attnum \
                     WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3"
                );
                let Some(r) = self.q1(&sql, &[P::Name(schema), P::Name(table), P::Name(&obj.name)]).await? else {
                    return Err(DbError::query(format!("Column {} does not exist", obj.display_path())));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "Position", r.i(0)?.to_string()));
                props.push(g("General", "Data type", r.s(1)?));
                props.push(g("General", "Not null", r.b(2)?.to_string()));
                let identity = r.s(4)?;
                let generated = r.s(5)?;
                if let Some(d) = r.os(3)? {
                    props.push(g("General", if generated.is_empty() { "Default" } else { "Generation expression" }, d));
                }
                if !identity.is_empty() {
                    props.push(g("General", "Identity", if identity == "a" { "ALWAYS".into() } else { "BY DEFAULT".into() }));
                }
                if let Some(c) = r.os(6)? {
                    props.push(g("General", "Collation", c));
                }
                props.push(g(
                    "Storage",
                    "Storage",
                    match r.s(7)?.as_str() {
                        "p" => "plain",
                        "e" => "external",
                        "m" => "main",
                        _ => "extended",
                    }
                    .to_string(),
                ));
                if let Some(c) = some_nonempty(r.os(8)?) {
                    props.push(g(
                        "Storage",
                        "Compression",
                        if c == "p" {
                            "pglz".into()
                        } else if c == "l" {
                            "lz4".into()
                        } else {
                            c
                        },
                    ));
                }
                if let Some(c) = some_nonempty(r.os(10)?) {
                    props.push(g("General", "Comment", c));
                }
                if let Some(acl) = r.os(9)? {
                    props.push(g("Security", "Privileges", acl));
                }
            }
            ObjectKind::PrimaryKey | ObjectKind::UniqueKey | ObjectKind::CheckConstraint | ObjectKind::ForeignKey => {
                let schema = require_schema(obj.schema.as_deref())?;
                let table = obj.table.as_deref().unwrap_or_default();
                let Some(r) = self
                    .q1(
                        "SELECT con.contype::text, pg_catalog.pg_get_constraintdef(con.oid), con.condeferrable, con.condeferred, \
                         con.convalidated, d.description FROM pg_catalog.pg_constraint con \
                         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = con.oid AND d.classoid = 'pg_catalog.pg_constraint'::regclass \
                         WHERE n.nspname = $1 AND c.relname = $2 AND con.conname = $3",
                        &[P::Name(schema), P::Name(table), P::Name(&obj.name)],
                    )
                    .await?
                else {
                    return Err(DbError::query(format!("Constraint {} does not exist", obj.display_path())));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g(
                    "General",
                    "Type",
                    match r.s(0)?.as_str() {
                        "p" => "PRIMARY KEY",
                        "u" => "UNIQUE",
                        "c" => "CHECK",
                        "f" => "FOREIGN KEY",
                        "x" => "EXCLUDE",
                        other => other,
                    }
                    .to_string(),
                ));
                props.push(g("General", "Definition", r.s(1)?));
                props.push(g("General", "Deferrable", r.b(2)?.to_string()));
                props.push(g("General", "Initially deferred", r.b(3)?.to_string()));
                props.push(g("General", "Validated", r.b(4)?.to_string()));
                if let Some(c) = some_nonempty(r.os(5)?) {
                    props.push(g("General", "Comment", c));
                }
            }
            ObjectKind::Trigger => {
                let schema = require_schema(obj.schema.as_deref())?;
                let table = obj.table.as_deref().unwrap_or_default();
                let Some(r) = self
                    .q1(
                        "SELECT pg_catalog.pg_get_triggerdef(t.oid), t.tgenabled::text, p.proname::text, d.description \
                         FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
                         LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_trigger'::regclass \
                         WHERE n.nspname = $1 AND c.relname = $2 AND t.tgname = $3",
                        &[P::Name(schema), P::Name(table), P::Name(&obj.name)],
                    )
                    .await?
                else {
                    return Err(DbError::query(format!("Trigger {} does not exist", obj.display_path())));
                };
                props.push(g("General", "Name", obj.name.clone()));
                props.push(g("General", "Definition", r.s(0)?));
                props.push(g(
                    "General",
                    "Enabled",
                    match r.s(1)?.as_str() {
                        "D" => "disabled",
                        "R" => "replica",
                        "A" => "always",
                        _ => "origin",
                    }
                    .to_string(),
                ));
                props.push(g("General", "Function", r.s(2)?));
                if let Some(c) = some_nonempty(r.os(3)?) {
                    props.push(g("General", "Comment", c));
                }
            }
            _ => {}
        }
        Ok(props)
    }

    pub(crate) async fn object_statistics_impl(&self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        if self.is_cockroach() {
            return Ok(Vec::new());
        }
        let mut props = Vec::new();
        let client = self.client()?;
        let mut add = |group: &str, data: dbiji_core::admin::TabularData, skip: &[&str]| {
            if let Some(row) = data.rows.first() {
                for (c, v) in data.columns.iter().zip(row) {
                    if skip.contains(&c.as_str()) {
                        continue;
                    }
                    let text = if v.is_null() { String::new() } else { v.to_text().into_owned() };
                    props.push(Property::grouped(group, c.clone(), text));
                }
            }
        };
        let skip = ["relid", "indexrelid", "schemaname", "relname", "indexrelname", "funcid", "funcname", "datid", "datname"];
        match obj.kind {
            ObjectKind::Table | ObjectKind::Partition | ObjectKind::MaterializedView | ObjectKind::ForeignTable => {
                let Some((_, oid)) = self.object_identity(obj).await? else { return Ok(props) };
                add("Activity", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_stat_all_tables WHERE relid = $1", &[P::Oid(oid)]).await?, &skip);
                add("I/O", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_statio_all_tables WHERE relid = $1", &[P::Oid(oid)]).await?, &skip);
                add(
                    "Size",
                    catalog::query_tabular(
                        client,
                        "SELECT c.reltuples::int8 AS estimated_rows, c.relpages::int8 AS pages, \
                         pg_catalog.pg_total_relation_size(c.oid) AS total_bytes, pg_catalog.pg_table_size(c.oid) AS table_bytes, \
                         pg_catalog.pg_indexes_size(c.oid) AS indexes_bytes, \
                         pg_catalog.pg_size_pretty(pg_catalog.pg_total_relation_size(c.oid)) AS total_size \
                         FROM pg_catalog.pg_class c WHERE c.oid = $1",
                        &[P::Oid(oid)],
                    )
                    .await?,
                    &skip,
                );
            }
            ObjectKind::Index => {
                let Some((_, oid)) = self.object_identity(obj).await? else { return Ok(props) };
                add("Activity", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_stat_all_indexes WHERE indexrelid = $1", &[P::Oid(oid)]).await?, &skip);
                add("I/O", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_statio_all_indexes WHERE indexrelid = $1", &[P::Oid(oid)]).await?, &skip);
                add(
                    "Size",
                    catalog::query_tabular(
                        client,
                        "SELECT pg_catalog.pg_relation_size($1::oid) AS size_bytes, pg_catalog.pg_size_pretty(pg_catalog.pg_relation_size($1::oid)) AS size",
                        &[P::Oid(oid)],
                    )
                    .await?,
                    &skip,
                );
            }
            ObjectKind::Sequence => {
                let Some((_, oid)) = self.object_identity(obj).await? else { return Ok(props) };
                add("I/O", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_statio_all_sequences WHERE relid = $1", &[P::Oid(oid)]).await?, &skip);
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let Some((_, oid)) = self.object_identity(obj).await? else { return Ok(props) };
                add("Activity", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_stat_user_functions WHERE funcid = $1", &[P::Oid(oid)]).await?, &skip);
            }
            ObjectKind::Database => {
                add("Activity", catalog::query_tabular(client, "SELECT * FROM pg_catalog.pg_stat_database WHERE datname = $1", &[P::Name(&obj.name)]).await?, &skip);
            }
            _ => {}
        }
        Ok(props)
    }

    pub(crate) async fn search_metadata_impl(&self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        if search.pattern.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = like_pattern(&search.pattern);
        let op = if search.case_sensitive { "LIKE" } else { "ILIKE" };
        let schema_filter = if search.schemas.is_empty() {
            "n.nspname NOT IN ('pg_catalog','information_schema','crdb_internal','pg_extension') \
             AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'"
                .to_string()
        } else {
            "n.nspname = ANY($3::name[])".to_string()
        };
        let comment_or = |_unused: &str| -> String { if search.search_comments { format!(" OR d.description {op} $1") } else { String::new() } };
        let wants = |k: ObjectKind| search.kinds.is_empty() || search.kinds.contains(&k);
        let mut parts: Vec<String> = Vec::new();
        let rel_kinds: Vec<(&str, ObjectKind)> =
            vec![("r','p", ObjectKind::Table), ("v", ObjectKind::View), ("m", ObjectKind::MaterializedView), ("f", ObjectKind::ForeignTable), ("S", ObjectKind::Sequence)];
        for (rk, kind) in rel_kinds {
            if wants(kind) {
                let partition = if kind == ObjectKind::Table { " AND NOT coalesce(c.relispartition, false)" } else { "" };
                parts.push(format!(
                    "SELECT '{}'::text AS k, n.nspname::text AS sch, NULL::text AS parent, c.relname::text AS name, NULL::text AS sig, \
                     d.description AS descr, NULL::text AS detail \
                     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = 0 \
                     WHERE c.relkind IN ('{rk}'){partition} AND {schema_filter} AND (c.relname {op} $1{})",
                    kind_code(kind),
                    comment_or("")
                ));
            }
        }
        if wants(ObjectKind::Column) {
            parts.push(format!(
                "SELECT 'column', n.nspname::text, c.relname::text, a.attname::text, NULL::text, d.description, \
                 pg_catalog.format_type(a.atttypid, a.atttypmod) \
                 FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = a.attnum \
                 WHERE c.relkind IN ('r','p','v','m','f') AND NOT coalesce(c.relispartition, false) AND a.attnum > 0 AND NOT a.attisdropped \
                 AND {schema_filter} AND (a.attname {op} $1{})",
                comment_or("")
            ));
        }
        for (pk, kind) in [("'f','w'", ObjectKind::Function), ("'p'", ObjectKind::Procedure), ("'a'", ObjectKind::Aggregate)] {
            if wants(kind) {
                parts.push(format!(
                    "SELECT '{}', n.nspname::text, NULL::text, p.proname::text, pg_catalog.oidvectortypes(p.proargtypes), d.description, \
                     pg_catalog.pg_get_function_identity_arguments(p.oid) \
                     FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = p.oid AND d.classoid = 'pg_catalog.pg_proc'::regclass AND d.objsubid = 0 \
                     WHERE p.prokind IN ({pk}) AND {schema_filter} AND (p.proname {op} $1{})",
                    kind_code(kind),
                    comment_or("")
                ));
            }
        }
        for (tt, kind) in [("'e','c','r','b'", ObjectKind::Type), ("'d'", ObjectKind::Domain)] {
            if wants(kind) {
                parts.push(format!(
                    "SELECT '{}', n.nspname::text, NULL::text, t.typname::text, NULL::text, d.description, NULL::text \
                     FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                     LEFT JOIN pg_catalog.pg_description d ON d.objoid = t.oid AND d.classoid = 'pg_catalog.pg_type'::regclass AND d.objsubid = 0 \
                     WHERE t.typtype IN ({tt}) AND (t.typrelid = 0 OR EXISTS (SELECT 1 FROM pg_catalog.pg_class cl WHERE cl.oid = t.typrelid AND cl.relkind = 'c')) \
                     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid) \
                     AND {schema_filter} AND (t.typname {op} $1{})",
                    kind_code(kind),
                    comment_or("")
                ));
            }
        }
        if wants(ObjectKind::Index) {
            parts.push(format!(
                "SELECT 'index', n.nspname::text, t.relname::text, c.relname::text, NULL::text, d.description, NULL::text \
                 FROM pg_catalog.pg_index x JOIN pg_catalog.pg_class c ON c.oid = x.indexrelid JOIN pg_catalog.pg_class t ON t.oid = x.indrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.classoid = {DESC_CLASS} AND d.objsubid = 0 \
                 WHERE {schema_filter} AND (c.relname {op} $1{})",
                comment_or("")
            ));
        }
        if wants(ObjectKind::Schema) {
            parts.push(format!(
                "SELECT 'schema', n.nspname::text, NULL::text, n.nspname::text, NULL::text, d.description, NULL::text \
                 FROM pg_catalog.pg_namespace n \
                 LEFT JOIN pg_catalog.pg_description d ON d.objoid = n.oid AND d.classoid = 'pg_catalog.pg_namespace'::regclass \
                 WHERE {schema_filter} AND (n.nspname {op} $1{})",
                comment_or("")
            ));
        }
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let limit = search.limit.clamp(1, 100_000) as i64;
        let raw = search.pattern.trim().to_string();
        let sql = format!(
            "SELECT * FROM ({}) s(k, sch, parent, name, sig, descr, detail) \
             ORDER BY (lower(s.name) = lower($4)) DESC, length(s.name), s.name, s.sch LIMIT $2",
            parts.join(" UNION ALL ")
        );
        let rows = if search.schemas.is_empty() {
            // $3 unused: keep parameter numbering stable
            let sql = sql.replace("$4", "$3");
            self.q(&sql, &[P::Text(&pattern), P::Int8(limit), P::Text(&raw)]).await?
        } else {
            self.q(&sql, &[P::Text(&pattern), P::Int8(limit), P::Names(&search.schemas), P::Text(&raw)]).await?
        };
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let kind = match r.s(0)?.as_str() {
                "table" => ObjectKind::Table,
                "view" => ObjectKind::View,
                "matview" => ObjectKind::MaterializedView,
                "foreign" => ObjectKind::ForeignTable,
                "sequence" => ObjectKind::Sequence,
                "column" => ObjectKind::Column,
                "function" => ObjectKind::Function,
                "procedure" => ObjectKind::Procedure,
                "aggregate" => ObjectKind::Aggregate,
                "type" => ObjectKind::Type,
                "domain" => ObjectKind::Domain,
                "index" => ObjectKind::Index,
                _ => ObjectKind::Schema,
            };
            let schema = r.s(1)?;
            let name = r.s(3)?;
            let obj = match kind {
                ObjectKind::Column | ObjectKind::Index => {
                    let parent = ObjectRef::table(Some(schema.clone()), r.s(2)?);
                    ObjectRef::child(kind, &parent, name)
                }
                ObjectKind::Schema => ObjectRef::new(kind, None, name),
                _ => {
                    let mut o = ObjectRef::new(kind, Some(schema.clone()), name);
                    if let Some(sig) = r.os(4)? {
                        o.signature = Some(format!("({sig})"));
                    }
                    o
                }
            };
            let mut info = ObjectInfo::new(obj).with_comment(r.os(5)?);
            info.detail = r.os(6)?;
            info.is_system = is_system_schema(&schema);
            out.push(info);
        }
        Ok(out)
    }

    /// Qualified name of a relation for messages.
    #[allow(dead_code)]
    pub(crate) fn qualified_name(obj: &ObjectRef) -> String {
        qualified(obj.schema.as_deref(), &obj.name)
    }
}

fn kind_code(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "matview",
        ObjectKind::ForeignTable => "foreign",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Aggregate => "aggregate",
        ObjectKind::Type => "type",
        ObjectKind::Domain => "domain",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert_eq!(like_pattern("user"), "%user%");
        assert_eq!(like_pattern("user*"), "user%");
        assert_eq!(like_pattern("u?er"), "u_er");
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
    }

    #[test]
    fn system_schemas() {
        assert!(is_system_schema("pg_catalog"));
        assert!(is_system_schema("pg_toast"));
        assert!(is_system_schema("pg_temp_3"));
        assert!(is_system_schema("information_schema"));
        assert!(!is_system_schema("public"));
        assert!(!is_system_schema("pg_app"));
    }

    #[test]
    fn routine_identities() {
        assert_eq!(split_routine_identity("public.f(integer, text)", Some("public")), ("f".to_string(), Some("(integer, text)".to_string())));
        assert_eq!(split_routine_identity("\"My Schema\".\"Fn\"(integer)", Some("My Schema")), ("Fn".to_string(), Some("(integer)".to_string())));
    }
}
