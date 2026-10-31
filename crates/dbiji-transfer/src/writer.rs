//! Writing rows into a database table, shared by import, migration and mock
//! data: multi-row `INSERT` statements with placeholders, upserts, transaction
//! scopes and isolation of failing rows.

use crate::progress::Progress;
use dbiji_core::driver::{Session, TransactionMode, execute_update};
use dbiji_core::{DbError, DbKind, DbResult, Dialect, ErrorKind, ObjectRef, Row, Value};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Statement parameters per `INSERT` are capped below the smallest server
/// limit (SQL Server: 2100).
const MAX_PARAMS: usize = 2000;
/// Row constructors per `INSERT ... VALUES` (SQL Server's limit is 1000).
const MAX_ROWS_PER_INSERT: usize = 1000;
/// Row errors kept in summaries with [`OnError::LogAndContinue`].
const MAX_LOGGED_ERRORS: usize = 10_000;
/// Row errors kept in summaries with [`OnError::SkipRow`].
const MAX_SKIPPED_ERRORS: usize = 100;

/// Transaction boundaries of a write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransactionScope {
    /// Commit after every batch.
    #[default]
    PerBatch,
    /// One transaction for everything; rolled back on failure or cancellation.
    Whole,
    /// Auto-commit every statement.
    None,
}

/// What happens when a row cannot be converted or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Stop at the first failing row (the current transaction is rolled back).
    #[default]
    Abort,
    /// Skip failing rows; the summary counts them and keeps the first 100 errors.
    SkipRow,
    /// Skip failing rows and log every error to the progress messages
    /// (up to 10,000 are kept in the summary).
    LogAndContinue,
}

/// A row that could not be written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RowError {
    /// 1-based number of the data row in the source.
    pub row: u64,
    /// Column the error refers to, when known.
    pub column: Option<String>,
    /// Error message.
    pub message: String,
}

/// Table and columns rows are written to.
pub(crate) struct TableTarget {
    /// Qualified and quoted table name.
    pub table_sql: String,
    /// Quoted column names, in the order of the row values.
    pub columns: Vec<String>,
    /// Their database types, where known (empty text: unknown).
    pub types: Vec<String>,
    /// Positions of the key columns for insert-or-replace; empty for plain inserts.
    pub key_index: Vec<usize>,
}

#[derive(Clone, Copy)]
enum SavepointOp {
    Create,
    Release,
    Rollback,
}

struct Savepoints {
    create: &'static str,
    release: Option<&'static str>,
    rollback: &'static str,
}

fn savepoint_sql(kind: DbKind) -> Option<Savepoints> {
    match kind {
        DbKind::ClickHouse | DbKind::Cassandra | DbKind::Redis | DbKind::MongoDb | DbKind::Elasticsearch | DbKind::Neo4j => None,
        DbKind::SqlServer => Some(Savepoints { create: "SAVE TRANSACTION", release: None, rollback: "ROLLBACK TRANSACTION" }),
        DbKind::Oracle => Some(Savepoints { create: "SAVEPOINT", release: None, rollback: "ROLLBACK TO SAVEPOINT" }),
        _ => Some(Savepoints { create: "SAVEPOINT", release: Some("RELEASE SAVEPOINT"), rollback: "ROLLBACK TO SAVEPOINT" }),
    }
}

/// Errors after which nothing more can be written.
fn is_fatal(e: &DbError) -> bool {
    matches!(e.kind, ErrorKind::Connection | ErrorKind::Cancelled)
}

fn with_rows(e: DbError, first: u64, last: u64) -> DbError {
    let at = if first == last { format!("Row {first}") } else { format!("Rows {first}-{last}") };
    let context = match &e.context {
        Some(c) => format!("{at}: {c}"),
        None => at,
    };
    e.with_context(context)
}

