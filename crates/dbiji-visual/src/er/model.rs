//! ER model: tables, columns and relations as shown in a diagram, built from
//! the shared DDL model of `dbiji-core`.

use dbiji_core::ddl::{ColumnDef, ForeignKeyDef, SchemaSnapshot, TableDef, ViewDef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Kind of a diagram box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErTableKind {
    /// Base table (including partitioned and foreign tables).
    #[default]
    Table,
    /// View or materialized view.
    View,
}

/// Change status of a table in a diagram diff (schema compare).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// Only present in the target / new schema.
    Added,
    /// Only present in the source / old schema.
    Removed,
    /// Present in both with different definitions.
    Modified,
    /// Present in both and identical.
    Unchanged,
}

impl DiffStatus {
    /// English label (route through `tr` for display).
    pub fn label(self) -> &'static str {
        match self {
            DiffStatus::Added => "Added",
            DiffStatus::Removed => "Removed",
            DiffStatus::Modified => "Modified",
            DiffStatus::Unchanged => "Unchanged",
        }
    }
}

/// One column row of a diagram box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErColumn {
    /// Column name.
    pub name: String,
    /// Full type text, e.g. `varchar(255)`.
    pub data_type: String,
    /// Part of the primary key.
    pub pk: bool,
    /// Part of at least one foreign key.
    pub fk: bool,
    /// Accepts NULL.
    pub nullable: bool,
    /// Covered by a single-column unique constraint or unique index.
    pub unique: bool,
}

impl ErColumn {
    /// Creates a nullable, non-key column.
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        ErColumn { name: name.into(), data_type: data_type.into(), nullable: true, ..Default::default() }
    }

    /// Marks the column as (part of) the primary key; primary key columns are NOT NULL.
    pub fn primary_key(mut self) -> Self {
        self.pk = true;
        self.nullable = false;
        self
    }

    /// Marks the column as a foreign key column.
    pub fn foreign_key(mut self) -> Self {
        self.fk = true;
        self
    }

    /// Marks the column as NOT NULL.
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Marks the column as unique.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// True for primary or foreign key columns ("keys only" display).
    pub fn is_key(&self) -> bool {
        self.pk || self.fk
    }
}

/// One box of the diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErTable {
    /// Unique key: `schema.table`, or just `table` without schema.
    pub key: String,
    /// Schema (or MySQL database) of the table.
    pub schema: Option<String>,
    /// Table name.
    pub name: String,
    /// Table or view.
    pub kind: ErTableKind,
    /// Columns in table order.
    pub columns: Vec<ErColumn>,
    /// Table comment.
    pub comment: Option<String>,
    /// Diff status when the diagram shows a comparison.
    pub status: Option<DiffStatus>,
}

impl ErTable {
    /// Creates a table box; the key is derived from schema and name.
    pub fn new(schema: Option<&str>, name: impl Into<String>, kind: ErTableKind) -> Self {
        let name = name.into();
        ErTable {
            key: table_key(schema, &name),
            schema: schema.map(str::to_string),
            name,
            kind,
            ..Default::default()
        }
    }

    /// Adds a column (builder style).
    pub fn with_column(mut self, column: ErColumn) -> Self {
        self.columns.push(column);
        self
    }

    /// Column by name.
    pub fn column(&self, name: &str) -> Option<&ErColumn> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Names of the primary key columns in table order.
    pub fn pk_columns(&self) -> Vec<String> {
        self.columns.iter().filter(|c| c.pk).map(|c| c.name.clone()).collect()
    }

