//! Pending change sets and the SQL generated from them.

use dbiji_core::{Dialect, ObjectRef, Value, trf};

/// A pending value for a cell.
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    /// A concrete value (possibly NULL).
    Value(Value),
    /// The column default (`DEFAULT` in SQL; omitted from INSERT column lists).
    Default,
}

impl CellValue {
    /// The value, with `Default` shown as NULL.
    pub fn value_or_null(&self) -> &Value {
        static NULL: Value = Value::Null;
        match self {
            CellValue::Value(v) => v,
            CellValue::Default => &NULL,
        }
    }
}

/// Key column value identifying a row (original, unmodified value).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyValue {
    /// Result column index.
    pub column: usize,
    /// Table column name.
    pub name: String,
    /// Database type name (for typed literals).
    pub type_name: String,
    /// Original value.
    pub value: Value,
}

/// A changed (or inserted) column value.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedValue {
    /// Result column index.
    pub column: usize,
    /// Table column name.
    pub name: String,
    /// Database type name (for typed literals).
    pub type_name: String,
    /// New value.
    pub value: CellValue,
}

/// `UPDATE` of one stored row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowUpdate {
    /// Store row index.
    pub row: u32,
    /// Key identifying the row.
    pub key: Vec<KeyValue>,
    /// Changed columns.
    pub changes: Vec<ChangedValue>,
}

/// `INSERT` of a new row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowInsert {
    /// Id of the pending inserted row.
    pub id: u64,
    /// Explicitly specified columns (columns left at their default are omitted).
    pub values: Vec<ChangedValue>,
}

/// `DELETE` of one stored row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowDelete {
    /// Store row index.
    pub row: u32,
    /// Key identifying the row.
    pub key: Vec<KeyValue>,
}

/// All pending changes of an editable result.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeSet {
    /// Target table.
    pub table: ObjectRef,
    /// Rows to delete.
    pub deletes: Vec<RowDelete>,
    /// Rows to update.
    pub updates: Vec<RowUpdate>,
    /// Rows to insert.
    pub inserts: Vec<RowInsert>,
    /// The table columns of the result (result column, table column name, type):
    /// read back after an insert (generated ids, defaults).
    pub columns: Vec<(usize, String, String)>,
    /// Result columns of the row key.
    pub key_columns: Vec<usize>,
}

impl ChangeSet {
    /// True when there is nothing to save.
    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty() && self.updates.is_empty() && self.inserts.is_empty()
    }

    /// Number of statements [`ChangeSet::statements`] produces.
    pub fn len(&self) -> usize {
        self.deletes.len() + self.updates.len() + self.inserts.len()
    }

    /// Parameterised statements in execution order: deletes, updates, inserts.
    pub fn statements(&self, dialect: &dyn Dialect) -> Vec<(String, Vec<Value>)> {
        let mut out = Vec::with_capacity(self.len());
        let table = dialect.qualified_ref(&self.table);
        let r = Renderer::Params;
        for d in &self.deletes {
            out.push(render_delete(dialect, &table, d, r));
        }
        for u in &self.updates {
            out.push(render_update(dialect, &table, u, r));
        }
        for i in &self.inserts {
            out.push(render_insert(dialect, &table, i, r));
        }
        out
    }

    /// [`statements`](Self::statements) for saving: an insert returns the new row
    /// (`RETURNING`) where the database can, so generated ids and defaults show at
    /// once. Its result columns are [`ChangeSet::columns`] in order.
    pub fn save_statements(&self, dialect: &dyn Dialect) -> Vec<(String, Vec<Value>)> {
        let mut out = self.statements(dialect);
        if dialect.supports_returning() && !self.columns.is_empty() {
            let cols: Vec<String> = self.columns.iter().map(|(_, n, t)| dialect.returning_column(n, t)).collect();
            let returning = format!(" RETURNING {}", cols.join(", "));
            let first_insert = out.len() - self.inserts.len();
            for (sql, _) in &mut out[first_insert..] {
                sql.push_str(&returning);
            }
        }
        out
    }

    /// Index in [`save_statements`](Self::save_statements) of the insert of pending row `id`.
    pub fn insert_statement(&self, id: u64) -> Option<usize> {
        let first_insert = self.deletes.len() + self.updates.len();
        self.inserts.iter().position(|i| i.id == id).map(|p| first_insert + p)
    }

    /// SQL text with literals, for the "preview SQL" dialog.
    pub fn preview_sql(&self, dialect: &dyn Dialect) -> String {
        let table = dialect.qualified_ref(&self.table);
        let r = Renderer::Literals;
        let mut lines = Vec::with_capacity(self.len());
        for d in &self.deletes {
            lines.push(render_delete(dialect, &table, d, r).0);
        }
        for u in &self.updates {
            lines.push(render_update(dialect, &table, u, r).0);
        }
        for i in &self.inserts {
            lines.push(render_insert(dialect, &table, i, r).0);
        }
        let mut s = lines.join(";\n");
        if !s.is_empty() {
            s.push(';');
        }
        s
    }

    /// One-line summary, e.g. "2 updated, 1 inserted, 0 deleted".
    pub fn summary(&self) -> String {
        trf("{} updated, {} inserted, {} deleted", &[&self.updates.len(), &self.inserts.len(), &self.deletes.len()])
    }
}