pub(crate) struct BatchWriter<'a> {
    session: &'a mut dyn Session,
    dialect: Arc<dyn Dialect>,
    target: TableTarget,
    scope: TransactionScope,
    on_error: OnError,
    rows_per_statement: usize,
    savepoints: Option<Savepoints>,
    original_mode: Option<TransactionMode>,
    /// SQL Server: `SET IDENTITY_INSERT … OFF` once done (explicit values went into an identity column).
    identity_off: Option<String>,
    upsert_sql: Option<String>,
    insert_cache: Option<(usize, String)>,
    /// PostgreSQL: a GENERATED ALWAYS identity column is written (`OVERRIDING SYSTEM VALUE`).
    overriding: bool,
    /// Columns written as literals instead of parameters ([`Dialect::inline_value`]:
    /// MySQL BIT and geometry, SQL Server datetime).
    inline: Vec<bool>,
    item: String,
    pub(crate) written: u64,
    pub(crate) failed: u64,
    pub(crate) errors: Vec<RowError>,
    pub(crate) errors_truncated: bool,
    pub(crate) statements: u64,
    pub(crate) warnings: Vec<String>,
}

impl<'a> BatchWriter<'a> {
    pub(crate) fn new(session: &'a mut dyn Session, target: TableTarget, scope: TransactionScope, on_error: OnError, rows_per_statement: usize, item: impl Into<String>) -> Self {
        let dialect = session.dialect();
        let ncols = target.columns.len().max(1);
        let mut rows_per_statement = rows_per_statement.clamp(1, MAX_ROWS_PER_INSERT).min((MAX_PARAMS / ncols).max(1));
        // Oracle has `INSERT ... VALUES (..), (..)` from 23ai on.
        if dialect.kind() == DbKind::Oracle && session.server_info().version_number.0 < 23 {
            rows_per_statement = 1;
        }
        let upsert_sql = if target.key_index.is_empty() {
            None
        } else {
            let keys: Vec<String> = target.key_index.iter().filter_map(|&i| target.columns.get(i).cloned()).collect();
            let placeholders: Vec<String> = (1..=target.columns.len()).map(|i| dialect.placeholder(i)).collect();
            dialect.upsert(&target.table_sql, &target.columns, &keys, &placeholders)
        };
        let savepoints = savepoint_sql(dialect.kind());
        let inline: Vec<bool> = (0..target.columns.len()).map(|i| target.types.get(i).is_some_and(|t| !t.trim().is_empty() && dialect.inline_value(t))).collect();
        let mut warnings = Vec::new();
        let mut scope = scope;
        // ClickHouse has no transactions: rows are committed as they are written.
        if dialect.kind() == DbKind::ClickHouse {
            scope = TransactionScope::None;
        } else if on_error != OnError::Abort && savepoints.is_none() && scope != TransactionScope::None {
            warnings.push(format!("{} has no savepoints: rows are committed statement by statement so failing rows can be skipped", dialect.kind().display_name()));
            scope = TransactionScope::None;
        }
        BatchWriter {
            session,
            dialect,
            target,
            scope,
            on_error,
            rows_per_statement,
            savepoints,
            original_mode: None,
            identity_off: None,
            upsert_sql,
            insert_cache: None,
            overriding: false,
            inline,
            item: item.into(),
            written: 0,
            failed: 0,
            errors: Vec::new(),
            errors_truncated: false,
            statements: 0,
            warnings,
        }
    }