    fn from_table_def(def: &TableDef, default_schema: Option<&str>) -> ErTable {
        let schema = def.schema.as_deref().or(default_schema);
        let fk_cols: BTreeSet<&str> =
            def.foreign_keys.iter().flat_map(|fk| fk.columns.iter().map(String::as_str)).collect();
        let mut unique_cols: BTreeSet<&str> = def
            .unique_keys
            .iter()
            .filter(|k| k.columns.len() == 1)
            .flat_map(|k| k.columns.iter().map(String::as_str))
            .collect();
        for idx in def.indexes.iter().filter(|i| i.unique && !i.primary && i.columns.len() == 1) {
            if let Some(c) = idx.columns.first().filter(|c| !c.is_expression) {
                unique_cols.insert(c.name.as_str());
            }
        }
        let pk_single = def.primary_key.as_ref().map(|k| k.columns.len() == 1).unwrap_or(false);
        let columns = def
            .columns
            .iter()
            .map(|c| {
                let pk = def.is_pk_column(&c.name);
                ErColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    pk,
                    fk: fk_cols.contains(c.name.as_str()),
                    nullable: c.nullable && !pk,
                    unique: unique_cols.contains(c.name.as_str()) || (pk && pk_single),
                }
            })
            .collect();
        ErTable {
            key: table_key(schema, &def.name),
            schema: schema.map(str::to_string),
            name: def.name.clone(),
            kind: ErTableKind::Table,
            columns,
            comment: def.comment.clone().filter(|c| !c.trim().is_empty()),
            status: None,
        }
    }

    fn from_view_def(def: &ViewDef, default_schema: Option<&str>) -> ErTable {
        let schema = def.schema.as_deref().or(default_schema);
        ErTable {
            key: table_key(schema, &def.name),
            schema: schema.map(str::to_string),
            name: def.name.clone(),
            kind: ErTableKind::View,
            columns: def.columns.iter().map(view_column).collect(),
            comment: def.comment.clone().filter(|c| !c.trim().is_empty()),
            status: None,
        }
    }

    /// Definition-level equality used by the diff (ignores status).
    fn same_definition(&self, other: &ErTable) -> bool {
        self.kind == other.kind && self.columns == other.columns && self.comment == other.comment
    }
}

fn view_column(c: &ColumnDef) -> ErColumn {
    ErColumn { name: c.name.clone(), data_type: c.data_type.clone(), nullable: c.nullable, ..Default::default() }
}

/// A foreign key drawn as a line between two boxes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErRelation {
    /// Constraint name (synthesised as `<table>_<columns>_fkey` when unnamed).
    pub name: String,
    /// Key of the referencing (child) table.
    pub from_table: String,
    /// FK columns in the child table.
    pub from_columns: Vec<String>,
    /// Key of the referenced (parent) table.
    pub to_table: String,
    /// Referenced columns in the parent table.
    pub to_columns: Vec<String>,
    /// All FK columns are part of the child's primary key.
    pub identifying: bool,
    /// At least one FK column is nullable (the parent is optional).
    pub optional: bool,
}

impl ErRelation {
    /// True when the child table references itself.
    pub fn is_self_reference(&self) -> bool {
        self.from_table == self.to_table
    }
}

/// Tables and relations of a diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErModel {
    /// Boxes, unique by [`ErTable::key`].
    pub tables: Vec<ErTable>,
    /// Foreign keys between tables of the model.
    pub relations: Vec<ErRelation>,
}

/// Key of a table: `schema.name` or `name`.
pub fn table_key(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{s}.{name}"),
        _ => name.to_string(),
    }
}

/// Resolves table references (schema may be omitted) to model keys.
struct KeyResolver {
    keys: BTreeSet<String>,
    by_name: HashMap<String, Vec<String>>,
}

impl KeyResolver {
    fn new<'a>(tables: impl Iterator<Item = &'a ErTable>) -> Self {
        let mut keys = BTreeSet::new();
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for t in tables {
            keys.insert(t.key.clone());
            by_name.entry(t.name.clone()).or_default().push(t.key.clone());
        }
        KeyResolver { keys, by_name }
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Option<String> {
        let key = table_key(schema, name);
        if self.keys.contains(&key) {
            return Some(key);
        }
        // Schema information missing or inconsistent: accept a unique name match.
        match self.by_name.get(name) {
            Some(v) if v.len() == 1 => v.first().cloned(),
            _ => None,
        }
    }
}

