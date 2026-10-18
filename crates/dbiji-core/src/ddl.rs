//! Structural definitions of schema objects and the per-dialect DDL generator
//! interface. The same model is produced by drivers (reading catalogs),
//! edited by the table designer, compared by schema compare and rendered by
//! the ER diagram.

use crate::config::DbKind;
use crate::meta::ObjectKind;
use crate::value::DataKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColumnDef {
    pub name: String,
    /// Full database type including modifiers, e.g. `varchar(255)`, `numeric(10,2)`, `integer[]`.
    pub data_type: String,
    pub kind: DataKind,
    pub nullable: bool,
    /// Default value as SQL expression text (`now()`, `'abc'`, `0`).
    pub default: Option<String>,
    /// AUTO_INCREMENT / IDENTITY / SERIAL / AUTOINCREMENT.
    pub auto_increment: bool,
    /// Expression of a generated (computed) column.
    pub generated: Option<String>,
    pub comment: Option<String>,
    pub collation: Option<String>,
    /// 1-based position in the table.
    pub ordinal: u32,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        let data_type = data_type.into();
        ColumnDef {
            name: name.into(),
            kind: DataKind::guess_from_type_name(&data_type),
            data_type,
            nullable: true,
            ..Default::default()
        }
    }
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
    pub fn default_value(mut self, expr: impl Into<String>) -> Self {
        self.default = Some(expr.into());
        self
    }
    pub fn auto_increment(mut self) -> Self {
        self.auto_increment = true;
        self
    }
}

