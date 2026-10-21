//! Deciding whether a result set can be edited (FR-D3).

use dbiji_core::{ColumnMeta, ObjectKind, ObjectRef, tr};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The table edits of an editable result are written to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTarget {
    /// Base table of the result.
    pub table: ObjectRef,
    /// Result column indices forming the row key (primary, unique or virtual key).
    pub key_columns: Vec<usize>,
    /// For every result column: the name of the table column it maps to,
    /// `None` for computed columns (read-only within an editable result).
    pub column_names: Vec<Option<String>>,
}

impl EditTarget {
    /// True when result column `col` maps to a table column.
    pub fn is_column_editable(&self, col: usize) -> bool {
        self.column_names.get(col).is_some_and(|c| c.is_some())
    }

    /// True when `col` is part of the row key.
    pub fn is_key_column(&self, col: usize) -> bool {
        self.key_columns.contains(&col)
    }
}

/// Why a result set is read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadOnlyReason {
    /// The columns could not be traced back to a base table (custom query,
    /// expressions only, or the driver did not resolve column sources).
    NoSourceTable,
    /// The columns come from more than one table (joins).
    MultipleTables,
    /// The table has no primary/unique key (and no virtual key was defined),
    /// or the result does not include all key columns.
    NoKey,
    /// The connection is configured as read-only.
    ConnectionReadOnly,
    /// A key column of the table appears in the result only as a computed expression.
    ComputedColumns,
    /// Application specific reason (already translated).
    Custom(String),
}

impl ReadOnlyReason {
    /// Translated explanation shown in the read-only indicator tooltip.
    pub fn message(&self) -> String {
        match self {
            ReadOnlyReason::NoSourceTable => tr("Read-only: the result columns cannot be traced back to a table (custom query or expressions).").to_string(),
            ReadOnlyReason::MultipleTables => tr("Read-only: the result combines columns of several tables (join).").to_string(),
            ReadOnlyReason::NoKey => tr("Read-only: the table has no primary or unique key in the result. Define a virtual key to enable editing.").to_string(),
            ReadOnlyReason::ConnectionReadOnly => tr("Read-only: the connection is read-only.").to_string(),
            ReadOnlyReason::ComputedColumns => tr("Read-only: a key column is computed by an expression and cannot identify rows.").to_string(),
            ReadOnlyReason::Custom(s) => s.clone(),
        }
    }

    /// Short label for status bars.
    pub fn short_label(&self) -> &'static str {
        match self {
            ReadOnlyReason::NoSourceTable => tr("No source table"),
            ReadOnlyReason::MultipleTables => tr("Multiple tables"),
            ReadOnlyReason::NoKey => tr("No unique key"),
            ReadOnlyReason::ConnectionReadOnly => tr("Read-only connection"),
            ReadOnlyReason::ComputedColumns => tr("Computed key"),
            ReadOnlyReason::Custom(_) => tr("Read-only"),
        }
    }

    /// Whether defining a virtual key could make the result editable.
    pub fn virtual_key_helps(&self) -> bool {
        matches!(self, ReadOnlyReason::NoKey)
    }
}

impl fmt::Display for ReadOnlyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// Identity of a source table (database, schema, table name).
type TableIdent<'a> = (Option<&'a str>, Option<&'a str>, &'a str);

/// The single base table of a result, if all sourced columns share one.
pub fn source_table(columns: &[ColumnMeta]) -> Result<ObjectRef, ReadOnlyReason> {
    let mut found: Option<TableIdent<'_>> = None;
    for c in columns {
        let Some(s) = &c.source else { continue };
        let ident = (s.database.as_deref(), s.schema.as_deref(), s.table.as_str());
        match found {
            None => found = Some(ident),
            Some(f) if f == ident => {}
            Some(_) => return Err(ReadOnlyReason::MultipleTables),
        }
    }
    let Some((database, schema, table)) = found else { return Err(ReadOnlyReason::NoSourceTable) };
    Ok(ObjectRef::new(ObjectKind::Table, schema.map(str::to_string), table).with_database(database.map(str::to_string)))
}