    /// Switch the session into the transaction mode of the scope.
    pub(crate) async fn begin(&mut self) -> DbResult<()> {
        let mode = self.session.transaction_mode();
        if mode == TransactionMode::Manual && self.session.in_transaction() {
            return Err(DbError::config("The session has an open transaction; commit or roll it back before writing data"));
        }
        // PostgreSQL refuses explicit values for a GENERATED ALWAYS identity
        // column (copied ids, imported keys) unless the INSERT overrides it.
        if self.dialect.kind() == DbKind::Postgres {
            let table = self.dialect.string_literal(&self.target.table_sql);
            let names: Vec<String> = self.target.columns.iter().map(|c| self.dialect.string_literal(&unquote_tsql(c))).collect();
            let sql =
                format!("SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass({table}) AND attidentity = 'a' AND NOT attisdropped AND attname IN ({})", names.join(", "));
            if self.query_i64(&sql).await?.unwrap_or(0) > 0 {
                self.overriding = true;
                let head = format!("INSERT INTO {} ({})", self.target.table_sql, self.target.columns.join(", "));
                if let Some(u) = self.upsert_sql.as_mut().filter(|u| u.starts_with(&head)) {
                    u.insert_str(head.len(), " OVERRIDING SYSTEM VALUE");
                }
            }
        }
        self.original_mode = Some(mode);
        let wanted = match self.scope {
            TransactionScope::None => TransactionMode::Auto,
            _ => TransactionMode::Manual,
        };
        if mode != wanted {
            self.session.set_transaction_mode(wanted).await?;
        }
        // SQL Server refuses explicit values for an identity column (copied ids,
        // imported keys) unless IDENTITY_INSERT is on for the table.
        if self.dialect.kind() == DbKind::SqlServer {
            let table = self.dialect.string_literal(&self.target.table_sql);
            let names: Vec<String> = self.target.columns.iter().map(|c| self.dialect.string_literal(&unquote_tsql(c))).collect();
            let when = format!("IF EXISTS (SELECT 1 FROM sys.identity_columns WHERE object_id = OBJECT_ID({table}) AND name IN ({}))", names.join(", "));
            let t = self.target.table_sql.clone();
            if let Err(e) = self.exec(&format!("{when} SET IDENTITY_INSERT {t} ON"), &[]).await {
                // The session is left as it was found.
                self.abort().await;
                return Err(e);
            }
            self.identity_off = Some(format!("{when} SET IDENTITY_INSERT {t} OFF"));
        }
        Ok(())
    }

    fn transactional(&self) -> bool {
        self.scope != TransactionScope::None
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> DbResult<Option<u64>> {
        self.statements += 1;
        execute_update(&mut *self.session, sql, params).await
    }

    /// Execute a statement through the writer's session.
    pub(crate) async fn execute(&mut self, sql: &str) -> DbResult<()> {
        self.exec(sql, &[]).await.map(|_| ())
    }

    /// The first value of `sql` as an integer, through the writer's session.
    pub(crate) async fn query_i64(&mut self, sql: &str) -> DbResult<Option<i64>> {
        let (_, rows) = dbiji_core::driver::query_all(&mut *self.session, sql, &[]).await?;
        Ok(rows.first().and_then(|r| r.first()).and_then(|v| v.as_i64()))
    }

    /// Remove all rows of `table`: `TRUNCATE` (falling back to `DELETE`), or
    /// `DELETE` inside the transaction for [`TransactionScope::Whole`].
    pub(crate) async fn clear_table(&mut self, table: &ObjectRef) -> DbResult<()> {
        let delete = format!("DELETE FROM {}", self.dialect.qualified_ref(table));
        if self.scope == TransactionScope::Whole {
            return self.execute(&delete).await;
        }
        let truncate = self.dialect.truncate_table(table);
        match self.exec(&truncate, &[]).await {
            Ok(_) => {}
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => {
                tracing::debug!("TRUNCATE failed ({e}), using DELETE");
                if self.transactional() {
                    let _ = self.session.rollback().await;
                }
                self.execute(&delete).await?;
            }
        }
        if self.scope == TransactionScope::PerBatch {
            self.session.commit().await?;
        }
        Ok(())
    }

    /// Record a failed row.
    pub(crate) fn record(&mut self, row: u64, column: Option<String>, message: String, progress: &Progress) {
        self.failed += 1;
        progress.add_errors(1);
        let keep = match self.on_error {
            OnError::LogAndContinue => {
                let at = match &column {
                    Some(c) => format!("Row {row}, column {c}"),
                    None => format!("Row {row}"),
                };
                progress.error(Some(&self.item), format!("{at}: {message}"));
                MAX_LOGGED_ERRORS
            }
            _ => MAX_SKIPPED_ERRORS,
        };
        if self.errors.len() < keep {
            self.errors.push(RowError { row, column, message });
        } else {
            self.errors_truncated = true;
        }
    }

    fn insert_sql(&mut self, nrows: usize) -> String {
        if let Some((n, sql)) = &self.insert_cache
            && *n == nrows
        {
            return sql.clone();
        }
        let ncols = self.target.columns.len();
        let overriding = if self.overriding { " OVERRIDING SYSTEM VALUE" } else { "" };
        let mut sql = format!("INSERT INTO {} ({}){overriding} VALUES ", self.target.table_sql, self.target.columns.join(", "));
        let mut p = 1;
        for r in 0..nrows {
            if r > 0 {
                sql.push_str(", ");
            }
            sql.push('(');
            for c in 0..ncols {
                if c > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&self.dialect.placeholder(p));
                p += 1;
            }
            sql.push(')');
        }
        self.insert_cache = Some((nrows, sql.clone()));
        sql
    }