/// Primary key or unique constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeyDef {
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CheckDef {
    pub name: Option<String>,
    /// Boolean expression without the `CHECK` keyword.
    pub expression: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub const ALL: [FkAction; 5] =
        [FkAction::NoAction, FkAction::Restrict, FkAction::Cascade, FkAction::SetNull, FkAction::SetDefault];

    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }

    pub fn parse(s: &str) -> FkAction {
        match s.trim().to_ascii_uppercase().replace('_', " ").as_str() {
            "RESTRICT" => FkAction::Restrict,
            "CASCADE" => FkAction::Cascade,
            "SET NULL" => FkAction::SetNull,
            "SET DEFAULT" => FkAction::SetDefault,
            // PostgreSQL pg_constraint codes
            "R" => FkAction::Restrict,
            "C" => FkAction::Cascade,
            "N" => FkAction::SetNull,
            "D" => FkAction::SetDefault,
            _ => FkAction::NoAction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ForeignKeyDef {
    pub name: Option<String>,
    /// Owning (referencing) table; filled in for `TableDef::referenced_by` entries
    /// and in snapshots, may be empty inside the owning `TableDef::foreign_keys`.
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    pub deferrable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IndexColumn {
    /// Column name, or an expression when `is_expression`.
    pub name: String,
    pub is_expression: bool,
    pub descending: bool,
}

impl IndexColumn {
    pub fn column(name: impl Into<String>) -> Self {
        IndexColumn { name: name.into(), is_expression: false, descending: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<IndexColumn>,
    pub unique: bool,
    /// The index backs the primary key (MySQL `PRIMARY`, PostgreSQL `*_pkey`).
    pub primary: bool,
    /// Access method: btree, hash, gin, gist, brin, fulltext, spatial ...
    pub method: Option<String>,
    /// Partial index predicate (without `WHERE`).
    pub predicate: Option<String>,
    /// Extra included columns (PostgreSQL `INCLUDE`, SQL Server).
    pub include: Vec<String>,
    /// Full definition as reported by the server, if available (`pg_get_indexdef`).
    pub definition: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TriggerDef {
    pub name: String,
    /// BEFORE / AFTER / INSTEAD OF
    pub timing: String,
    /// INSERT / UPDATE / DELETE / TRUNCATE
    pub events: Vec<String>,
    pub for_each_row: bool,
    pub enabled: bool,
    /// Complete `CREATE TRIGGER ...` statement (as the server reports it or as edited).
    pub definition: String,
    /// Trigger body or called function, for display.
    pub body: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PartitioningDef {
    /// RANGE / LIST / HASH / KEY
    pub strategy: String,
    /// Partition key expression list, e.g. `created_at` or `(region, id)`.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PartitionDef {
    pub name: String,
    /// Bound clause, e.g. `FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')`
    /// (PostgreSQL) or `VALUES LESS THAN (2025)` (MySQL).
    pub bound: String,
    pub row_estimate: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    #[default]
    Table,
    PartitionedTable,
    ForeignTable,
    Temporary,
    Unlogged,
}

/// Full table definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TableDef {
    pub schema: Option<String>,
    pub name: String,
    pub table_kind: TableKind,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Option<KeyDef>,
    pub unique_keys: Vec<KeyDef>,
    pub checks: Vec<CheckDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// Secondary indexes (indexes that only back PK/unique constraints may be included with `primary`/`unique`).
    pub indexes: Vec<IndexDef>,
    pub triggers: Vec<TriggerDef>,
    pub partitioning: Option<PartitioningDef>,
    pub partitions: Vec<PartitionDef>,
    pub comment: Option<String>,
    /// Storage options: MySQL `engine`, `charset`, `collation`, `auto_increment`;
    /// PostgreSQL `tablespace`, `with` (reloptions), `inherits`.
    pub options: BTreeMap<String, String>,
    /// Foreign keys of other tables referencing this one (not part of the DDL).
    pub referenced_by: Vec<ForeignKeyDef>,
    pub row_estimate: Option<i64>,
    pub size_bytes: Option<i64>,
}

impl TableDef {
    pub fn new(schema: Option<String>, name: impl Into<String>) -> Self {
        TableDef { schema, name: name.into(), ..Default::default() }
    }

    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Best key for identifying rows: primary key, else the first unique key whose
    /// columns are all NOT NULL, else the first unique key.
    pub fn row_key(&self) -> Option<&KeyDef> {
        if let Some(pk) = &self.primary_key
            && !pk.columns.is_empty() {
                return Some(pk);
            }
        self.unique_keys
            .iter()
            .find(|k| k.columns.iter().all(|c| self.column(c).is_some_and(|c| !c.nullable)))
            .or_else(|| self.unique_keys.first())
    }

    pub fn is_pk_column(&self, column: &str) -> bool {
        self.primary_key.as_ref().is_some_and(|pk| pk.columns.iter().any(|c| c == column))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ViewDef {
    pub schema: Option<String>,
    pub name: String,
    pub materialized: bool,
    /// The SELECT statement (without `CREATE VIEW ... AS`).
    pub definition: String,
    pub columns: Vec<ColumnDef>,
    pub comment: Option<String>,
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SequenceDef {
    pub schema: Option<String>,
    pub name: String,
    pub data_type: Option<String>,
    pub start: Option<i64>,
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: bool,
    /// `table.column` owning the sequence.
    pub owned_by: Option<String>,
    pub last_value: Option<i64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RoutineDef {
    pub schema: Option<String>,
    pub name: String,
    /// Function, Procedure or Aggregate.
    pub kind: Option<ObjectKind>,
    /// Argument list text without parentheses, e.g. `a integer, b text`.
    pub arguments: String,
    pub returns: Option<String>,
    pub language: Option<String>,
    /// Complete `CREATE ...` statement.
    pub definition: String,
    pub comment: Option<String>,
}

impl RoutineDef {
    pub fn signature(&self) -> String {
        format!("{}({})", self.name, self.arguments)
    }
}

/// User defined type (enum, composite, domain, range).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TypeDef {
    pub schema: Option<String>,
    pub name: String,
    /// enum / composite / domain / range / base
    pub category: String,
    /// Enum labels or composite attributes (`name type`).
    pub elements: Vec<String>,
    pub definition: String,
    pub comment: Option<String>,
}

/// Everything in one schema, as needed by ER diagrams, schema compare and autocomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SchemaSnapshot {
    pub kind: Option<DbKind>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub tables: Vec<TableDef>,
    pub views: Vec<ViewDef>,
    pub sequences: Vec<SequenceDef>,
    pub routines: Vec<RoutineDef>,
    pub types: Vec<TypeDef>,
}

impl SchemaSnapshot {
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// What to include when taking a [`SchemaSnapshot`]. Light snapshots
/// (columns + keys only) are used for autocomplete and ER diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotOptions {
    pub columns: bool,
    /// Primary, unique, check and foreign keys (including `referenced_by`).
    pub constraints: bool,
    pub indexes: bool,
    pub triggers: bool,
    pub partitions: bool,
    pub views: bool,
    /// Load view definitions (SELECT text).
    pub view_definitions: bool,
    pub sequences: bool,
    pub routines: bool,
    pub types: bool,
    pub comments: bool,
    pub statistics: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self::full()
    }
}

impl SnapshotOptions {
    pub fn full() -> Self {
        SnapshotOptions {
            columns: true,
            constraints: true,
            indexes: true,
            triggers: true,
            partitions: true,
            views: true,
            view_definitions: true,
            sequences: true,
            routines: true,
            types: true,
            comments: true,
            statistics: false,
        }
    }

    /// Tables, views, columns and keys only.
    pub fn light() -> Self {
        SnapshotOptions {
            columns: true,
            constraints: true,
            indexes: false,
            triggers: false,
            partitions: false,
            views: true,
            view_definitions: false,
            sequences: false,
            routines: false,
            types: false,
            comments: true,
            statistics: false,
        }
    }
}

/// Generates DDL statements for one SQL dialect. Every method returns
/// complete statements without a trailing `;`.
pub trait DdlGenerator: Send + Sync {
    /// `CREATE TABLE` plus everything needed to reproduce the definition:
    /// secondary indexes, comments, triggers. Foreign keys are included inline
    /// when `include_foreign_keys` is true; schema compare passes false and adds
    /// them later with [`DdlGenerator::add_foreign_key`] to respect dependencies.
    fn create_table(&self, table: &TableDef, include_foreign_keys: bool) -> Vec<String>;

    fn drop_table(&self, schema: Option<&str>, name: &str, cascade: bool) -> String;

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String;

    /// Statements that transform `from` into `to`. `renamed_columns` lists
    /// `(old_name, new_name)` pairs the user renamed explicitly; columns not
    /// listed are matched by name. The output must be executable in order.
    fn alter_table(&self, from: &TableDef, to: &TableDef, renamed_columns: &[(String, String)]) -> Vec<String>;

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String>;
    fn drop_column(&self, table: &TableDef, column: &str) -> String;

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String;
    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String;

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String;
    fn drop_primary_key(&self, table: &TableDef, key: &KeyDef) -> String;
    fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String;
    fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String;
    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String;
    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String;
    fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String;
    fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String;

    fn create_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> Vec<String>;
    fn drop_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> String;

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String>;
    fn drop_view(&self, view: &ViewDef) -> String;

    fn create_sequence(&self, seq: &SequenceDef) -> String;
    fn alter_sequence(&self, from: &SequenceDef, to: &SequenceDef) -> Vec<String>;
    fn drop_sequence(&self, seq: &SequenceDef) -> String;

    /// Usually `routine.definition`, possibly normalised (e.g. `CREATE OR REPLACE`).
    fn create_routine(&self, routine: &RoutineDef) -> Vec<String>;
    fn drop_routine(&self, routine: &RoutineDef) -> String;

    fn create_type(&self, ty: &TypeDef) -> Vec<String> {
        if ty.definition.is_empty() { Vec::new() } else { vec![ty.definition.clone()] }
    }
    fn drop_type(&self, _ty: &TypeDef) -> Option<String> {
        None
    }

    fn create_schema(&self, name: &str) -> String;
    fn drop_schema(&self, name: &str, cascade: bool) -> String;

    /// Convert a column type coming from another dialect into this dialect,
    /// used by cross-database migration and schema compare between products.
    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String;

    /// Type used for a logical kind when nothing better is known (CSV import).
    fn default_type_for(&self, kind: DataKind) -> String;
}
