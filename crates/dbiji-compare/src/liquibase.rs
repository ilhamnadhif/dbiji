//! Liquibase XML changelog generation from a schema comparison.
//!
//! The changelog follows the same migration plan as [`crate::migration_script`]
//! (same order, same objects); each logical step becomes one or more
//! changeSets using native Liquibase change types where they exist and a
//! `<sql>` change (rendered by the target dialect) otherwise.

use crate::model::SchemaDiff;
use crate::plan::{MigrationOp, MigrationStep, ScriptOptions, migration_plan};
use crate::util::{xml_cdata, xml_escape};
use dbiji_core::Dialect;
use dbiji_core::ddl::*;
use dbiji_core::dialect::GenericDialect;
use std::fmt;
use std::sync::Arc;

/// Options for [`liquibase_changelog`].
#[derive(Clone)]
pub struct LiquibaseOptions {
    /// `author` attribute of every changeSet.
    pub author: String,
    /// changeSet ids are `<id_prefix>-001`, `<id_prefix>-002` ...
    pub id_prefix: String,
    /// Which differences to apply (drop removed objects, cascade ...).
    pub script: ScriptOptions,
    /// Dialect used for `<sql>` fallbacks (check constraints, routines,
    /// triggers, types) and residual table changes; ANSI SQL when `None`.
    pub dialect: Option<Arc<dyn Dialect>>,
}

impl Default for LiquibaseOptions {
    fn default() -> Self {
        LiquibaseOptions { author: "dbiji".into(), id_prefix: "dbiji".into(), script: ScriptOptions::default(), dialect: None }
    }
}

impl fmt::Debug for LiquibaseOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiquibaseOptions")
            .field("author", &self.author)
            .field("id_prefix", &self.id_prefix)
            .field("script", &self.script)
            .field("dialect", &self.dialect.as_ref().map(|d| d.kind()))
            .finish()
    }
}

struct Xml {
    out: String,
    depth: usize,
}

type Attrs<'a> = Vec<(&'a str, Option<String>)>;

impl Xml {
    fn indent(&mut self) {
        for _ in 0..self.depth {
            self.out.push_str("    ");
        }
    }
    fn tag(&mut self, name: &str, attrs: &Attrs<'_>, close: bool) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            if let Some(v) = v {
                self.out.push(' ');
                self.out.push_str(k);
                self.out.push_str("=\"");
                self.out.push_str(&xml_escape(v));
                self.out.push('"');
            }
        }
        self.out.push_str(if close { "/>\n" } else { ">\n" });
    }
    fn open(&mut self, name: &str, attrs: Attrs<'_>) {
        self.tag(name, &attrs, false);
        self.depth += 1;
    }
    fn empty(&mut self, name: &str, attrs: Attrs<'_>) {
        self.tag(name, &attrs, true);
    }
    fn close(&mut self, name: &str) {
        self.depth = self.depth.saturating_sub(1);
        self.indent();
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push_str(">\n");
    }
    fn text(&mut self, name: &str, text: &str) {
        self.indent();
        self.out.push_str(&format!("<{name}>{}</{name}>\n", xml_escape(text)));
    }
    fn cdata(&mut self, name: &str, attrs: Attrs<'_>, text: &str) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in &attrs {
            if let Some(v) = v {
                self.out.push_str(&format!(" {k}=\"{}\"", xml_escape(v)));
            }
        }
        self.out.push('>');
        self.out.push_str(&xml_cdata(text));
        self.out.push_str(&format!("</{name}>\n"));
    }
}

fn s(v: impl Into<String>) -> Option<String> {
    Some(v.into())
}

fn b(v: bool) -> Option<String> {
    Some(if v { "true".into() } else { "false".into() })
}

fn column_attrs<'x>(c: &'x ColumnDef, with_type: bool) -> Attrs<'x> {
    let mut a: Attrs<'x> = vec![("name", s(c.name.clone()))];
    if with_type {
        a.push(("type", s(c.data_type.clone())));
    }
    if let Some(d) = c.default.as_deref().filter(|d| !d.trim().eq_ignore_ascii_case("null")) {
        a.push(("defaultValueComputed", s(d)));
    }
    if c.auto_increment {
        a.push(("autoIncrement", b(true)));
    }
    if let Some(r) = c.comment.as_deref().filter(|r| !r.is_empty()) {
        a.push(("remarks", s(r)));
    }
    a
}