    /// Write rows (values in target column order) with their source row numbers.
    pub(crate) async fn write(&mut self, rows: Vec<(u64, Row)>, progress: &Progress) -> DbResult<()> {
        if self.upsert_sql.is_some() {
            for (no, row) in rows {
                self.write_one(no, row, progress).await?;
            }
            return Ok(());
        }
        let mut iter = rows.into_iter().peekable();
        while iter.peek().is_some() {
            let chunk: Vec<(u64, Row)> = iter.by_ref().take(self.rows_per_statement).collect();
            self.write_chunk(chunk, progress).await?;
        }
        Ok(())
    }

    async fn savepoint(&mut self, op: SavepointOp, name: &str) -> DbResult<()> {
        let sql = match (&self.savepoints, op) {
            (Some(s), SavepointOp::Create) => format!("{} {name}", s.create),
            (Some(s), SavepointOp::Release) => match s.release {
                Some(r) => format!("{r} {name}"),
                None => return Ok(()),
            },
            (Some(s), SavepointOp::Rollback) => format!("{} {name}", s.rollback),
            (None, _) => return Ok(()),
        };
        self.execute(&sql).await
    }

    fn protect(&self) -> bool {
        self.on_error != OnError::Abort && self.transactional() && self.savepoints.is_some()
    }

    async fn write_chunk(&mut self, chunk: Vec<(u64, Row)>, progress: &Progress) -> DbResult<()> {
        let (Some(first), Some(last)) = (chunk.first().map(|r| r.0), chunk.last().map(|r| r.0)) else {
            return Ok(());
        };
        let protect = self.protect();
        if protect {
            self.savepoint(SavepointOp::Create, "dbiji_batch").await?;
        }
        // Replace without upsert keeps one row per key: count what is written, not what was read.
        let chunk = if self.upsert_sql.is_none() && !self.target.key_index.is_empty() { dedupe_by_key(chunk, &self.target.key_index) } else { chunk };
        let n = chunk.len() as u64;
        match self.exec_rows(chunk).await {
            Ok(()) => {
                if protect {
                    self.savepoint(SavepointOp::Release, "dbiji_batch").await?;
                }
                self.written += n;
                Ok(())
            }
            Err((e, _)) if self.on_error == OnError::Abort || is_fatal(&e) => Err(with_rows(e, first, last)),
            Err((e, rows)) => {
                if protect {
                    self.savepoint(SavepointOp::Rollback, "dbiji_batch").await?;
                }
                if rows.len() == 1 {
                    self.record(first, None, e.full_text(), progress);
                } else {
                    // Find the bad rows one by one.
                    for (no, row) in rows {
                        self.write_one(no, row, progress).await?;
                    }
                }
                if protect {
                    self.savepoint(SavepointOp::Release, "dbiji_batch").await?;
                }
                Ok(())
            }
        }
    }

    async fn write_one(&mut self, no: u64, row: Row, progress: &Progress) -> DbResult<()> {
        let protect = self.protect();
        if protect {
            self.savepoint(SavepointOp::Create, "dbiji_row").await?;
        }
        match self.exec_rows(vec![(no, row)]).await {
            Ok(()) => {
                if protect {
                    self.savepoint(SavepointOp::Release, "dbiji_row").await?;
                }
                self.written += 1;
                Ok(())
            }
            Err((e, _)) if self.on_error == OnError::Abort || is_fatal(&e) => Err(with_rows(e, no, no)),
            Err((e, _)) => {
                if protect {
                    self.savepoint(SavepointOp::Rollback, "dbiji_row").await?;
                }
                self.record(no, None, e.full_text(), progress);
                Ok(())
            }
        }
    }