fn relation_from_fk(fk: &ForeignKeyDef, child: &ErTable, child_def: &TableDef, to_key: String) -> ErRelation {
    let name = match &fk.name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => format!("{}_{}_fkey", child.name, fk.columns.join("_")),
    };
    let pk: BTreeSet<&str> =
        child_def.primary_key.iter().flat_map(|k| k.columns.iter().map(String::as_str)).collect();
    let identifying = !fk.columns.is_empty() && fk.columns.iter().all(|c| pk.contains(c.as_str()));
    let optional = fk.columns.iter().any(|c| child_def.column(c).map(|c| c.nullable).unwrap_or(true));
    ErRelation {
        name,
        from_table: child.key.clone(),
        from_columns: fk.columns.clone(),
        to_table: to_key,
        to_columns: fk.ref_columns.clone(),
        identifying,
        optional: optional && !identifying,
    }
}

impl ErModel {
    /// Builds a model from a schema snapshot. `filter` restricts the boxes to
    /// the given table keys (`schema.table`) or bare names; relations are kept
    /// when both ends are part of the model.
    pub fn from_snapshot(snapshot: &SchemaSnapshot, filter: Option<&[String]>) -> ErModel {
        let default_schema = snapshot.schema.as_deref();
        let wanted = |schema: Option<&str>, name: &str| -> bool {
            match filter {
                None => true,
                Some(list) => {
                    let key = table_key(schema, name);
                    list.iter().any(|f| f == &key || f == name)
                }
            }
        };
        let mut model = ErModel::default();
        let mut seen = BTreeSet::new();
        let mut defs: Vec<(&TableDef, usize)> = Vec::new();
        for def in &snapshot.tables {
            let schema = def.schema.as_deref().or(default_schema);
            if !wanted(schema, &def.name) {
                continue;
            }
            let t = ErTable::from_table_def(def, default_schema);
            if seen.insert(t.key.clone()) {
                defs.push((def, model.tables.len()));
                model.tables.push(t);
            }
        }
        for view in &snapshot.views {
            let schema = view.schema.as_deref().or(default_schema);
            if !wanted(schema, &view.name) {
                continue;
            }
            let t = ErTable::from_view_def(view, default_schema);
            if seen.insert(t.key.clone()) {
                model.tables.push(t);
            }
        }
        let resolver = KeyResolver::new(model.tables.iter());
        let mut relations = Vec::new();
        for (def, idx) in &defs {
            let Some(child) = model.tables.get(*idx) else { continue };
            for fk in &def.foreign_keys {
                let ref_schema = fk.ref_schema.as_deref().or(child.schema.as_deref());
                if let Some(to_key) = resolver.resolve(ref_schema, &fk.ref_table) {
                    relations.push(relation_from_fk(fk, child, def, to_key));
                }
            }
        }
        model.relations = dedup_relations(relations);
        model
    }

