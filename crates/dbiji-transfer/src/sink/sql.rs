//! SQL `INSERT` / upsert script export.

use super::{ExportSummary, RowSink, TextOutput, cell, source_table_name};
use crate::options::{ExportOptions, IdentifierQuoting, SqlInsertMode, SqlOptions};
use dbiji_core::ddl::{ColumnDef, KeyDef, TableDef};
use dbiji_core::{ColumnMeta, DataKind, DbKind, DbResult, Dialect, ObjectRef, Row, Value};
use std::sync::Arc;

pub(crate) struct SqlSink {
    out: TextOutput,
    opts: SqlOptions,
    dialect: Arc<dyn Dialect>,
    table_def: Option<TableDef>,
    columns: Vec<ColumnMeta>,
    /// Type names passed to `Dialect::literal` (only when source and target match).
    hints: Vec<Option<String>>,
    table_sql: String,
    quoted_columns: Vec<String>,
    insert_prefix: String,
    /// Upsert: positions and quoted names of the key columns.
    key_index: Vec<usize>,
    quoted_keys: Vec<String>,
    upsert_native: bool,
    upsert: bool,
    stmt: String,
    pending_rows: usize,
    rows: u64,
    warnings: Vec<String>,
    literals: Vec<String>,
}

impl SqlSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions, dialect: Arc<dyn Dialect>) -> Self {
        SqlSink {
            out,
            opts: options.sql.clone(),
            dialect,
            table_def: None,
            columns: Vec::new(),
            hints: Vec::new(),
            table_sql: String::new(),
            quoted_columns: Vec::new(),
            insert_prefix: String::new(),
            key_index: Vec::new(),
            quoted_keys: Vec::new(),
            upsert_native: false,
            upsert: false,
            stmt: String::new(),
            pending_rows: 0,
            rows: 0,
            warnings: Vec::new(),
            literals: Vec::new(),
        }
    }

    fn quote(&self, ident: &str) -> String {
        match self.opts.quoting {
            IdentifierQuoting::Always => self.dialect.force_quote_ident(ident),
            IdentifierQuoting::AsNeeded => self.dialect.quote_ident(ident),
        }
    }

    fn same_product(&self) -> bool {
        self.opts.source_kind.is_some_and(|k| k == self.dialect.kind() && k != DbKind::Generic)
    }

    fn terminator(&self) -> &str {
        &self.opts.terminator
    }

    fn render_literals(&mut self, row: &Row) {
        self.literals.clear();
        for i in 0..self.columns.len() {
            let v = cell(row, i);
            let lit = self.dialect.literal(v, self.hints[i].as_deref());
            self.literals.push(lit);
        }
    }

    fn flush_insert(&mut self) -> DbResult<()> {
        if self.pending_rows == 0 {
            return Ok(());
        }
        self.stmt.push_str(&self.opts.terminator);
        self.stmt.push('\n');
        let res = self.out.write_str(&self.stmt);
        self.stmt.clear();
        self.pending_rows = 0;
        res
    }

    /// `CREATE TABLE` for the export target.
    fn create_table_sql(&self, schema: Option<String>, name: &str) -> Vec<String> {
        let mut def = match &self.table_def {
            Some(d) if self.same_product() => d.clone(),
            Some(d) => {
                let mut t = d.clone();
                let source = self.opts.source_kind.unwrap_or(DbKind::Generic);
                for c in &mut t.columns {
                    c.data_type = self.dialect.ddl().map_type(source, &c.data_type, c.kind);
                    // Defaults and generated expressions are written in the source dialect.
                    c.default = None;
                    c.generated = None;
                }
                t.checks.clear();
                t.triggers.clear();
                t.indexes.retain(|i| i.predicate.is_none() && i.columns.iter().all(|c| !c.is_expression));
                t
            }
            None => {
                let mut t = TableDef::default();
                for (i, c) in self.columns.iter().enumerate() {
                    let data_type = column_type(c, self.dialect.as_ref(), self.opts.source_kind, self.same_product());
                    t.columns.push(ColumnDef {
                        name: c.name.clone(),
                        data_type,
                        kind: c.kind,
                        nullable: c.nullable.unwrap_or(true),
                        ordinal: i as u32 + 1,
                        ..Default::default()
                    });
                }
                t
            }
        };
        def.schema = schema;
        def.name = name.to_string();
        if def.primary_key.is_none() && !self.opts.key_columns.is_empty() {
            def.primary_key = Some(KeyDef { name: None, columns: self.opts.key_columns.clone() });
        }
        // Foreign keys may reference tables that are not part of the script.
        def.foreign_keys.clear();
        self.dialect.ddl().create_table(&def, false)
    }
}

/// Column type for `CREATE TABLE` from result set metadata.
pub(crate) fn column_type(c: &ColumnMeta, dialect: &dyn Dialect, source: Option<DbKind>, same_product: bool) -> String {
    if same_product && !c.type_name.is_empty() {
        return c.type_name.clone();
    }
    let mut ty = c.type_name.clone();
    if !ty.is_empty() && !ty.contains('(') {
        match c.kind {
            DataKind::Text if c.max_length.is_some_and(|l| l > 0) => {
                ty = format!("{ty}({})", c.max_length.unwrap_or_default());
            }
            DataKind::Decimal if c.precision.is_some_and(|p| p > 0) => {
                ty = match c.scale {
                    Some(s) => format!("{ty}({},{s})", c.precision.unwrap_or_default()),
                    None => format!("{ty}({})", c.precision.unwrap_or_default()),
                };
            }
            _ => {}
        }
    }
    if ty.is_empty() {
        return dialect.ddl().default_type_for(c.kind);
    }
    dialect.ddl().map_type(source.unwrap_or(DbKind::Generic), &ty, c.kind)
}