    /// Execute the statements for `rows`; on failure the rows are handed back.
    async fn exec_rows(&mut self, rows: Vec<(u64, Row)>) -> Result<(), (DbError, Vec<(u64, Row)>)> {
        let ncols = self.target.columns.len();
        let rows: Vec<(u64, Row)> = rows
            .into_iter()
            .map(|(n, mut r)| {
                r.resize(ncols, Value::Null);
                (n, r)
            })
            .collect();
        if let Some(upsert) = self.upsert_sql.clone() {
            let mut failure = None;
            for (i, (_, row)) in rows.iter().enumerate() {
                let result = if self.inline.contains(&true) {
                    let mut params = Vec::new();
                    let values = self.row_values(row, &mut params);
                    let keys: Vec<String> = self.target.key_index.iter().filter_map(|&k| self.target.columns.get(k).cloned()).collect();
                    match self.dialect.upsert(&self.target.table_sql, &self.target.columns, &keys, &values) {
                        Some(sql) => self.exec(&sql, &params).await,
                        None => self.exec(&upsert, row).await,
                    }
                } else {
                    self.exec(&upsert, row).await
                };
                if let Err(e) = result {
                    failure = Some((i, e));
                    break;
                }
            }
            return match failure {
                Some((i, e)) => Err((e, rows.into_iter().skip(i).collect())),
                None => Ok(()),
            };
        }
        if !self.target.key_index.is_empty() {
            // Replace without native upsert: delete existing keys, then insert.
            let rows = dedupe_by_key(rows, &self.target.key_index);
            let mut conds = Vec::new();
            let mut params = Vec::new();
            let mut p = 1;
            for (_, row) in &rows {
                if self.target.key_index.iter().any(|&k| row.get(k).is_none_or(Value::is_null)) {
                    continue;
                }
                let parts: Vec<String> = self
                    .target
                    .key_index
                    .iter()
                    .map(|&k| {
                        let s = format!("{} = {}", self.target.columns[k], self.dialect.placeholder(p));
                        p += 1;
                        s
                    })
                    .collect();
                conds.push(format!("({})", parts.join(" AND ")));
                params.extend(self.target.key_index.iter().map(|&k| row[k].clone()));
            }
            if !conds.is_empty() {
                let sql = format!("DELETE FROM {} WHERE {}", self.target.table_sql, conds.join(" OR "));
                if let Err(e) = self.exec(&sql, &params).await {
                    return Err((e, rows));
                }
            }
            return self.insert_rows(rows, ncols).await;
        }
        self.insert_rows(rows, ncols).await
    }

    /// Placeholders (their values pushed to `params`) or literals of one row.
    fn row_values(&self, row: &[Value], params: &mut Vec<Value>) -> Vec<String> {
        row.iter()
            .enumerate()
            .map(|(i, v)| {
                let ty = self.target.types.get(i).map(String::as_str).filter(|t| !t.trim().is_empty());
                if self.inline.get(i).copied().unwrap_or(false) && !v.is_null() {
                    self.dialect.literal(v, ty)
                } else {
                    params.push(v.clone());
                    self.dialect.placeholder(params.len())
                }
            })
            .collect()
    }