    /// Builds a model from table definitions. With `include_referenced`,
    /// tables referenced by (or referencing) the given ones are added as
    /// placeholder boxes showing only the key columns known from the foreign
    /// keys; otherwise relations to tables outside `tables` are dropped.
    pub fn from_tables(tables: &[TableDef], include_referenced: bool) -> ErModel {
        let mut model = ErModel::default();
        let mut seen = BTreeSet::new();
        let mut defs: Vec<(&TableDef, usize)> = Vec::new();
        for def in tables {
            let t = ErTable::from_table_def(def, None);
            if seen.insert(t.key.clone()) {
                defs.push((def, model.tables.len()));
                model.tables.push(t);
            }
        }
        if include_referenced {
            let mut placeholders: BTreeMap<String, ErTable> = BTreeMap::new();
            for (def, idx) in &defs {
                let child_schema = model.tables.get(*idx).and_then(|t| t.schema.clone());
                for fk in &def.foreign_keys {
                    let schema = fk.ref_schema.clone().or_else(|| child_schema.clone());
                    let key = table_key(schema.as_deref(), &fk.ref_table);
                    if seen.contains(&key) {
                        continue;
                    }
                    let entry = placeholders
                        .entry(key)
                        .or_insert_with(|| ErTable::new(schema.as_deref(), fk.ref_table.clone(), ErTableKind::Table));
                    for c in &fk.ref_columns {
                        if entry.column(c).is_none() {
                            entry.columns.push(ErColumn::new(c.clone(), "").primary_key());
                        }
                    }
                }
                for fk in &def.referenced_by {
                    if fk.table.is_empty() {
                        continue;
                    }
                    let schema = fk.schema.clone().or_else(|| child_schema.clone());
                    let key = table_key(schema.as_deref(), &fk.table);
                    if seen.contains(&key) {
                        continue;
                    }
                    let entry = placeholders
                        .entry(key)
                        .or_insert_with(|| ErTable::new(schema.as_deref(), fk.table.clone(), ErTableKind::Table));
                    for c in &fk.columns {
                        if entry.column(c).is_none() {
                            entry.columns.push(ErColumn::new(c.clone(), "").foreign_key());
                        }
                    }
                }
            }
            model.tables.extend(placeholders.into_values());
        }
        let resolver = KeyResolver::new(model.tables.iter());
        let mut relations = Vec::new();
        for (def, idx) in &defs {
            let Some(child) = model.tables.get(*idx) else { continue };
            for fk in &def.foreign_keys {
                let ref_schema = fk.ref_schema.as_deref().or(child.schema.as_deref());
                if let Some(to_key) = resolver.resolve(ref_schema, &fk.ref_table) {
                    relations.push(relation_from_fk(fk, child, def, to_key));
                }
            }
            // Incoming foreign keys of tables that are not part of `tables`.
            let Some(parent_key) = model.tables.get(*idx).map(|t| t.key.clone()) else { continue };
            for fk in &def.referenced_by {
                if fk.table.is_empty() {
                    continue;
                }
                let schema = fk.schema.as_deref().or(child.schema.as_deref());
                let Some(from_key) = resolver.resolve(schema, &fk.table) else { continue };
                if defs.iter().any(|(_, i)| model.tables.get(*i).map(|t| t.key == from_key).unwrap_or(false)) {
                    continue; // the owner's own foreign_keys already produced it
                }
                let name = fk.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
                    format!("{}_{}_fkey", fk.table, fk.columns.join("_"))
                });
                relations.push(ErRelation {
                    name,
                    from_table: from_key,
                    from_columns: fk.columns.clone(),
                    to_table: parent_key.clone(),
                    to_columns: fk.ref_columns.clone(),
                    identifying: false,
                    optional: true,
                });
            }
        }
        model.relations = dedup_relations(relations);
        // Mark FK columns of placeholder children.
        let fk_cols: Vec<(String, String)> = model
            .relations
            .iter()
            .flat_map(|r| r.from_columns.iter().map(move |c| (r.from_table.clone(), c.clone())))
            .collect();
        for (table, col) in fk_cols {
            if let Some(t) = model.tables.iter_mut().find(|t| t.key == table)
                && let Some(c) = t.columns.iter_mut().find(|c| c.name == col) {
                    c.fk = true;
                }
        }
        model
    }

    /// Sets the diff status of every table: tables missing from `statuses`
    /// get no status.
    pub fn set_statuses(&mut self, statuses: BTreeMap<String, DiffStatus>) {
        for t in &mut self.tables {
            t.status = statuses.get(&t.key).copied();
        }
    }

    /// Diagram diff: the union of `old` and `new` with every table marked
    /// Added / Removed / Modified / Unchanged (FR-M3 "diagram diff").
    /// Removed tables keep their old definition; relations of both versions
    /// are kept.
    pub fn diff(old: &ErModel, new: &ErModel) -> ErModel {
        let old_by_key: HashMap<&str, &ErTable> = old.tables.iter().map(|t| (t.key.as_str(), t)).collect();
        let new_keys: BTreeSet<&str> = new.tables.iter().map(|t| t.key.as_str()).collect();
        let mut tables = Vec::new();
        for t in &new.tables {
            let mut t2 = t.clone();
            t2.status = Some(match old_by_key.get(t.key.as_str()) {
                None => DiffStatus::Added,
                Some(o) if o.same_definition(t) => DiffStatus::Unchanged,
                Some(_) => DiffStatus::Modified,
            });
            tables.push(t2);
        }
        for t in &old.tables {
            if !new_keys.contains(t.key.as_str()) {
                let mut t2 = t.clone();
                t2.status = Some(DiffStatus::Removed);
                tables.push(t2);
            }
        }
        let mut relations = new.relations.clone();
        relations.extend(old.relations.iter().cloned());
        let keys: BTreeSet<String> = tables.iter().map(|t| t.key.clone()).collect();
        relations.retain(|r| keys.contains(&r.from_table) && keys.contains(&r.to_table));
        ErModel { tables, relations: dedup_relations(relations) }
    }

    /// Table by key.
    pub fn table(&self, key: &str) -> Option<&ErTable> {
        self.tables.iter().find(|t| t.key == key)
    }

    /// Mutable table by key.
    pub fn table_mut(&mut self, key: &str) -> Option<&mut ErTable> {
        self.tables.iter_mut().find(|t| t.key == key)
    }

    /// Keys of tables directly connected to `key` by a relation.
    pub fn related_keys(&self, key: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for r in &self.relations {
            if r.from_table == key {
                out.insert(r.to_table.clone());
            }
            if r.to_table == key {
                out.insert(r.from_table.clone());
            }
        }
        out.remove(key);
        out
    }

    /// Removes tables (and their relations) from the model.
    pub fn remove_tables(&mut self, keys: &BTreeSet<String>) {
        self.tables.retain(|t| !keys.contains(&t.key));
        self.relations.retain(|r| !keys.contains(&r.from_table) && !keys.contains(&r.to_table));
    }

    /// Adds the tables and relations of `other` that are not present yet.
    /// Returns the keys of the newly added tables.
    pub fn merge(&mut self, other: ErModel) -> Vec<String> {
        let mut keys: BTreeSet<String> = self.tables.iter().map(|t| t.key.clone()).collect();
        let mut added = Vec::new();
        for t in other.tables {
            if keys.insert(t.key.clone()) {
                added.push(t.key.clone());
                self.tables.push(t);
            }
        }
        let mut relations = std::mem::take(&mut self.relations);
        relations.extend(other.relations);
        relations.retain(|r| keys.contains(&r.from_table) && keys.contains(&r.to_table));
        self.relations = dedup_relations(relations);
        added
    }

    /// Recomputes the `fk` flag of columns from the relations (after edits).
    pub fn refresh_fk_flags(&mut self) {
        let fk: BTreeSet<(String, String)> = self
            .relations
            .iter()
            .flat_map(|r| r.from_columns.iter().map(move |c| (r.from_table.clone(), c.clone())))
            .collect();
        for t in &mut self.tables {
            for c in &mut t.columns {
                c.fk = fk.contains(&(t.key.clone(), c.name.clone()));
            }
        }
    }
}