/// Determine whether a result with `columns` is editable.
///
/// * `key_lookup` returns the key column names of a table: primary key,
///   else a unique key, else a user defined virtual key; `None`/empty when
///   the table has none. It is called at most once.
/// * `connection_read_only` short-circuits to [`ReadOnlyReason::ConnectionReadOnly`].
pub fn determine_editability(columns: &[ColumnMeta], key_lookup: impl Fn(&ObjectRef) -> Option<Vec<String>>, connection_read_only: bool) -> Result<EditTarget, ReadOnlyReason> {
    if connection_read_only {
        return Err(ReadOnlyReason::ConnectionReadOnly);
    }
    let table = source_table(columns)?;
    let column_names: Vec<Option<String>> = columns.iter().map(|c| c.source.as_ref().filter(|s| s.table == table.name).map(|s| s.column.clone())).collect();
    // A column of the table twice: a self-join (`users a JOIN users b`) whose
    // columns cannot tell which of the two rows they belong to.
    let sourced: Vec<&str> = column_names.iter().flatten().map(String::as_str).collect();
    if sourced.iter().enumerate().any(|(i, c)| sourced[..i].contains(c)) {
        return Err(ReadOnlyReason::MultipleTables);
    }
    let key_names = key_lookup(&table).filter(|k| !k.is_empty()).ok_or(ReadOnlyReason::NoKey)?;
    let mut key_columns = Vec::with_capacity(key_names.len());
    for k in &key_names {
        // Prefer an exact match, then a case-insensitive one.
        let pos = column_names
            .iter()
            .position(|c| c.as_deref() == Some(k.as_str()))
            .or_else(|| column_names.iter().position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(k))));
        match pos {
            Some(p) => key_columns.push(p),
            None => {
                let computed = columns.iter().zip(&column_names).any(|(c, n)| n.is_none() && c.name.eq_ignore_ascii_case(k));
                return Err(if computed { ReadOnlyReason::ComputedColumns } else { ReadOnlyReason::NoKey });
            }
        }
    }
    Ok(EditTarget { table, key_columns, column_names })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnSource, DataKind};

    fn src(table: &str, col: &str) -> ColumnSource {
        ColumnSource { database: None, schema: Some("public".into()), table: table.into(), column: col.into() }
    }

    fn cols() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::new("id", "int4", DataKind::Int).with_source(src("users", "id")),
            ColumnMeta::new("Name", "text", DataKind::Text).with_source(src("users", "name")),
            ColumnMeta::new("upper", "text", DataKind::Text),
        ]
    }

    fn pk(_: &ObjectRef) -> Option<Vec<String>> {
        Some(vec!["id".into()])
    }

    #[test]
    fn editable_single_table() {
        let t = determine_editability(&cols(), pk, false).unwrap();
        assert_eq!(t.table.name, "users");
        assert_eq!(t.table.schema.as_deref(), Some("public"));
        assert_eq!(t.key_columns, vec![0]);
        assert_eq!(t.column_names, vec![Some("id".into()), Some("name".into()), None]);
        assert!(t.is_column_editable(1));
        assert!(!t.is_column_editable(2));
        assert!(t.is_key_column(0));
    }

    #[test]
    fn read_only_reasons() {
        assert_eq!(determine_editability(&cols(), pk, true), Err(ReadOnlyReason::ConnectionReadOnly));
        let none = vec![ColumnMeta::new("x", "int", DataKind::Int)];
        assert_eq!(determine_editability(&none, pk, false), Err(ReadOnlyReason::NoSourceTable));
        let mut join = cols();
        join.push(ColumnMeta::new("total", "int", DataKind::Int).with_source(src("orders", "total")));
        assert_eq!(determine_editability(&join, pk, false), Err(ReadOnlyReason::MultipleTables));
        let mut self_join = cols();
        self_join.push(ColumnMeta::new("name", "text", DataKind::Text).with_source(src("users", "name")));
        assert_eq!(determine_editability(&self_join, pk, false), Err(ReadOnlyReason::MultipleTables));
        assert_eq!(determine_editability(&cols(), |_| None, false), Err(ReadOnlyReason::NoKey));
        assert_eq!(determine_editability(&cols(), |_| Some(vec![]), false), Err(ReadOnlyReason::NoKey));
        // Key column missing from the result.
        let partial = vec![ColumnMeta::new("Name", "text", DataKind::Text).with_source(src("users", "name"))];
        assert_eq!(determine_editability(&partial, pk, false), Err(ReadOnlyReason::NoKey));
        // Key column present only as an expression.
        let computed = vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("Name", "text", DataKind::Text).with_source(src("users", "name"))];
        assert_eq!(determine_editability(&computed, pk, false), Err(ReadOnlyReason::ComputedColumns));
        assert!(ReadOnlyReason::NoKey.virtual_key_helps());
        assert!(!ReadOnlyReason::NoKey.message().is_empty());
    }

    #[test]
    fn virtual_and_composite_keys() {
        let lookup = |t: &ObjectRef| (t.name == "users").then(|| vec!["NAME".to_string(), "id".to_string()]);
        let t = determine_editability(&cols(), lookup, false).unwrap();
        assert_eq!(t.key_columns, vec![1, 0]);
    }
}
