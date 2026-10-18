//! Options controlling what schema compare looks at and how.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Callback turning a data type name into a canonical spelling, applied to
/// both sides after the built-in normalisation (lower case, whitespace and,
/// when enabled, [`CompareOptions::normalize_type_aliases`]).
pub type TypeNormalizer = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// A column known to have been renamed: the target column `from` is the
/// source column `to`. The migration renames it instead of dropping and
/// re-adding it (Liquibase: `renameColumn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColumnRename {
    /// Table name on the source side.
    pub table: String,
    /// Column name in the target.
    pub from: String,
    /// Column name in the source.
    pub to: String,
}

/// What to compare and how to match objects.
///
/// Every `bool` in the first group switches one kind of object or property on
/// or off. Disabled properties are neither reported nor changed by the
/// migration script (the target keeps its current value).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompareOptions {
    /// Compare tables at all.
    pub tables: bool,
    /// Compare the column lists of tables.
    pub columns: bool,
    /// Column data types (and collations when both sides report one).
    pub column_types: bool,
    /// Column nullability.
    pub nullability: bool,
    /// Column defaults, identity / auto-increment and generated expressions.
    pub defaults: bool,
    /// Primary keys.
    pub primary_keys: bool,
    /// Unique constraints.
    pub unique_keys: bool,
    /// Check constraints.
    pub checks: bool,
    /// Foreign keys (columns, referenced table and columns, actions).
    pub foreign_keys: bool,
    /// Secondary indexes (indexes backing primary / unique keys are part of the keys).
    pub indexes: bool,
    /// Triggers.
    pub triggers: bool,
    /// Partitioning scheme, partitions and table kind.
    pub partitions: bool,
    /// Storage options such as MySQL `engine` / `charset` or PostgreSQL `tablespace`.
    /// Volatile values (`auto_increment`) are never compared.
    pub table_options: bool,
    /// Views and materialized views.
    pub views: bool,
    /// Sequences (implicit identity sequences are never compared).
    pub sequences: bool,
    /// Functions, procedures and aggregates.
    pub routines: bool,
    /// User defined types and domains.
    pub types: bool,
    /// Table, column and view comments.
    pub comments: bool,
    /// Compare sequence `START` values (usually environment specific).
    pub sequence_start_values: bool,

    /// Match identifiers case-insensitively (`Users` = `users`).
    pub ignore_identifier_case: bool,
    /// Do not report differences in column order.
    pub ignore_column_order: bool,
    /// Compare SQL definitions (views, routines, triggers, checks, defaults)
    /// after normalising whitespace, keyword case, comments, redundant quoting
    /// and qualification with the object's own schema.
    pub normalize_sql: bool,
    /// Match constraints and indexes by structure instead of by name, and do
    /// not report name differences.
    pub ignore_constraint_names: bool,
    /// Treat well known type aliases as equal (`int4` = `integer`,
    /// `varchar` = `character varying`, `int(11)` = `int` ...).
    pub normalize_type_aliases: bool,
    /// Compare schemas of different database products: column types are
    /// compared by logical kind and casts / function spellings are normalised.
    pub cross_dialect: bool,

    /// Source table name -> target table name, for tables that are the same
    /// object under different names (the target keeps its name).
    pub table_mapping: BTreeMap<String, String>,
    /// Known column renames.
    pub column_renames: Vec<ColumnRename>,
    /// Wildcard patterns (`*`, `?`, case-insensitive) of object names to skip
    /// on both sides, e.g. `flyway_schema_history` or `databasechangelog*`.
    pub exclude: Vec<String>,
    /// Optional extra type normalisation (not serialised).
    #[serde(skip)]
    pub type_normalizer: Option<TypeNormalizer>,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions {
            tables: true,
            columns: true,
            column_types: true,
            nullability: true,
            defaults: true,
            primary_keys: true,
            unique_keys: true,
            checks: true,
            foreign_keys: true,
            indexes: true,
            triggers: true,
            partitions: true,
            table_options: true,
            views: true,
            sequences: true,
            routines: true,
            types: true,
            comments: true,
            sequence_start_values: false,
            ignore_identifier_case: false,
            ignore_column_order: true,
            normalize_sql: true,
            ignore_constraint_names: false,
            normalize_type_aliases: true,
            cross_dialect: false,
            table_mapping: BTreeMap::new(),
            column_renames: Vec::new(),
            exclude: Vec::new(),
            type_normalizer: None,
        }
    }
}

impl CompareOptions {
    /// Preset for comparing schemas of two different database products:
    /// logical type comparison, case-insensitive identifiers, structural
    /// constraint matching; routines, triggers, types and table options
    /// (which are product specific) are skipped.
    pub fn cross_dialect() -> Self {
        CompareOptions {
            cross_dialect: true,
            ignore_identifier_case: true,
            ignore_constraint_names: true,
            routines: false,
            triggers: false,
            types: false,
            table_options: false,
            partitions: false,
            ..Default::default()
        }
    }

    /// Set the extra type normalisation callback.
    pub fn with_type_normalizer(mut self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.type_normalizer = Some(Arc::new(f));
        self
    }
}

impl fmt::Debug for CompareOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompareOptions")
            .field("tables", &self.tables)
            .field("columns", &self.columns)
            .field("column_types", &self.column_types)
            .field("nullability", &self.nullability)
            .field("defaults", &self.defaults)
            .field("primary_keys", &self.primary_keys)
            .field("unique_keys", &self.unique_keys)
            .field("checks", &self.checks)
            .field("foreign_keys", &self.foreign_keys)
            .field("indexes", &self.indexes)
            .field("triggers", &self.triggers)
            .field("partitions", &self.partitions)
            .field("table_options", &self.table_options)
            .field("views", &self.views)
            .field("sequences", &self.sequences)
            .field("routines", &self.routines)
            .field("types", &self.types)
            .field("comments", &self.comments)
            .field("sequence_start_values", &self.sequence_start_values)
            .field("ignore_identifier_case", &self.ignore_identifier_case)
            .field("ignore_column_order", &self.ignore_column_order)
            .field("normalize_sql", &self.normalize_sql)
            .field("ignore_constraint_names", &self.ignore_constraint_names)
            .field("normalize_type_aliases", &self.normalize_type_aliases)
            .field("cross_dialect", &self.cross_dialect)
            .field("table_mapping", &self.table_mapping)
            .field("column_renames", &self.column_renames)
            .field("exclude", &self.exclude)
            .field("type_normalizer", &self.type_normalizer.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_skips_callback() {
        let o = CompareOptions::default().with_type_normalizer(|t| t.replace("int4", "integer"));
        let json = serde_json::to_string(&o).unwrap();
        let back: CompareOptions = serde_json::from_str(&json).unwrap();
        assert!(back.type_normalizer.is_none());
        assert!(back.ignore_column_order);
        let partial: CompareOptions = serde_json::from_str(r#"{"views":false}"#).unwrap();
        assert!(!partial.views && partial.tables);
        assert!(format!("{o:?}").contains("<fn>"));
    }
}