/// Removes duplicate relations (same child, columns and parent).
fn dedup_relations(relations: Vec<ErRelation>) -> Vec<ErRelation> {
    let mut seen = BTreeSet::new();
    relations
        .into_iter()
        .filter(|r| seen.insert((r.from_table.clone(), r.from_columns.clone(), r.to_table.clone(), r.to_columns.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ddl::{ColumnDef, KeyDef};

    fn users() -> TableDef {
        let mut t = TableDef::new(Some("app".into()), "users");
        t.columns.push(ColumnDef::new("id", "integer").not_null());
        t.columns.push(ColumnDef::new("email", "text").not_null());
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        t.unique_keys.push(KeyDef { name: Some("users_email_key".into()), columns: vec!["email".into()] });
        t
    }

    fn orders() -> TableDef {
        let mut t = TableDef::new(Some("app".into()), "orders");
        t.columns.push(ColumnDef::new("id", "integer").not_null());
        t.columns.push(ColumnDef::new("user_id", "integer"));
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("orders_user_fk".into()),
            columns: vec!["user_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        t
    }

    fn order_items() -> TableDef {
        let mut t = TableDef::new(Some("app".into()), "order_items");
        t.columns.push(ColumnDef::new("order_id", "integer").not_null());
        t.columns.push(ColumnDef::new("line", "integer").not_null());
        t.primary_key = Some(KeyDef { name: None, columns: vec!["order_id".into(), "line".into()] });
        t.foreign_keys.push(ForeignKeyDef {
            name: None,
            columns: vec!["order_id".into()],
            ref_schema: Some("app".into()),
            ref_table: "orders".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        t
    }

    #[test]
    fn snapshot_model() {
        let snap = SchemaSnapshot {
            schema: Some("app".into()),
            tables: vec![users(), orders(), order_items()],
            views: vec![ViewDef {
                schema: None,
                name: "active_users".into(),
                columns: vec![ColumnDef::new("id", "integer")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let m = ErModel::from_snapshot(&snap, None);
        assert_eq!(m.tables.len(), 4);
        let u = m.table("app.users").unwrap();
        assert!(u.column("id").unwrap().pk && u.column("id").unwrap().unique);
        assert!(u.column("email").unwrap().unique);
        let v = m.table("app.active_users").unwrap();
        assert_eq!(v.kind, ErTableKind::View);
        assert_eq!(m.relations.len(), 2);
        let r = m.relations.iter().find(|r| r.from_table == "app.orders").unwrap();
        assert_eq!(r.to_table, "app.users");
        assert!(r.optional && !r.identifying);
        let r2 = m.relations.iter().find(|r| r.from_table == "app.order_items").unwrap();
        assert!(r2.identifying && !r2.optional);
        assert_eq!(r2.name, "order_items_order_id_fkey");
        assert!(m.table("app.orders").unwrap().column("user_id").unwrap().fk);

        let filtered = ErModel::from_snapshot(&snap, Some(&["orders".into(), "app.order_items".into()]));
        assert_eq!(filtered.tables.len(), 2);
        assert_eq!(filtered.relations.len(), 1, "relation to users dropped");
    }

    #[test]
    fn tables_with_referenced_placeholders() {
        let m = ErModel::from_tables(&[orders()], false);
        assert_eq!(m.tables.len(), 1);
        assert!(m.relations.is_empty());
        let m = ErModel::from_tables(&[orders()], true);
        assert_eq!(m.tables.len(), 2);
        let users = m.table("app.users").unwrap();
        assert_eq!(users.columns.len(), 1);
        assert!(users.columns[0].pk);
        assert_eq!(m.relations.len(), 1);

        let mut u = users_def_with_incoming();
        u.schema = Some("app".into());
        let m = ErModel::from_tables(&[u], true);
        assert!(m.table("app.comments").is_some());
        assert_eq!(m.relations.len(), 1);
        assert_eq!(m.relations[0].from_table, "app.comments");
        assert!(m.table("app.comments").unwrap().column("author_id").unwrap().fk);
    }

    fn users_def_with_incoming() -> TableDef {
        let mut u = users();
        u.referenced_by.push(ForeignKeyDef {
            name: Some("comments_author_fk".into()),
            schema: Some("app".into()),
            table: "comments".into(),
            columns: vec!["author_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        u
    }

    #[test]
    fn statuses_and_diff() {
        let old = ErModel::from_tables(&[users(), orders()], false);
        let mut changed = orders();
        changed.columns.push(ColumnDef::new("total", "numeric"));
        let new = ErModel::from_tables(&[changed, order_items()], false);
        let d = ErModel::diff(&old, &new);
        let status = |k: &str| d.table(k).unwrap().status;
        assert_eq!(status("app.orders"), Some(DiffStatus::Modified));
        assert_eq!(status("app.order_items"), Some(DiffStatus::Added));
        assert_eq!(status("app.users"), Some(DiffStatus::Removed));
        assert_eq!(d.relations.len(), 2);

        let mut m = old.clone();
        m.set_statuses(BTreeMap::from([("app.users".to_string(), DiffStatus::Added)]));
        assert_eq!(m.table("app.users").unwrap().status, Some(DiffStatus::Added));
        assert_eq!(m.table("app.orders").unwrap().status, None);
    }

    #[test]
    fn merge_and_remove() {
        let mut m = ErModel::from_tables(&[users()], false);
        let added = m.merge(ErModel::from_tables(&[users(), orders()], false));
        assert_eq!(added, vec!["app.orders".to_string()]);
        assert_eq!(m.relations.len(), 1);
        assert_eq!(m.related_keys("app.users"), BTreeSet::from(["app.orders".to_string()]));
        m.remove_tables(&BTreeSet::from(["app.users".to_string()]));
        assert_eq!(m.tables.len(), 1);
        assert!(m.relations.is_empty());
    }
}