    async fn insert_rows(&mut self, rows: Vec<(u64, Row)>, ncols: usize) -> Result<(), (DbError, Vec<(u64, Row)>)> {
        if self.inline.contains(&true) {
            let overriding = if self.overriding { " OVERRIDING SYSTEM VALUE" } else { "" };
            let mut params = Vec::new();
            let tuples: Vec<String> = rows.iter().map(|(_, row)| format!("({})", self.row_values(row, &mut params).join(", "))).collect();
            let sql = format!("INSERT INTO {} ({}){overriding} VALUES {}", self.target.table_sql, self.target.columns.join(", "), tuples.join(", "));
            return match self.exec(&sql, &params).await {
                Ok(_) => Ok(()),
                Err(e) => Err((e, rows)),
            };
        }
        let numbers: Vec<u64> = rows.iter().map(|r| r.0).collect();
        let sql = self.insert_sql(rows.len());
        let mut params: Vec<Value> = Vec::with_capacity(rows.len() * ncols);
        for (_, mut row) in rows {
            row.resize(ncols, Value::Null);
            params.extend(row);
        }
        match self.exec(&sql, &params).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let mut back = Vec::with_capacity(numbers.len());
                let mut values = params.into_iter();
                for no in numbers {
                    back.push((no, values.by_ref().take(ncols).collect()));
                }
                Err((e, back))
            }
        }
    }

    /// Commit the current batch ([`TransactionScope::PerBatch`]).
    pub(crate) async fn end_batch(&mut self) -> DbResult<()> {
        if self.scope == TransactionScope::PerBatch {
            self.session.commit().await?;
        }
        Ok(())
    }

    /// Commit and restore the original transaction mode.
    pub(crate) async fn finish(&mut self) -> DbResult<()> {
        if self.transactional()
            && let Err(e) = self.session.commit().await
        {
            self.abort().await;
            return Err(e);
        }
        self.identity_insert_off().await;
        self.restore_mode().await
    }

    async fn identity_insert_off(&mut self) {
        if let Some(sql) = self.identity_off.take()
            && let Err(e) = execute_update(&mut *self.session, &sql, &[]).await
        {
            tracing::debug!("SET IDENTITY_INSERT OFF failed: {e}");
        }
    }

    /// Roll back what is not committed yet and restore the transaction mode.
    pub(crate) async fn abort(&mut self) {
        if self.transactional()
            && let Err(e) = self.session.rollback().await
        {
            tracing::debug!("rollback after failed write: {e}");
        }
        self.identity_insert_off().await;
        if let Err(e) = self.restore_mode().await {
            tracing::debug!("restoring the transaction mode failed: {e}");
        }
    }

    async fn restore_mode(&mut self) -> DbResult<()> {
        if let Some(orig) = self.original_mode.take()
            && self.session.transaction_mode() != orig
        {
            self.session.set_transaction_mode(orig).await?;
        }
        Ok(())
    }
}

/// Keep the last row of every key (later rows replace earlier ones).
/// `[a]]b]` → `a]b`: a quoted T-SQL name as stored in the catalog.
fn unquote_tsql(name: &str) -> String {
    let n = name.trim();
    if let Some(inner) = n.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        inner.replace("]]", "]")
    } else if let Some(inner) = n.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        inner.replace("\"\"", "\"")
    } else {
        n.to_string()
    }
}

fn dedupe_by_key(rows: Vec<(u64, Row)>, keys: &[usize]) -> Vec<(u64, Row)> {
    if rows.len() < 2 {
        return rows;
    }
    let key_of =
        |row: &Row| -> String { keys.iter().map(|&k| row.get(k).map(|v| format!("{}:{}", v.variant_name(), v.to_text())).unwrap_or_default()).collect::<Vec<_>>().join("\u{1}") };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(u64, Row)> = Vec::with_capacity(rows.len());
    for (no, row) in rows.into_iter().rev() {
        if seen.insert(key_of(&row)) {
            out.push((no, row));
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keeps_last() {
        let rows = vec![(1, vec![Value::Int(1), Value::text("a")]), (2, vec![Value::Int(2), Value::text("b")]), (3, vec![Value::Int(1), Value::text("c")])];
        let out = dedupe_by_key(rows, &[0]);
        assert_eq!(out.iter().map(|r| r.0).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn row_context() {
        let e = with_rows(DbError::query("bad"), 3, 7);
        assert_eq!(e.context.as_deref(), Some("Rows 3-7"));
        let e = with_rows(DbError::query("bad"), 4, 4);
        assert_eq!(e.context.as_deref(), Some("Row 4"));
    }
}