/// How values are rendered into statements.
#[derive(Clone, Copy)]
enum Renderer {
    Params,
    Literals,
}

struct Builder<'a> {
    dialect: &'a dyn Dialect,
    mode: Renderer,
    params: Vec<Value>,
}

impl Builder<'_> {
    fn value(&mut self, v: &Value, type_name: &str) -> String {
        match self.mode {
            Renderer::Params if !v.is_null() && !type_name.is_empty() && self.dialect.inline_value(type_name) => self.dialect.literal(v, Some(type_name)),
            Renderer::Params => {
                self.params.push(v.clone());
                self.dialect.placeholder(self.params.len())
            }
            Renderer::Literals => self.dialect.literal(v, (!type_name.is_empty()).then_some(type_name)),
        }
    }

    /// `col = value` or `col IS NULL` (NULL-safe key comparison).
    fn key_condition(&mut self, keys: &[KeyValue]) -> String {
        let parts: Vec<String> = keys
            .iter()
            .map(|k| {
                let col = self.dialect.quote_ident(&k.name);
                if k.value.is_null() {
                    format!("{col} IS NULL")
                } else {
                    let v = self.value(&k.value, &k.type_name);
                    format!("{col} = {v}")
                }
            })
            .collect();
        parts.join(" AND ")
    }
}

fn render_delete(dialect: &dyn Dialect, table: &str, d: &RowDelete, mode: Renderer) -> (String, Vec<Value>) {
    let mut b = Builder { dialect, mode, params: Vec::new() };
    let cond = b.key_condition(&d.key);
    (format!("DELETE FROM {table} WHERE {cond}"), b.params)
}

fn render_update(dialect: &dyn Dialect, table: &str, u: &RowUpdate, mode: Renderer) -> (String, Vec<Value>) {
    let mut b = Builder { dialect, mode, params: Vec::new() };
    let sets: Vec<String> = u
        .changes
        .iter()
        .map(|c| {
            let col = dialect.quote_ident(&c.name);
            match &c.value {
                CellValue::Default => format!("{col} = DEFAULT"),
                CellValue::Value(v) => {
                    let p = b.value(v, &c.type_name);
                    format!("{col} = {p}")
                }
            }
        })
        .collect();
    let cond = b.key_condition(&u.key);
    (format!("UPDATE {table} SET {} WHERE {cond}", sets.join(", ")), b.params)
}

fn render_insert(dialect: &dyn Dialect, table: &str, i: &RowInsert, mode: Renderer) -> (String, Vec<Value>) {
    let mut b = Builder { dialect, mode, params: Vec::new() };
    let specified: Vec<&ChangedValue> = i.values.iter().filter(|c| c.value != CellValue::Default).collect();
    if specified.is_empty() {
        let sql = if dialect.kind().is_mysql_family() { format!("INSERT INTO {table} () VALUES ()") } else { format!("INSERT INTO {table} DEFAULT VALUES") };
        return (sql, b.params);
    }
    let cols: Vec<String> = specified.iter().map(|c| dialect.quote_ident(&c.name)).collect();
    let vals: Vec<String> = specified.iter().map(|c| b.value(c.value.value_or_null(), &c.type_name)).collect();
    (format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(", "), vals.join(", ")), b.params)
}
