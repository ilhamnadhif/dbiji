//! DDL of MongoDB, written as shell commands: collections and their indexes.
//!
//! The generic (ANSI) generator backs the methods MongoDB has no equivalent
//! for; the navigator and the DDL tab use [`dbiji_core::Session::object_ddl`]
//! instead, which also reports the collection options.

use dbiji_core::ddl::{DdlGenerator, IndexDef, TableDef};
use dbiji_core::dialect::SqlSyntax;
use dbiji_core::dialect::ansi::AnsiDdl;

/// DDL generator for collections.
pub struct MongoDdl {
    ansi: AnsiDdl,
}

impl MongoDdl {
    /// Create the generator.
    pub fn new(syntax: SqlSyntax) -> Self {
        MongoDdl { ansi: AnsiDdl::new(syntax) }
    }

    /// `db.coll` for a collection name.
    fn target(schema: Option<&str>, name: &str) -> String {
        match schema.filter(|s| !s.is_empty()) {
            Some(db) => format!("db.getSiblingDB({}).getCollection({})", quote(db), quote(name)),
            None => format!("db.getCollection({})", quote(name)),
        }
    }
}

/// A shell string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl DdlGenerator for MongoDdl {
    fn create_table(&self, table: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        let target = Self::target(table.schema.as_deref(), &table.name);
        let mut out = vec![format!("{target}.__create()")];
        for idx in &table.indexes {
            out.push(self.create_index(table, idx));
        }
        // `__create` stands for `db.createCollection(name, options)`; the
        // navigator shows the real statement (see `object_ddl`).
        let name = quote(&table.name);
        out[0] = match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(db) => format!("db.getSiblingDB({}).createCollection({name})", quote(db)),
            None => format!("db.createCollection({name})"),
        };
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("{}.drop()", Self::target(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!("db.adminCommand({{renameCollection: {}.getFullName(), to: {}.getFullName()}})", Self::target(schema, old_name), Self::target(schema, new_name))
    }

    fn alter_table(&self, from: &TableDef, to: &TableDef, renamed_columns: &[(String, String)]) -> Vec<String> {
        // A collection has no schema: only index changes apply.
        let mut out = Vec::new();
        for idx in &to.indexes {
            if !from.indexes.iter().any(|i| i.name == idx.name) {
                out.push(self.create_index(to, idx));
            }
        }
        for idx in &from.indexes {
            if !to.indexes.iter().any(|i| i.name == idx.name) {
                out.push(self.drop_index(from, idx));
            }
        }
        let _ = renamed_columns;
        out
    }

    fn add_column(&self, _table: &TableDef, _column: &dbiji_core::ddl::ColumnDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_column(&self, _table: &TableDef, _column: &str) -> String {
        String::new()
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        let target = Self::target(table.schema.as_deref(), &table.name);
        let keys = index.columns.iter().map(|c| format!("{}: {}", quote(&c.name), if c.descending { -1 } else { 1 })).collect::<Vec<_>>().join(", ");
        let mut options = Vec::new();
        if !index.name.is_empty() {
            options.push(format!("name: {}", quote(&index.name)));
        }
        if index.unique {
            options.push("unique: true".into());
        }
        if let Some(p) = index.predicate.as_deref().filter(|p| !p.is_empty()) {
            options.push(format!("partialFilterExpression: {p}"));
        }
        if options.is_empty() { format!("{target}.createIndex({{{keys}}})") } else { format!("{target}.createIndex({{{keys}}}, {{{}}})", options.join(", ")) }
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("{}.dropIndex({})", Self::target(table.schema.as_deref(), &table.name), quote(&index.name))
    }

    fn add_primary_key(&self, table: &TableDef, _key: &dbiji_core::ddl::KeyDef) -> String {
        format!("{}.createIndex({{_id: 1}}, {{unique: true}})", Self::target(table.schema.as_deref(), &table.name))
    }
    fn drop_primary_key(&self, table: &TableDef, key: &dbiji_core::ddl::KeyDef) -> String {
        let name = key.name.clone().unwrap_or_else(|| "_id_".into());
        format!("{}.dropIndex({})", Self::target(table.schema.as_deref(), &table.name), quote(&name))
    }
    fn add_unique_key(&self, table: &TableDef, key: &dbiji_core::ddl::KeyDef) -> String {
        let target = Self::target(table.schema.as_deref(), &table.name);
        let keys = key.columns.iter().map(|c| format!("{}: 1", quote(c))).collect::<Vec<_>>().join(", ");
        format!("{target}.createIndex({{{keys}}}, {{unique: true}})")
    }
    fn drop_unique_key(&self, table: &TableDef, key: &dbiji_core::ddl::KeyDef) -> String {
        let name = key.name.clone().unwrap_or_else(|| key.columns.join("_"));
        format!("{}.dropIndex({})", Self::target(table.schema.as_deref(), &table.name), quote(&name))
    }
    fn add_check(&self, _table: &TableDef, _check: &dbiji_core::ddl::CheckDef) -> String {
        String::new()
    }
    fn drop_check(&self, _table: &TableDef, _check: &dbiji_core::ddl::CheckDef) -> String {
        String::new()
    }
    fn add_foreign_key(&self, _table: &TableDef, _fk: &dbiji_core::ddl::ForeignKeyDef) -> String {
        String::new()
    }
    fn drop_foreign_key(&self, _table: &TableDef, _fk: &dbiji_core::ddl::ForeignKeyDef) -> String {
        String::new()
    }
    fn create_trigger(&self, _table: &TableDef, _trigger: &dbiji_core::ddl::TriggerDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_trigger(&self, _table: &TableDef, _trigger: &dbiji_core::ddl::TriggerDef) -> String {
        String::new()
    }
    fn create_view(&self, view: &dbiji_core::ddl::ViewDef, _or_replace: bool) -> Vec<String> {
        let target = Self::target(view.schema.as_deref(), &view.name);
        vec![format!("{}.__view()", target)]
    }
    fn drop_view(&self, view: &dbiji_core::ddl::ViewDef) -> String {
        format!("{}.drop()", Self::target(view.schema.as_deref(), &view.name))
    }
    fn create_sequence(&self, _seq: &dbiji_core::ddl::SequenceDef) -> String {
        String::new()
    }
    fn alter_sequence(&self, _from: &dbiji_core::ddl::SequenceDef, _to: &dbiji_core::ddl::SequenceDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_sequence(&self, _seq: &dbiji_core::ddl::SequenceDef) -> String {
        String::new()
    }
    fn create_routine(&self, _routine: &dbiji_core::ddl::RoutineDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_routine(&self, _routine: &dbiji_core::ddl::RoutineDef) -> String {
        String::new()
    }
    fn create_schema(&self, name: &str) -> String {
        // A database exists once it has a collection.
        format!("db.getSiblingDB({}).createCollection(\"__dbiji\")", quote(name))
    }
    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("db.getSiblingDB({}).dropDatabase()", quote(name))
    }
    fn map_type(&self, source: dbiji_core::DbKind, data_type: &str, kind: dbiji_core::DataKind) -> String {
        self.ansi.map_type(source, data_type, kind)
    }
    fn default_type_for(&self, kind: dbiji_core::DataKind) -> String {
        self.ansi.default_type_for(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ddl::{ColumnDef, IndexColumn};

    fn table() -> TableDef {
        TableDef { schema: Some("shop".into()), name: "orders".into(), columns: vec![ColumnDef::new("total", "double")], ..Default::default() }
    }

    #[test]
    fn collections_and_indexes_are_shell_commands() {
        let d = MongoDdl::new(SqlSyntax::ansi());
        let t = table();
        assert_eq!(d.drop_table(Some("shop"), "orders", false), "db.getSiblingDB(\"shop\").getCollection(\"orders\").drop()");
        let mut idx = IndexDef { name: "by_total".into(), unique: true, ..Default::default() };
        idx.columns = vec![IndexColumn { name: "total".into(), is_expression: false, descending: true }];
        assert_eq!(d.create_index(&t, &idx), "db.getSiblingDB(\"shop\").getCollection(\"orders\").createIndex({\"total\": -1}, {name: \"by_total\", unique: true})");
        assert_eq!(d.drop_index(&t, &idx), "db.getSiblingDB(\"shop\").getCollection(\"orders\").dropIndex(\"by_total\")");
        assert_eq!(d.drop_schema("shop", false), "db.getSiblingDB(\"shop\").dropDatabase()");
    }
}
