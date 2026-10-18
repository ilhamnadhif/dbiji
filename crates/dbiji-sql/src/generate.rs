//! SQL generation from objects and grid selections (FR-S10).
//!
//! Table based generators produce templates with placeholders
//! ([`Dialect::placeholder`], or `:column` named parameters); row based
//! generators produce one complete statement per row with literals rendered
//! by [`Dialect::literal`] using the column type. Statements are returned
//! without a trailing `;` unless noted.

use dbiji_core::ddl::TableDef;
use dbiji_core::value::{ColumnMeta, DataKind, Row, Value};
use dbiji_core::{Dialect, ObjectRef};
use serde::{Deserialize, Serialize};

/// Options of the table based generators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenerateOptions {
    /// Qualify the table with its schema.
    pub fully_qualified: bool,
    /// `:column` parameters instead of the dialect placeholders (`$1`, `?`).
    pub named_parameters: bool,
    /// Leave auto-increment and generated columns out of INSERT statements.
    pub skip_auto_columns: bool,
    /// One line instead of one clause per line.
    pub compact: bool,
    /// `SELECT *` instead of listing the columns.
    pub select_star: bool,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        GenerateOptions {
            fully_qualified: true,
            named_parameters: false,
            skip_auto_columns: false,
            compact: false,
            select_star: false,
        }
    }
}

fn table_name(d: &dyn Dialect, t: &TableDef, opts: &GenerateOptions) -> String {
    if opts.fully_qualified {
        d.qualified(t.schema.as_deref(), &t.name)
    } else {
        d.quote_ident(&t.name)
    }
}

fn nl(opts: &GenerateOptions) -> &'static str {
    if opts.compact { " " } else { "\n" }
}

/// Named parameter for a column: non identifier characters become `_`.
fn param_name(col: &str) -> String {
    let mut s: String = col
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, 'p');
    }
    s
}

struct Params<'a> {
    d: &'a dyn Dialect,
    named: bool,
    next: usize,
}

impl Params<'_> {
    fn next(&mut self, column: &str) -> String {
        self.next += 1;
        if self.named {
            format!(":{}", param_name(column))
        } else {
            self.d.placeholder(self.next)
        }
    }
}

/// Key columns used to identify rows: the row key, else every column.
fn key_columns(t: &TableDef) -> Vec<String> {
    match t.row_key() {
        Some(k) if !k.columns.is_empty() => k.columns.clone(),
        _ => t.columns.iter().map(|c| c.name.clone()).collect(),
    }
}

/// `SELECT col, ... FROM table`.
pub fn select_sql(d: &dyn Dialect, table: &TableDef, opts: &GenerateOptions) -> String {
    let cols = if opts.select_star || table.columns.is_empty() {
        "*".to_string()
    } else {
        table
            .columns
            .iter()
            .map(|c| d.quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "SELECT {cols}{}FROM {}",
        nl(opts),
        table_name(d, table, opts)
    )
}

fn insert_columns<'t>(table: &'t TableDef, opts: &GenerateOptions) -> Vec<&'t str> {
    table
        .columns
        .iter()
        .filter(|c| !(opts.skip_auto_columns && (c.auto_increment || c.generated.is_some())))
        .map(|c| c.name.as_str())
        .collect()
}

/// `INSERT INTO table (cols) VALUES (placeholders)`.
pub fn insert_sql(d: &dyn Dialect, table: &TableDef, opts: &GenerateOptions) -> String {
    let cols = insert_columns(table, opts);
    let mut p = Params {
        d,
        named: opts.named_parameters,
        next: 0,
    };
    let values: Vec<String> = cols.iter().map(|c| p.next(c)).collect();
    let list: Vec<String> = cols.iter().map(|c| d.quote_ident(c)).collect();
    format!(
        "INSERT INTO {} ({}){}VALUES ({})",
        table_name(d, table, opts),
        list.join(", "),
        nl(opts),
        values.join(", ")
    )
}