impl RowSink for SqlSink {
    fn set_table_definition(&mut self, def: &TableDef) {
        self.table_def = Some(def.clone());
    }

    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        self.columns = columns.to_vec();
        let same = self.same_product();
        self.hints = columns.iter().map(|c| (same && !c.type_name.is_empty()).then(|| c.type_name.clone())).collect();
        let source = source_table_name(columns, table);
        let name = match &self.opts.table_name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => match &source {
                Some((_, n)) => n.clone(),
                None => {
                    self.warnings
                        .push("The result has no single source table; statements target 'query_result'".into());
                    "query_result".to_string()
                }
            },
        };
        let schema = match &self.opts.schema_name {
            Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ if self.opts.qualify_with_schema => source.and_then(|(s, _)| s),
            _ => None,
        };
        self.table_sql = match &schema {
            Some(s) => format!("{}.{}", self.quote(s), self.quote(&name)),
            None => self.quote(&name),
        };
        self.quoted_columns = columns.iter().map(|c| self.quote(&c.name)).collect();
        self.insert_prefix = format!("INSERT INTO {} ({}) VALUES", self.table_sql, self.quoted_columns.join(", "));

        if self.opts.include_create_table {
            let mut ddl = String::new();
            for stmt in self.create_table_sql(schema.clone(), &name) {
                ddl.push_str(&stmt);
                ddl.push_str(self.terminator());
                ddl.push('\n');
            }
            ddl.push('\n');
            self.out.write_str(&ddl)?;
        }

        if self.opts.mode == SqlInsertMode::Upsert {
            let mut keys: Vec<String> = self.opts.key_columns.clone();
            if keys.is_empty()
                && let Some(k) = self.table_def.as_ref().and_then(|d| d.row_key())
            {
                keys = k.columns.clone();
            }
            let index: Option<Vec<usize>> = keys.iter().map(|k| columns.iter().position(|c| &c.name == k)).collect();
            match index {
                Some(idx) if !idx.is_empty() => {
                    self.key_index = idx;
                    self.quoted_keys = keys.iter().map(|k| self.quote(k)).collect();
                    let probe: Vec<String> = (1..=columns.len()).map(|i| self.dialect.placeholder(i)).collect();
                    self.upsert_native =
                        self.dialect.upsert(&self.table_sql, &self.quoted_columns, &self.quoted_keys, &probe).is_some();
                    if !self.upsert_native {
                        self.warnings.push(format!(
                            "{} has no upsert statement; DELETE + INSERT pairs are written instead",
                            self.dialect.kind().display_name()
                        ));
                    }
                    self.upsert = true;
                }
                _ => {
                    self.warnings.push(
                        "Upsert needs key columns present in the result; plain INSERT statements are written".into(),
                    );
                }
            }
        }
        Ok(())
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        self.render_literals(row);
        if self.upsert {
            let mut text = String::new();
            if self.upsert_native {
                if let Some(sql) =
                    self.dialect.upsert(&self.table_sql, &self.quoted_columns, &self.quoted_keys, &self.literals)
                {
                    text.push_str(&sql);
                    text.push_str(&self.opts.terminator);
                    text.push('\n');
                }
            } else {
                let cond: Vec<String> = self
                    .key_index
                    .iter()
                    .zip(&self.quoted_keys)
                    .map(|(&i, k)| match cell(row, i) {
                        Value::Null => format!("{k} IS NULL"),
                        _ => format!("{k} = {}", self.literals[i]),
                    })
                    .collect();
                text.push_str(&format!(
                    "DELETE FROM {} WHERE {}{}\n",
                    self.table_sql,
                    cond.join(" AND "),
                    self.opts.terminator
                ));
                text.push_str(&format!(
                    "{} ({}){}\n",
                    self.insert_prefix,
                    self.literals.join(", "),
                    self.opts.terminator
                ));
            }
            self.out.write_str(&text)?;
        } else {
            let per_stmt = self.opts.rows_per_statement.clamp(1, 1000);
            if self.pending_rows == 0 {
                self.stmt.push_str(&self.insert_prefix);
                self.stmt.push_str(if per_stmt == 1 { " " } else { "\n" });
            } else {
                self.stmt.push_str(",\n");
            }
            self.stmt.push('(');
            for (i, lit) in self.literals.iter().enumerate() {
                if i > 0 {
                    self.stmt.push_str(", ");
                }
                self.stmt.push_str(lit);
            }
            self.stmt.push(')');
            self.pending_rows += 1;
            if self.pending_rows >= per_stmt {
                self.flush_insert()?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        self.flush_insert()?;
        let mut summary = self.out.finish(self.rows)?;
        summary.warnings.splice(0..0, self.warnings.drain(..));
        Ok(summary)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}