fn column(x: &mut Xml, c: &ColumnDef, pk: Option<&KeyDef>) {
    let attrs = column_attrs(c, true);
    let is_pk = pk.is_some_and(|k| k.columns.len() == 1 && k.columns[0] == c.name);
    if !c.nullable || is_pk {
        x.open("column", attrs);
        let mut ca: Attrs<'_> = vec![("nullable", if c.nullable && !is_pk { None } else { b(false) })];
        if is_pk {
            ca.push(("primaryKey", b(true)));
            ca.push(("primaryKeyName", pk.and_then(|k| k.name.clone())));
        }
        x.empty("constraints", ca);
        x.close("column");
    } else {
        x.empty("column", attrs);
    }
}

struct Gen<'a> {
    xml: Xml,
    n: usize,
    opts: &'a LiquibaseOptions,
    dialect: &'a dyn Dialect,
    schema: Option<String>,
}

impl Gen<'_> {
    fn change_set(&mut self, comment: &str, body: impl FnOnce(&mut Xml)) {
        self.n += 1;
        let id = format!("{}-{:03}", self.opts.id_prefix, self.n);
        self.xml.open("changeSet", vec![("id", s(id)), ("author", s(self.opts.author.clone()))]);
        if !comment.is_empty() {
            self.xml.text("comment", comment);
        }
        body(&mut self.xml);
        self.xml.close("changeSet");
    }

    /// One changeSet per statement, as `<sql>` changes.
    fn sql(&mut self, comment: &str, statements: &[String]) {
        for st in statements.iter().filter(|x| !x.trim().is_empty()) {
            self.change_set(comment, |x| {
                x.cdata("sql", vec![("splitStatements", s("false")), ("stripComments", s("false"))], st);
            });
        }
    }

    fn schema_attr(&self) -> Option<String> {
        self.schema.clone().filter(|x| !x.is_empty())
    }

    fn create_index(&mut self, table: &str, index: &IndexDef, ctx: &TableDef) {
        let simple = index.predicate.is_none() && index.include.is_empty() && index.method.as_deref().is_none_or(|m| m.eq_ignore_ascii_case("btree"));
        let comment = format!("Create index {} on {table}", index.name);
        if !simple {
            let sql = vec![self.dialect.ddl().create_index(ctx, index)];
            self.sql(&comment, &sql);
            return;
        }
        let schema = self.schema_attr();
        self.change_set(&comment, |x| {
            x.open(
                "createIndex",
                vec![("indexName", s(index.name.clone())), ("tableName", s(table)), ("schemaName", schema), ("unique", if index.unique { b(true) } else { None })],
            );
            for c in &index.columns {
                let mut a: Attrs<'_> = vec![("name", s(c.name.clone()))];
                if c.is_expression {
                    a.push(("computed", b(true)));
                }
                if c.descending {
                    a.push(("descending", b(true)));
                }
                x.empty("column", a);
            }
            x.close("createIndex");
        });
    }

    fn add_fk(&mut self, table: &str, fk: &ForeignKeyDef) {
        let name = fk.name.clone().unwrap_or_else(|| format!("fk_{}_{}", table, fk.columns.join("_")));
        let schema = self.schema_attr();
        let ref_schema = fk.ref_schema.clone().filter(|x| !x.is_empty()).or_else(|| schema.clone());
        self.change_set(&format!("Add foreign key {name} on {table}"), |x| {
            x.empty(
                "addForeignKeyConstraint",
                vec![
                    ("constraintName", s(name.clone())),
                    ("baseTableName", s(table)),
                    ("baseTableSchemaName", schema),
                    ("baseColumnNames", s(fk.columns.join(", "))),
                    ("referencedTableName", s(fk.ref_table.clone())),
                    ("referencedTableSchemaName", ref_schema),
                    ("referencedColumnNames", s(fk.ref_columns.join(", "))),
                    ("onDelete", if fk.on_delete == FkAction::NoAction { None } else { s(fk.on_delete.sql()) }),
                    ("onUpdate", if fk.on_update == FkAction::NoAction { None } else { s(fk.on_update.sql()) }),
                    ("deferrable", if fk.deferrable { b(true) } else { None }),
                ],
            );
        });
    }

    fn create_table(&mut self, t: &TableDef, step: &MigrationStep, inline_fks: bool) {
        if t.columns.iter().any(|c| c.generated.is_some()) {
            // generated columns have no portable Liquibase representation
            self.sql(&step.description, &step.statements);
        } else {
            let pk = t.primary_key.as_ref().filter(|k| !k.columns.is_empty());
            let inline_pk = pk.filter(|k| k.columns.len() == 1);
            let schema = self.schema_attr();
            self.change_set(&step.description, |x| {
                x.open("createTable", vec![("tableName", s(t.name.clone())), ("schemaName", schema.clone()), ("remarks", t.comment.clone().filter(|c| !c.is_empty()))]);
                for c in &t.columns {
                    column(x, c, inline_pk);
                }
                x.close("createTable");
            });
            if let Some(k) = pk.filter(|k| k.columns.len() > 1) {
                self.add_pk(&t.name, k);
            }
            for u in &t.unique_keys {
                self.add_unique(&t.name, u);
            }
            for c in &t.checks {
                let sql = vec![self.dialect.ddl().add_check(t, c)];
                self.sql(&format!("Add check on {}", t.name), &sql);
            }
            let secondary: Vec<IndexDef> = dbiji_core::dialect::ansi::secondary_indexes(t).cloned().collect();
            for i in &secondary {
                self.create_index(&t.name, i, t);
            }
        }
        if inline_fks {
            for fk in &t.foreign_keys {
                self.add_fk(&t.name, fk);
            }
        }
    }

    fn add_pk(&mut self, table: &str, k: &KeyDef) {
        let schema = self.schema_attr();
        self.change_set(&format!("Add primary key on {table}"), |x| {
            x.empty("addPrimaryKey", vec![("tableName", s(table)), ("schemaName", schema), ("columnNames", s(k.columns.join(", "))), ("constraintName", k.name.clone())]);
        });
    }

    fn add_unique(&mut self, table: &str, k: &KeyDef) {
        let schema = self.schema_attr();
        self.change_set(&format!("Add unique key on {table}"), |x| {
            x.empty("addUniqueConstraint", vec![("tableName", s(table)), ("schemaName", schema), ("columnNames", s(k.columns.join(", "))), ("constraintName", k.name.clone())]);
        });
    }

    fn alter_table(&mut self, from: &TableDef, to: &TableDef, renamed: &[(String, String)]) {
        let table = to.name.clone();
        let schema = self.schema_attr();
        let new_name = |old: &str| renamed.iter().find(|(o, _)| o == old).map(|(_, n)| n.clone()).unwrap_or_else(|| old.to_string());
        for (old, new) in renamed {
            let ty = to.column(new).map(|c| c.data_type.clone());
            self.change_set(&format!("Rename column {table}.{old} to {new}"), |x| {
                x.empty(
                    "renameColumn",
                    vec![
                        ("tableName", s(table.clone())),
                        ("schemaName", schema.clone()),
                        ("oldColumnName", s(old.clone())),
                        ("newColumnName", s(new.clone())),
                        ("columnDataType", ty),
                    ],
                );
            });
        }
        for c in &from.columns {
            if to.column(&new_name(&c.name)).is_none() {
                self.change_set(&format!("Drop column {table}.{}", c.name), |x| {
                    x.empty("dropColumn", vec![("tableName", s(table.clone())), ("schemaName", schema.clone()), ("columnName", s(c.name.clone()))]);
                });
            }
        }
        for fc in &from.columns {
            let Some(tc) = to.column(&new_name(&fc.name)) else { continue };
            let base: Attrs<'_> = vec![("tableName", s(table.clone())), ("schemaName", schema.clone()), ("columnName", s(tc.name.clone()))];
            if fc.data_type != tc.data_type {
                let mut a = base.clone();
                a.push(("newDataType", s(tc.data_type.clone())));
                self.change_set(&format!("Change type of {table}.{}", tc.name), |x| x.empty("modifyDataType", a));
            }
            if fc.nullable != tc.nullable {
                let mut a = base.clone();
                a.push(("columnDataType", s(tc.data_type.clone())));
                let tag = if tc.nullable { "dropNotNullConstraint" } else { "addNotNullConstraint" };
                self.change_set(&format!("Change nullability of {table}.{}", tc.name), |x| x.empty(tag, a));
            }
            if fc.default != tc.default {
                let mut a = base.clone();
                a.push(("columnDataType", s(tc.data_type.clone())));
                match tc.default.as_deref().filter(|d| !d.trim().eq_ignore_ascii_case("null")) {
                    Some(d) => {
                        a.push(("defaultValueComputed", s(d)));
                        self.change_set(&format!("Set default of {table}.{}", tc.name), |x| x.empty("addDefaultValue", a));
                    }
                    None => self.change_set(&format!("Drop default of {table}.{}", tc.name), |x| x.empty("dropDefaultValue", a)),
                }
            }
            if fc.comment != tc.comment {
                let mut a = base.clone();
                a.push(("remarks", s(tc.comment.clone().unwrap_or_default())));
                self.change_set(&format!("Set remarks of {table}.{}", tc.name), |x| x.empty("setColumnRemarks", a));
            }
        }
        let added: Vec<&ColumnDef> = to.columns.iter().filter(|tc| !from.columns.iter().any(|fc| new_name(&fc.name) == tc.name)).collect();
        for c in added {
            self.change_set(&format!("Add column {table}.{}", c.name), |x| {
                x.open("addColumn", vec![("tableName", s(table.clone())), ("schemaName", schema.clone())]);
                column(x, c, None);
                x.close("addColumn");
            });
        }
        if from.comment != to.comment {
            self.change_set(&format!("Set remarks of {table}"), |x| {
                x.empty("setTableRemarks", vec![("tableName", s(table.clone())), ("schemaName", schema.clone()), ("remarks", s(to.comment.clone().unwrap_or_default()))]);
            });
        }
        // properties without a Liquibase change type: let the dialect script them
        let mut mid = to.clone();
        for c in &mut mid.columns {
            if let Some(fc) = from.columns.iter().find(|fc| new_name(&fc.name) == c.name) {
                c.auto_increment = fc.auto_increment;
                c.generated = fc.generated.clone();
                c.collation = fc.collation.clone();
            }
        }
        mid.options = from.options.clone();
        mid.partitioning = from.partitioning.clone();
        mid.partitions = from.partitions.clone();
        mid.table_kind = from.table_kind;
        if mid != *to {
            let sql = self.dialect.ddl().alter_table(&mid, to, &[]);
            self.sql(&format!("Alter table {table}"), &sql);
        }
    }

    fn step(&mut self, step: &MigrationStep) {
        let schema = self.schema_attr();
        let desc = step.description.clone();
        match &step.op {
            MigrationOp::DropForeignKey { table, fk } => match &fk.name {
                Some(n) => self.change_set(&desc, |x| {
                    x.empty("dropForeignKeyConstraint", vec![("baseTableName", s(table.clone())), ("baseTableSchemaName", schema), ("constraintName", s(n.clone()))]);
                }),
                None => self.sql(&desc, &step.statements),
            },
            MigrationOp::DropView { view } if !view.materialized => self.change_set(&desc, |x| {
                x.empty("dropView", vec![("viewName", s(view.name.clone())), ("schemaName", schema)]);
            }),
            MigrationOp::DropIndex { table, index } => self.change_set(&desc, |x| {
                x.empty("dropIndex", vec![("indexName", s(index.name.clone())), ("tableName", s(table.clone())), ("schemaName", schema)]);
            }),
            MigrationOp::DropPrimaryKey { table, key } => self.change_set(&desc, |x| {
                x.empty("dropPrimaryKey", vec![("tableName", s(table.clone())), ("schemaName", schema), ("constraintName", key.name.clone())]);
            }),
            MigrationOp::DropUniqueKey { table, key } if key.name.is_some() => self.change_set(&desc, |x| {
                x.empty("dropUniqueConstraint", vec![("tableName", s(table.clone())), ("schemaName", schema), ("constraintName", key.name.clone())]);
            }),
            MigrationOp::DropTable { table, cascade } => self.change_set(&desc, |x| {
                x.empty("dropTable", vec![("tableName", s(table.name.clone())), ("schemaName", schema), ("cascadeConstraints", if *cascade { b(true) } else { None })]);
            }),
            MigrationOp::DropSequence { sequence } => self.change_set(&desc, |x| {
                x.empty("dropSequence", vec![("sequenceName", s(sequence.name.clone())), ("schemaName", schema)]);
            }),
            MigrationOp::CreateSequence { sequence: q } => self.change_set(&desc, |x| {
                x.empty(
                    "createSequence",
                    vec![
                        ("sequenceName", s(q.name.clone())),
                        ("schemaName", schema),
                        ("dataType", q.data_type.clone()),
                        ("startValue", q.start.map(|v| v.to_string())),
                        ("incrementBy", q.increment.map(|v| v.to_string())),
                        ("minValue", q.min_value.map(|v| v.to_string())),
                        ("maxValue", q.max_value.map(|v| v.to_string())),
                        ("cacheSize", q.cache.map(|v| v.to_string())),
                        ("cycle", if q.cycle { b(true) } else { None }),
                    ],
                );
            }),
            MigrationOp::AlterSequence { from, to } => {
                let ch = |a: Option<i64>, b: Option<i64>| if a != b { b.map(|v| v.to_string()) } else { None };
                let attrs = vec![
                    ("sequenceName", s(to.name.clone())),
                    ("schemaName", schema),
                    ("incrementBy", ch(from.increment, to.increment)),
                    ("minValue", ch(from.min_value, to.min_value)),
                    ("maxValue", ch(from.max_value, to.max_value)),
                    ("cacheSize", ch(from.cache, to.cache)),
                    ("cycle", if from.cycle != to.cycle { b(to.cycle) } else { None }),
                ];
                if attrs.iter().skip(2).any(|(_, v)| v.is_some()) {
                    self.change_set(&desc, |x| x.empty("alterSequence", attrs));
                } else {
                    self.sql(&desc, &step.statements);
                }
            }
            MigrationOp::CreateTable { table, inline_foreign_keys } => self.create_table(table, step, *inline_foreign_keys),
            MigrationOp::AlterTable { from, to, renamed_columns } => self.alter_table(from, to, renamed_columns),
            MigrationOp::AddPrimaryKey { table, key } => self.add_pk(table, key),
            MigrationOp::AddUniqueKey { table, key } => self.add_unique(table, key),
            MigrationOp::CreateIndex { table, index } => {
                let ctx = TableDef::new(self.schema.clone(), table.clone());
                self.create_index(table, index, &ctx);
            }
            MigrationOp::AddForeignKey { table, fk } => self.add_fk(table, fk),
            MigrationOp::CreateView { view, .. } if !view.materialized => {
                let def = dbiji_core::dialect::ansi::strip_trailing_semicolon(&view.definition).to_string();
                self.change_set(&desc, |x| {
                    x.cdata("createView", vec![("viewName", s(view.name.clone())), ("schemaName", schema), ("replaceIfExists", b(true))], &def);
                });
            }
            // checks, triggers, routines, types, materialized views ...
            _ => self.sql(&desc, &step.statements),
        }
    }
}

