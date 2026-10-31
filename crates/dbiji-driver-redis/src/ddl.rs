//! DDL of Redis, written as the commands that recreate a key or a database.
//!
//! Redis has no schema: a "table" is a key, its "columns" are the fields of
//! the value. The generic (ANSI) generator backs the methods without an
//! equivalent; [`dbiji_core::Session::object_ddl`] produces the real text.

use dbiji_core::DataKind;
use dbiji_core::ddl::{DdlGenerator, IndexDef, TableDef};
use dbiji_core::dialect::SqlSyntax;
use dbiji_core::dialect::ansi::AnsiDdl;

/// DDL generator for keys.
pub struct RedisDdl {
    ansi: AnsiDdl,
}

impl RedisDdl {
    /// Create the generator.
    pub fn new(syntax: SqlSyntax) -> Self {
        RedisDdl { ansi: AnsiDdl::new(syntax) }
    }
}

/// A Redis command argument, quoted for the editor.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl DdlGenerator for RedisDdl {
    fn create_table(&self, table: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        // The key is created by its first write; `object_ddl` writes the value.
        vec![format!("# key {} ({})", quote(&table.name), table.columns.len())]
    }

    fn drop_table(&self, _schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DEL {}", quote(name))
    }

    fn rename_table(&self, _schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!("RENAME {} {}", quote(old_name), quote(new_name))
    }

    fn alter_table(&self, _from: &TableDef, _to: &TableDef, _renamed_columns: &[(String, String)]) -> Vec<String> {
        Vec::new()
    }

    fn add_column(&self, _table: &TableDef, _column: &dbiji_core::ddl::ColumnDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!("HDEL {} {}", quote(&table.name), quote(column))
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("# {} has no indexes; the key {} is found by name", index.name, quote(&table.name))
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("# {} has no indexes to drop ({})", index.name, quote(&table.name))
    }

    fn add_primary_key(&self, _table: &TableDef, _key: &dbiji_core::ddl::KeyDef) -> String {
        String::new()
    }
    fn drop_primary_key(&self, _table: &TableDef, _key: &dbiji_core::ddl::KeyDef) -> String {
        String::new()
    }
    fn add_unique_key(&self, _table: &TableDef, _key: &dbiji_core::ddl::KeyDef) -> String {
        String::new()
    }
    fn drop_unique_key(&self, _table: &TableDef, _key: &dbiji_core::ddl::KeyDef) -> String {
        String::new()
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
    fn create_view(&self, _view: &dbiji_core::ddl::ViewDef, _or_replace: bool) -> Vec<String> {
        Vec::new()
    }
    fn drop_view(&self, _view: &dbiji_core::ddl::ViewDef) -> String {
        String::new()
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
        // A numbered database exists already; it is empty until something is written.
        format!("SELECT {name}")
    }
    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("SELECT {name}\nFLUSHDB")
    }
    fn map_type(&self, source: dbiji_core::DbKind, data_type: &str, kind: DataKind) -> String {
        self.ansi.map_type(source, data_type, kind)
    }
    fn default_type_for(&self, kind: DataKind) -> String {
        self.ansi.default_type_for(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_commands() {
        let d = RedisDdl::new(SqlSyntax::ansi());
        assert_eq!(d.drop_table(None, "user:1", false), "DEL \"user:1\"");
        assert_eq!(d.rename_table(None, "a", "b"), "RENAME \"a\" \"b\"");
        assert_eq!(d.create_schema("3"), "SELECT 3");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }
}