/// `UPDATE table SET col = ? ... WHERE key = ?`.
pub fn update_sql(d: &dyn Dialect, table: &TableDef, opts: &GenerateOptions) -> String {
    let keys = key_columns(table);
    let mut set_cols: Vec<&str> = table
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|c| !keys.iter().any(|k| k == c))
        .collect();
    if set_cols.is_empty() {
        set_cols = table.columns.iter().map(|c| c.name.as_str()).collect();
    }
    let mut p = Params {
        d,
        named: opts.named_parameters,
        next: 0,
    };
    let sets: Vec<String> = set_cols
        .iter()
        .map(|c| format!("{} = {}", d.quote_ident(c), p.next(c)))
        .collect();
    let wheres: Vec<String> = keys
        .iter()
        .map(|c| format!("{} = {}", d.quote_ident(c), p.next(c)))
        .collect();
    let mut s = format!(
        "UPDATE {}{}SET {}",
        table_name(d, table, opts),
        nl(opts),
        sets.join(", ")
    );
    if !wheres.is_empty() {
        s.push_str(nl(opts));
        s.push_str("WHERE ");
        s.push_str(&wheres.join(" AND "));
    }
    s
}

/// `DELETE FROM table WHERE key = ?`.
pub fn delete_sql(d: &dyn Dialect, table: &TableDef, opts: &GenerateOptions) -> String {
    let keys = key_columns(table);
    let mut p = Params {
        d,
        named: opts.named_parameters,
        next: 0,
    };
    let wheres: Vec<String> = keys
        .iter()
        .map(|c| format!("{} = {}", d.quote_ident(c), p.next(c)))
        .collect();
    let mut s = format!("DELETE FROM {}", table_name(d, table, opts));
    if !wheres.is_empty() {
        s.push_str(nl(opts));
        s.push_str("WHERE ");
        s.push_str(&wheres.join(" AND "));
    }
    s
}

/// Insert-or-update statement via [`Dialect::upsert`]; `None` when the
/// dialect has no upsert or the table has no key.
pub fn upsert_sql(d: &dyn Dialect, table: &TableDef, opts: &GenerateOptions) -> Option<String> {
    let key = table.row_key().filter(|k| !k.columns.is_empty())?;
    let cols = insert_columns(table, opts);
    let mut p = Params {
        d,
        named: opts.named_parameters,
        next: 0,
    };
    let values: Vec<String> = cols.iter().map(|c| p.next(c)).collect();
    let quoted: Vec<String> = cols.iter().map(|c| d.quote_ident(c)).collect();
    let keys: Vec<String> = key.columns.iter().map(|c| d.quote_ident(c)).collect();
    d.upsert(&table_name(d, table, opts), &quoted, &keys, &values)
}