/// Liquibase XML changelog (`databaseChangeLog` with namespace and schema
/// location) applying the differences of `diff` to the target, one change
/// per changeSet.
pub fn liquibase_changelog(diff: &SchemaDiff, source: &SchemaSnapshot, target: &SchemaSnapshot, opts: &LiquibaseOptions) -> String {
    let generic = GenericDialect::new();
    let dialect: &dyn Dialect = match &opts.dialect {
        Some(d) => d.as_ref(),
        None => &generic,
    };
    let plan = migration_plan(diff, source, target, dialect, &opts.script);
    let mut g = Gen { xml: Xml { out: String::with_capacity(8 * 1024), depth: 0 }, n: 0, opts, dialect, schema: crate::analysis::snapshot_schema(target).map(str::to_string) };
    g.xml.out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    g.xml.out.push_str(
        "<databaseChangeLog\n    xmlns=\"http://www.liquibase.org/xml/ns/dbchangelog\"\n    xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n    xsi:schemaLocation=\"http://www.liquibase.org/xml/ns/dbchangelog http://www.liquibase.org/xml/ns/dbchangelog/dbchangelog-latest.xsd\">\n",
    );
    g.xml.depth = 1;
    for w in &plan.warnings {
        g.xml.indent();
        let text = w.replace("--", "- -");
        g.xml.out.push_str(&format!("<!-- WARNING: {} -->\n", xml_escape(&text)));
    }
    for step in &plan.steps {
        g.step(step);
    }
    g.xml.out.push_str("</databaseChangeLog>\n");
    g.xml.out
}