/// `CREATE TABLE` script (with indexes and constraints), statements
/// terminated by `;` and separated by blank lines.
pub fn ddl_sql(d: &dyn Dialect, table: &TableDef) -> String {
    let stmts = d.ddl().create_table(table, true);
    stmts
        .iter()
        .map(|s| format!("{};", s.trim_end().trim_end_matches(';')))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn column_name(c: &ColumnMeta) -> &str {
    c.source
        .as_ref()
        .map(|s| s.column.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&c.name)
}

fn literal(d: &dyn Dialect, c: &ColumnMeta, v: &Value) -> String {
    let ty = Some(c.type_name.as_str()).filter(|t| !t.is_empty());
    d.literal(v, ty)
}

/// Types without a usable `=` operator, left out of WHERE clauses that
/// identify rows by all their columns.
fn comparable(c: &ColumnMeta) -> bool {
    let t = c.type_name.to_ascii_lowercase();
    !(matches!(c.kind, DataKind::Xml | DataKind::Geometry) || t == "json" || t == "xml")
}

fn row_value(row: &Row, i: usize) -> &Value {
    static NULL: Value = Value::Null;
    row.get(i).unwrap_or(&NULL)
}

fn condition(d: &dyn Dialect, columns: &[ColumnMeta], idx: &[usize], row: &Row) -> String {
    idx.iter()
        .filter_map(|&i| columns.get(i).map(|c| (i, c)))
        .map(|(i, c)| {
            let v = row_value(row, i);
            let name = d.quote_ident(column_name(c));
            if v.is_null() {
                format!("{name} IS NULL")
            } else {
                format!("{name} = {}", literal(d, c, v))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn where_columns(columns: &[ColumnMeta], key_cols: &[usize]) -> Vec<usize> {
    let keys: Vec<usize> = key_cols
        .iter()
        .copied()
        .filter(|&i| i < columns.len())
        .collect();
    if keys.is_empty() {
        (0..columns.len())
            .filter(|&i| comparable(&columns[i]))
            .collect()
    } else {
        keys
    }
}

/// One `INSERT` per row.
pub fn rows_insert_sql(
    d: &dyn Dialect,
    table: &ObjectRef,
    columns: &[ColumnMeta],
    rows: &[Row],
) -> Vec<String> {
    let t = d.qualified_ref(table);
    let names: Vec<String> = columns
        .iter()
        .map(|c| d.quote_ident(column_name(c)))
        .collect();
    rows.iter()
        .map(|row| {
            let values: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| literal(d, c, row_value(row, i)))
                .collect();
            format!(
                "INSERT INTO {t} ({}) VALUES ({})",
                names.join(", "),
                values.join(", ")
            )
        })
        .collect()
}

/// One `UPDATE` per row setting every non-key column, identified by
/// `key_cols` (indices into `columns`; all comparable columns when empty).
pub fn rows_update_sql(
    d: &dyn Dialect,
    table: &ObjectRef,
    columns: &[ColumnMeta],
    key_cols: &[usize],
    rows: &[Row],
) -> Vec<String> {
    let t = d.qualified_ref(table);
    let wheres = where_columns(columns, key_cols);
    let mut set_idx: Vec<usize> = (0..columns.len())
        .filter(|i| !key_cols.contains(i))
        .collect();
    if set_idx.is_empty() {
        set_idx = (0..columns.len()).collect();
    }
    rows.iter()
        .map(|row| {
            let sets: Vec<String> = set_idx
                .iter()
                .map(|&i| {
                    format!(
                        "{} = {}",
                        d.quote_ident(column_name(&columns[i])),
                        literal(d, &columns[i], row_value(row, i))
                    )
                })
                .collect();
            let mut s = format!("UPDATE {t} SET {}", sets.join(", "));
            let cond = condition(d, columns, &wheres, row);
            if !cond.is_empty() {
                s.push_str(" WHERE ");
                s.push_str(&cond);
            }
            s
        })
        .collect()
}

/// One `DELETE` per row identified by `key_cols` (all comparable columns when empty).
pub fn rows_delete_sql(
    d: &dyn Dialect,
    table: &ObjectRef,
    columns: &[ColumnMeta],
    key_cols: &[usize],
    rows: &[Row],
) -> Vec<String> {
    let t = d.qualified_ref(table);
    let wheres = where_columns(columns, key_cols);
    rows.iter()
        .map(|row| {
            let cond = condition(d, columns, &wheres, row);
            if cond.is_empty() {
                format!("DELETE FROM {t}")
            } else {
                format!("DELETE FROM {t} WHERE {cond}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDialect;
    use dbiji_core::ddl::{ColumnDef, KeyDef};
    use dbiji_core::value::ColumnSource;

    fn users() -> TableDef {
        let mut t = TableDef::new(Some("app".into()), "users");
        t.columns
            .push(ColumnDef::new("id", "integer").not_null().auto_increment());
        t.columns.push(ColumnDef::new("name", "text"));
        t.columns.push(ColumnDef::new("Full Name", "text"));
        t.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t
    }

    #[test]
    fn table_templates_pg() {
        let d = TestDialect::pg();
        let o = GenerateOptions::default();
        assert_eq!(
            select_sql(&d, &users(), &o),
            "SELECT id, name, \"Full Name\"\nFROM app.users"
        );
        assert_eq!(
            select_sql(
                &d,
                &users(),
                &GenerateOptions {
                    select_star: true,
                    compact: true,
                    fully_qualified: false,
                    ..o.clone()
                }
            ),
            "SELECT * FROM users"
        );
        assert_eq!(
            insert_sql(&d, &users(), &o),
            "INSERT INTO app.users (id, name, \"Full Name\")\nVALUES ($1, $2, $3)"
        );
        assert_eq!(
            insert_sql(
                &d,
                &users(),
                &GenerateOptions {
                    skip_auto_columns: true,
                    named_parameters: true,
                    ..o.clone()
                }
            ),
            "INSERT INTO app.users (name, \"Full Name\")\nVALUES (:name, :Full_Name)"
        );
        assert_eq!(
            update_sql(&d, &users(), &o),
            "UPDATE app.users\nSET name = $1, \"Full Name\" = $2\nWHERE id = $3"
        );
        assert_eq!(
            delete_sql(&d, &users(), &o),
            "DELETE FROM app.users\nWHERE id = $1"
        );
        assert_eq!(
            upsert_sql(&d, &users(), &o).unwrap(),
            "INSERT INTO app.users (id, name, \"Full Name\") VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, \"Full Name\" = EXCLUDED.\"Full Name\""
        );
        let ddl = ddl_sql(&d, &users());
        assert!(ddl.starts_with("CREATE TABLE app.users ("), "{ddl}");
        assert!(ddl.ends_with(';'));
    }

    #[test]
    fn table_templates_mysql_and_keyless() {
        let d = TestDialect::mysql();
        let o = GenerateOptions {
            compact: true,
            ..Default::default()
        };
        assert_eq!(
            insert_sql(&d, &users(), &o),
            "INSERT INTO app.users (id, name, `Full Name`) VALUES (?, ?, ?)"
        );
        let mut t = users();
        t.primary_key = None;
        assert_eq!(
            delete_sql(&d, &t, &o),
            "DELETE FROM app.users WHERE id = ? AND name = ? AND `Full Name` = ?"
        );
        assert!(upsert_sql(&d, &t, &o).is_none());
        assert!(upsert_sql(&TestDialect::sqlite(), &users(), &o).is_none());
        let empty = TableDef::new(None, "e");
        assert_eq!(select_sql(&d, &empty, &o), "SELECT * FROM e");
    }

    fn grid() -> (ObjectRef, Vec<ColumnMeta>, Vec<Row>) {
        let t = ObjectRef::table(Some("app".into()), "users");
        let cols = vec![
            ColumnMeta::new("id", "int4", DataKind::Int),
            ColumnMeta::new("label", "text", DataKind::Text).with_source(ColumnSource {
                database: None,
                schema: None,
                table: "users".into(),
                column: "name".into(),
            }),
            ColumnMeta::new("doc", "json", DataKind::Json),
        ];
        let rows = vec![
            vec![Value::Int(1), Value::text("it's"), Value::json("{}")],
            vec![Value::Int(2), Value::Null],
        ];
        (t, cols, rows)
    }

    #[test]
    fn row_statements() {
        let d = TestDialect::pg();
        let (t, cols, rows) = grid();
        assert_eq!(
            rows_insert_sql(&d, &t, &cols, &rows),
            vec![
                "INSERT INTO app.users (id, name, doc) VALUES (1, 'it''s', '{}')".to_string(),
                "INSERT INTO app.users (id, name, doc) VALUES (2, NULL, NULL)".to_string(),
            ]
        );
        assert_eq!(
            rows_update_sql(&d, &t, &cols, &[0], &rows),
            vec![
                "UPDATE app.users SET name = 'it''s', doc = '{}' WHERE id = 1".to_string(),
                "UPDATE app.users SET name = NULL, doc = NULL WHERE id = 2".to_string(),
            ]
        );
        assert_eq!(
            rows_delete_sql(&d, &t, &cols, &[0], &rows),
            vec![
                "DELETE FROM app.users WHERE id = 1",
                "DELETE FROM app.users WHERE id = 2"
            ]
        );
        // Without a key every comparable column identifies the row (json is skipped).
        assert_eq!(
            rows_delete_sql(&d, &t, &cols, &[], &rows[1..]),
            vec!["DELETE FROM app.users WHERE id = 2 AND name IS NULL"]
        );
        // Out-of-range key indices are ignored.
        assert_eq!(rows_delete_sql(&d, &t, &cols, &[9], &rows[..1]).len(), 1);
        assert!(rows_insert_sql(&d, &t, &cols, &[]).is_empty());
    }
}
