//! Table migration between databases, possibly of different products (FR-T4).
//!
//! For every table the definition is read from the source session, mapped to
//! the target dialect (`ddl().map_type`), the table is created as configured,
//! rows are streamed with a cursor (`SELECT ... FROM`) and written in batches.
//! Secondary indexes are created after the data of each table, foreign keys
//! after all tables.

use crate::convert::{ConvertOptions, convert_value};
use crate::error::with_item;
use crate::export::{SessionFactory, cancellable};
use crate::progress::Progress;
use crate::source::{CursorSource, RowSource};
use crate::writer::{BatchWriter, OnError, RowError, TableTarget, TransactionScope};
use dbiji_core::ddl::{ForeignKeyDef, IndexDef, KeyDef, TableDef};
use dbiji_core::driver::{Session, execute_update};
use dbiji_core::{DataKind, DbError, DbKind, DbResult, Dialect, ErrorKind, ObjectRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// How target tables are prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MigrationMode {
    /// Add rows to the target table (created when missing and `create_tables` is set).
    #[default]
    Append,
    /// Empty the target table first.
    Truncate,
    /// Insert or replace rows by primary key.
    Replace,
    /// Drop the target table if it exists and create it again.
    DropAndCreate,
}

/// Automatic case conversion of target names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NameCase {
    /// Keep names as they are.
    #[default]
    Keep,
    /// Lower case (PostgreSQL friendly).
    Lower,
    /// Upper case.
    Upper,
}

impl NameCase {
    fn apply(self, s: &str) -> String {
        match self {
            NameCase::Keep => s.to_string(),
            NameCase::Lower => s.to_lowercase(),
            NameCase::Upper => s.to_uppercase(),
        }
    }
}

/// A saved migration (FR-T4, FR-T5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MigrationJob {
    /// Source tables, migrated in this order.
    pub tables: Vec<ObjectRef>,
    /// Schema of the target tables; `None` uses the target session's current schema.
    pub target_schema: Option<String>,
    /// Target table name by source table name.
    pub table_names: BTreeMap<String, String>,
    /// Target column names: source table name → (source column → target column).
    pub column_names: BTreeMap<String, BTreeMap<String, String>>,
    /// Case conversion for names without an explicit mapping.
    pub name_case: NameCase,
    /// Explicit target types by `table.column` (source names).
    pub column_types: BTreeMap<String, String>,
    /// Create target tables that do not exist.
    pub create_tables: bool,
    /// Target preparation.
    pub mode: MigrationMode,
    /// Rows per batch.
    pub batch_size: usize,
    /// Create secondary indexes after loading each table.
    pub copy_indexes: bool,
    /// Create foreign keys after all tables are loaded.
    pub copy_foreign_keys: bool,
    /// Load each table in one target transaction (otherwise commit per batch).
    pub transaction_per_table: bool,
    /// Handling of rows that cannot be written.
    pub on_error: OnError,
    /// Continue with the next table when one fails.
    pub continue_on_table_error: bool,
}

impl Default for MigrationJob {
    fn default() -> Self {
        MigrationJob {
            tables: Vec::new(),
            target_schema: None,
            table_names: BTreeMap::new(),
            column_names: BTreeMap::new(),
            name_case: NameCase::Keep,
            column_types: BTreeMap::new(),
            create_tables: true,
            mode: MigrationMode::Append,
            batch_size: 1000,
            copy_indexes: true,
            copy_foreign_keys: true,
            transaction_per_table: true,
            on_error: OnError::Abort,
            continue_on_table_error: false,
        }
    }
}

/// Result of migrating one table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TableMigrationSummary {
    /// Source table.
    pub source: Option<ObjectRef>,
    /// Target table.
    pub target: Option<ObjectRef>,
    /// The target table was created.
    pub created: bool,
    /// Rows read from the source.
    pub rows_read: u64,
    /// Rows written to the target.
    pub rows_written: u64,
    /// Rows skipped because of errors.
    pub rows_failed: u64,
    /// Failed rows.
    pub errors: Vec<RowError>,
    /// Secondary indexes created.
    pub indexes_created: u32,
    /// Error that stopped this table.
    pub error: Option<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

/// Result of a migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MigrationSummary {
    /// Per table results.
    pub tables: Vec<TableMigrationSummary>,
    /// Rows written in total.
    pub rows: u64,
    /// Foreign keys created.
    pub foreign_keys_created: u32,
    /// Non fatal problems (skipped triggers, failed indexes ...).
    pub warnings: Vec<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

struct Plan {
    source: ObjectRef,
    source_def: TableDef,
    target: TableDef,
    target_ref: ObjectRef,
    /// (source column, target column, target kind) of the copied columns.
    copy: Vec<(String, String, DataKind)>,
    indexes: Vec<IndexDef>,
}

impl MigrationJob {
    fn table_name(&self, source: &str) -> String {
        self.table_names.get(source).cloned().unwrap_or_else(|| self.name_case.apply(source))
    }

    fn column_name(&self, table: &str, column: &str) -> String {
        self.column_names.get(table).and_then(|m| m.get(column)).cloned().unwrap_or_else(|| self.name_case.apply(column))
    }

    fn map_columns(&self, table: &str, cols: &[String]) -> Vec<String> {
        cols.iter().map(|c| self.column_name(table, c)).collect()
    }
}

/// Map a source definition into the target dialect.
fn plan_table(job: &MigrationJob, source: &ObjectRef, def: TableDef, src_kind: DbKind, dst: &dyn Dialect, warnings: &mut Vec<String>) -> Plan {
    let same = src_kind == dst.kind() && src_kind != DbKind::Generic;
    let tname = job.table_name(&def.name);
    let mut t = TableDef::new(job.target_schema.clone(), tname.clone());
    let mut copy = Vec::new();
    for c in &def.columns {
        let name = job.column_name(&def.name, &c.name);
        let explicit = job.column_types.get(&format!("{}.{}", def.name, c.name)).cloned();
        let data_type = explicit.unwrap_or_else(|| if same { c.data_type.clone() } else { dst.ddl().map_type(src_kind, &c.data_type, c.kind) });
        let mut col = c.clone();
        col.name = name.clone();
        col.data_type = data_type;
        let sequence_default = c.default.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains("nextval("));
        if !same || c.auto_increment || sequence_default {
            col.default = None;
        }
        if !same {
            col.generated = None;
            col.collation = None;
        }
        if col.generated.is_none() {
            let kind = match dst.type_kind(&col.data_type) {
                DataKind::Other => c.kind,
                k => k,
            };
            copy.push((c.name.clone(), name, kind));
        }
        t.columns.push(col);
    }
    let key = |k: &KeyDef| KeyDef { name: if same { k.name.clone() } else { None }, columns: job.map_columns(&def.name, &k.columns) };
    t.primary_key = def.primary_key.as_ref().filter(|k| !k.columns.is_empty()).map(key);
    t.unique_keys = def.unique_keys.iter().map(key).collect();
    if same {
        t.checks = def.checks.clone();
        t.options = def.options.clone();
    } else if !def.checks.is_empty() {
        warnings.push(format!("{}: check constraints are not copied between different databases", def.name));
    }
    t.comment = def.comment.clone();
    if !def.triggers.is_empty() {
        warnings.push(format!("{}: {} trigger(s) are not copied", def.name, def.triggers.len()));
    }
    let indexes: Vec<IndexDef> = dbiji_core::dialect::ansi::secondary_indexes(&def)
        .filter(|i| same || (i.predicate.is_none() && i.columns.iter().all(|c| !c.is_expression)))
        .map(|i| {
            let mut idx = i.clone();
            idx.definition = None;
            if !same {
                idx.method = None;
                idx.include.clear();
            }
            for c in &mut idx.columns {
                if !c.is_expression {
                    c.name = job.column_name(&def.name, &c.name);
                }
            }
            idx
        })
        .collect();
    // MySQL and SQL Server cannot index unbounded text (`longtext`,
    // `nvarchar(max)`): key and index columns get a bounded type.
    if !same {
        let keyed: Vec<String> = t
            .primary_key
            .iter()
            .chain(&t.unique_keys)
            .flat_map(|k| k.columns.iter().cloned())
            .chain(indexes.iter().flat_map(|i| i.columns.iter().filter(|c| !c.is_expression).map(|c| c.name.clone())))
            .collect();
        for col in t.columns.iter_mut().filter(|c| keyed.contains(&c.name)) {
            let explicit = job.column_types.contains_key(&format!("{}.{}", def.name, col.name));
            if let Some(bounded) = bounded_key_type(dst.kind(), &col.data_type).filter(|_| !explicit) {
                warnings.push(format!("{}.{}: {} cannot be a key or index column, {bounded} is used", def.name, col.name, col.data_type));
                col.data_type = bounded.to_string();
            }
        }
    }
    let target_ref = ObjectRef::table(job.target_schema.clone(), tname);
    Plan { source: source.clone(), source_def: def, target: t, target_ref, copy, indexes }
}

/// Bounded type for an unbounded text / binary type `ty` of a key or index column.
fn bounded_key_type(kind: DbKind, ty: &str) -> Option<&'static str> {
    let ty = ty.trim().to_ascii_lowercase();
    match kind {
        DbKind::MySql | DbKind::MariaDb => match ty.as_str() {
            "tinytext" | "text" | "mediumtext" | "longtext" => Some("varchar(255)"),
            "tinyblob" | "blob" | "mediumblob" | "longblob" => Some("varbinary(255)"),
            _ => None,
        },
        DbKind::SqlServer => match ty.replace(' ', "").as_str() {
            "nvarchar(max)" | "ntext" => Some("nvarchar(450)"),
            "varchar(max)" | "text" => Some("varchar(900)"),
            "varbinary(max)" | "image" => Some("varbinary(900)"),
            _ => None,
        },
        _ => None,
    }
}

/// Copy `job.tables` from `source` to `target`.
///
/// Tables are processed in order with one source and one target session.
/// Failures stop the migration unless `continue_on_table_error` is set;
/// cancellation returns an error of kind [`ErrorKind::Cancelled`].
pub async fn run_migration(
    job: MigrationJob,
    source: Arc<dyn SessionFactory>,
    target: Arc<dyn SessionFactory>,
    progress: &Progress,
    cancel: &CancellationToken,
) -> DbResult<MigrationSummary> {
    let started = Instant::now();
    let src_dialect = source.dialect();
    let dst_dialect = target.dialect();
    let mut src = cancellable(source.open(), cancel, None).await?;
    let mut dst = match cancellable(target.open(), cancel, None).await {
        Ok(s) => s,
        Err(e) => {
            let _ = src.close().await;
            return Err(e);
        }
    };
    let result = migrate(&job, src.as_mut(), dst.as_mut(), src_dialect, dst_dialect, progress, cancel).await;
    let _ = src.close().await;
    let _ = dst.close().await;
    let mut summary = result?;
    summary.duration_ms = started.elapsed().as_millis() as u64;
    Ok(summary)
}

async fn migrate(
    job: &MigrationJob,
    src: &mut dyn Session,
    dst: &mut dyn Session,
    src_dialect: Arc<dyn Dialect>,
    dst_dialect: Arc<dyn Dialect>,
    progress: &Progress,
    cancel: &CancellationToken,
) -> DbResult<MigrationSummary> {
    let mut summary = MigrationSummary::default();
    progress.set_items_total(job.tables.len() as u64);
    let src_handle = src.cancel_handle();
    let dst_handle = dst.cancel_handle();

    // 1. Definitions (before any cursor is open on the source session).
    let mut plans = Vec::with_capacity(job.tables.len());
    for t in &job.tables {
        match cancellable(src.table_definition(t), cancel, src_handle.clone()).await {
            Ok(def) => {
                if let Some(n) = def.row_estimate.filter(|n| *n > 0) {
                    progress.add_total_rows(n as u64);
                }
                plans.push(plan_table(job, t, def, src_dialect.kind(), dst_dialect.as_ref(), &mut summary.warnings));
            }
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e) if job.continue_on_table_error => {
                let name = t.display_path();
                progress.add_errors(1);
                progress.error(Some(&name), format!("Cannot read the table definition: {}", e.full_text()));
                progress.item_started(&name);
                progress.item_finished(&name);
                summary.tables.push(TableMigrationSummary {
                    source: Some(t.clone()),
                    error: Some(format!("Cannot read the table definition: {}", e.full_text())),
                    ..Default::default()
                });
            }
            Err(e) => return Err(with_item(e, &t.display_path())),
        }
    }

    // 2. Drop target tables (children first: retry until no progress).
    if job.mode == MigrationMode::DropAndCreate {
        let mut existing: Vec<&Plan> = Vec::new();
        for p in plans.iter().rev() {
            if table_exists(dst, &p.target_ref).await {
                existing.push(p);
            }
        }
        let mut last_error = None;
        while !existing.is_empty() {
            let before = existing.len();
            let mut still = Vec::new();
            for p in existing {
                let sql = dst_dialect.ddl().drop_table(p.target_ref.schema.as_deref(), &p.target_ref.name, false);
                match cancellable(execute_update(dst, &sql, &[]), cancel, dst_handle.clone()).await {
                    Ok(_) => progress.info(Some(&p.target_ref.display_path()), "Dropped existing table"),
                    Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
                    Err(e) => {
                        last_error = Some(with_item(e, &p.target_ref.display_path()));
                        still.push(p);
                    }
                }
            }
            if still.len() == before {
                return Err(last_error.unwrap_or_else(|| DbError::query("Cannot drop the target tables")));
            }
            existing = still;
        }
    }

    // 3. Tables.
    for plan in &plans {
        let name = plan.source.display_path();
        progress.item_started(&name);
        let t0 = Instant::now();
        let mut ts = TableMigrationSummary { source: Some(plan.source.clone()), target: Some(plan.target_ref.clone()), ..Default::default() };
        let outcome = migrate_table(job, plan, src, dst, &dst_dialect, &src_dialect, progress, cancel, &mut ts).await;
        progress.item_finished(&name);
        ts.duration_ms = t0.elapsed().as_millis() as u64;
        summary.rows += ts.rows_written;
        match outcome {
            Ok(()) => {
                if job.copy_indexes {
                    for idx in &plan.indexes {
                        let sql = dst_dialect.ddl().create_index(&plan.target, idx);
                        match cancellable(execute_update(dst, &sql, &[]), cancel, dst_handle.clone()).await {
                            Ok(_) => ts.indexes_created += 1,
                            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
                            Err(e) => summary.warnings.push(format!("{}: index {} not created: {e}", plan.target_ref.display_path(), idx.name)),
                        }
                    }
                }
                progress.info(Some(&name), format!("{} rows copied", ts.rows_written));
                summary.tables.push(ts);
            }
            Err(e) => {
                if e.kind == ErrorKind::Cancelled {
                    return Err(e);
                }
                progress.add_errors(1);
                progress.error(Some(&name), e.full_text());
                ts.error = Some(e.full_text());
                summary.tables.push(ts);
                if !job.continue_on_table_error {
                    return Err(with_item(e, &name));
                }
            }
        }
    }

    // 4. Foreign keys, after all data is loaded.
    if job.copy_foreign_keys {
        for plan in &plans {
            let done = summary.tables.iter().any(|t| t.source.as_ref() == Some(&plan.source) && t.error.is_none());
            if !done {
                continue;
            }
            for fk in &plan.source_def.foreign_keys {
                let Some(parent) = plans.iter().find(|p| p.source_def.name == fk.ref_table && (fk.ref_schema.is_none() || fk.ref_schema == p.source.schema)) else {
                    summary.warnings.push(format!("{}: foreign key to {} not copied (table not migrated)", plan.target_ref.display_path(), fk.ref_table));
                    continue;
                };
                let mapped = ForeignKeyDef {
                    name: fk.name.clone(),
                    schema: plan.target_ref.schema.clone(),
                    table: plan.target_ref.name.clone(),
                    columns: job.map_columns(&plan.source_def.name, &fk.columns),
                    ref_schema: parent.target_ref.schema.clone(),
                    ref_table: parent.target_ref.name.clone(),
                    ref_columns: job.map_columns(&parent.source_def.name, &fk.ref_columns),
                    on_update: fk.on_update,
                    on_delete: fk.on_delete,
                    deferrable: fk.deferrable,
                };
                let sql = dst_dialect.ddl().add_foreign_key(&plan.target, &mapped);
                match cancellable(execute_update(dst, &sql, &[]), cancel, dst_handle.clone()).await {
                    Ok(_) => summary.foreign_keys_created += 1,
                    Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
                    Err(e) => summary.warnings.push(format!("{}: foreign key not created: {e}", plan.target_ref.display_path())),
                }
            }
        }
    }
    for w in &summary.warnings {
        progress.warn(None, w.clone());
    }
    Ok(summary)
}

async fn table_exists(session: &mut dyn Session, table: &ObjectRef) -> bool {
    match session.table_definition(table).await {
        Ok(def) => !def.columns.is_empty(),
        Err(_) => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn migrate_table(
    job: &MigrationJob,
    plan: &Plan,
    src: &mut dyn Session,
    dst: &mut dyn Session,
    dst_dialect: &Arc<dyn Dialect>,
    src_dialect: &Arc<dyn Dialect>,
    progress: &Progress,
    cancel: &CancellationToken,
    ts: &mut TableMigrationSummary,
) -> DbResult<()> {
    let dst_handle = dst.cancel_handle();
    let src_handle = src.cancel_handle();
    let exists = match job.mode {
        MigrationMode::DropAndCreate => false,
        _ => table_exists(dst, &plan.target_ref).await,
    };
    if !exists {
        if !job.create_tables && job.mode != MigrationMode::DropAndCreate {
            return Err(DbError::config(format!("Target table {} does not exist", plan.target_ref.display_path())));
        }
        let mut def = plan.target.clone();
        def.indexes.clear();
        def.foreign_keys.clear();
        for stmt in dst_dialect.ddl().create_table(&def, false) {
            cancellable(execute_update(dst, &stmt, &[]), cancel, dst_handle.clone()).await?;
        }
        ts.created = true;
    }
    if plan.copy.is_empty() {
        return Ok(());
    }
    let key_index: Vec<usize> = if job.mode == MigrationMode::Replace {
        let key = plan.target.row_key().ok_or_else(|| DbError::config(format!("{} has no primary or unique key for replace mode", plan.target_ref.display_path())))?;
        key.columns
            .iter()
            .map(|k| plan.copy.iter().position(|(_, t, _)| t == k).ok_or_else(|| DbError::config(format!("Key column {k} is not copied"))))
            .collect::<DbResult<_>>()?
    } else {
        Vec::new()
    };
    let target = TableTarget {
        table_sql: dst_dialect.qualified_ref(&plan.target_ref),
        columns: plan.copy.iter().map(|(_, t, _)| dst_dialect.quote_ident(t)).collect(),
        types: plan.copy.iter().map(|(_, t, _)| plan.target.columns.iter().find(|c| &c.name == t).map(|c| c.data_type.clone()).unwrap_or_default()).collect(),
        key_index,
    };
    let scope = if job.transaction_per_table { TransactionScope::Whole } else { TransactionScope::PerBatch };
    let select_cols: Vec<String> = plan.copy.iter().map(|(s, _, _)| s.clone()).collect();
    let sql = src_dialect.select_table(&plan.source, Some(&select_cols), None, None, None);
    let kinds: Vec<DataKind> = plan.copy.iter().map(|(_, _, k)| *k).collect();
    let convert = ConvertOptions::default();
    // MySQL gives TIMESTAMP values in the session's time zone without saying
    // which: going into time zone aware columns (PostgreSQL's timestamptz),
    // they are read in UTC on this session (the migration's own), so they keep
    // their instant. Into plain timestamps they keep their wall clock time.
    let mysql = matches!(src_dialect.kind(), DbKind::MySql | DbKind::MariaDb);
    let timestamps: Vec<bool> =
        plan.copy.iter().map(|(s, _, _)| mysql && plan.source_def.columns.iter().any(|c| &c.name == s && c.data_type.to_ascii_lowercase().starts_with("timestamp"))).collect();
    let zoned = |want: bool| timestamps.iter().zip(&kinds).any(|(t, k)| *t && (*k == DataKind::DateTimeTz) == want);
    let read_utc = zoned(true) && !zoned(false);
    if read_utc {
        cancellable(execute_update(src, "SET @dbiji_time_zone = @@session.time_zone, time_zone = '+00:00'", &[]), cancel, src_handle.clone()).await?;
    }
    let utc = ConvertOptions { default_offset_minutes: Some(0), ..ConvertOptions::default() };
    let in_utc: Vec<bool> = timestamps.iter().map(|t| *t && read_utc).collect();
    let cursor = cancellable(src.execute(&sql, &[], &dbiji_core::ExecOptions::default()), cancel, src_handle.clone()).await?;
    let mut rows = cancellable(CursorSource::new(cursor), cancel, src_handle.clone()).await?;
    let name = plan.target_ref.display_path();
    // The source's ids are copied into the target's identity / serial columns.
    let identity: Vec<String> = plan.copy.iter().filter(|(_, t, _)| plan.target.columns.iter().any(|c| &c.name == t && c.auto_increment)).map(|(_, t, _)| t.clone()).collect();
    let table_sql = dst_dialect.qualified_ref(&plan.target_ref);
    let mut writer = BatchWriter::new(dst, target, scope, job.on_error, job.batch_size.min(500), name.clone());
    writer.begin().await?;
    let outcome = async {
        if job.mode == MigrationMode::Truncate {
            writer.clear_table(&plan.target_ref).await?;
        }
        let mut row_no = 0u64;
        loop {
            let batch = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DbError::cancelled()),
                b = rows.next_batch(job.batch_size.max(1)) => b?,
            };
            if batch.is_empty() {
                break;
            }
            let n = batch.len() as u64;
            ts.rows_read += n;
            let mut converted = Vec::with_capacity(batch.len());
            for row in batch {
                row_no += 1;
                let out: dbiji_core::Row = kinds
                    .iter()
                    .enumerate()
                    .map(|(i, kind)| {
                        let v = row.get(i).unwrap_or(&crate::sink::NULL_VALUE);
                        // Values that do not convert are passed on unchanged; the database decides.
                        convert_value(v, *kind, if in_utc[i] { &utc } else { &convert }).unwrap_or_else(|_| v.clone())
                    })
                    .collect();
                converted.push((row_no, out));
            }
            writer.write(converted, progress).await?;
            writer.end_batch().await?;
            progress.add_rows(n);
        }
        Ok(())
    }
    .await;
    let _ = rows.close().await;
    if read_utc {
        let _ = execute_update(src, "SET time_zone = @dbiji_time_zone", &[]).await;
    }
    match outcome {
        Ok(()) => {
            writer.finish().await?;
            // The sequence continues after the copied ids (else the next insert collides).
            if dst_dialect.kind() == DbKind::Postgres {
                for col in &identity {
                    let sql = format!(
                        "SELECT setval(pg_get_serial_sequence({}, {}), MAX({})) FROM {table_sql}",
                        dst_dialect.string_literal(&table_sql),
                        dst_dialect.string_literal(col),
                        dst_dialect.quote_ident(col)
                    );
                    if let Err(e) = writer.execute(&sql).await {
                        writer.errors.push(RowError { row: 0, column: Some(col.clone()), message: format!("Cannot advance the sequence: {}", e.message) });
                    }
                }
            }
            // Oracle: the identity restarts after the largest value in the column.
            if dst_dialect.kind() == DbKind::Oracle {
                for col in &identity {
                    let sql = format!("ALTER TABLE {table_sql} MODIFY ({} GENERATED BY DEFAULT ON NULL AS IDENTITY (START WITH LIMIT VALUE))", dst_dialect.quote_ident(col));
                    if let Err(e) = writer.execute(&sql).await {
                        writer.errors.push(RowError { row: 0, column: Some(col.clone()), message: format!("Cannot advance the identity: {}", e.message) });
                    }
                }
            }
        }
        Err(e) => {
            writer.abort().await;
            if e.kind == ErrorKind::Cancelled
                && let Some(h) = &src_handle
            {
                let _ = h.cancel().await;
            }
            return Err(e);
        }
    }
    ts.rows_written = writer.written;
    ts.rows_failed = writer.failed;
    ts.errors = std::mem::take(&mut writer.errors);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ddl::{ColumnDef, IndexColumn};
    use dbiji_core::dialect::GenericDialect;

    #[test]
    fn plan_maps_names_and_types() {
        let mut def = TableDef::new(Some("app".into()), "Users");
        def.columns.push(ColumnDef::new("Id", "serial").not_null().auto_increment().default_value("nextval('users_id_seq')"));
        def.columns.push(ColumnDef::new("Name", "character varying(40)"));
        let mut generated = ColumnDef::new("Upper", "text");
        generated.generated = Some("upper(name)".into());
        def.columns.push(generated);
        def.primary_key = Some(KeyDef { name: Some("users_pkey".into()), columns: vec!["Id".into()] });
        def.indexes.push(IndexDef { name: "users_name".into(), columns: vec![IndexColumn::column("Name")], ..Default::default() });
        def.indexes.push(IndexDef {
            name: "users_lower".into(),
            columns: vec![IndexColumn { name: "lower(name)".into(), is_expression: true, descending: false }],
            ..Default::default()
        });
        let job = MigrationJob { name_case: NameCase::Lower, target_schema: Some("dst".into()), ..Default::default() };
        let mut warnings = Vec::new();
        let plan = plan_table(&job, &ObjectRef::table(Some("app".into()), "Users"), def, DbKind::Postgres, &GenericDialect::new(), &mut warnings);
        assert_eq!(plan.target.name, "users");
        assert_eq!(plan.target.schema.as_deref(), Some("dst"));
        assert_eq!(plan.target.columns[0].name, "id");
        assert_eq!(plan.target.columns[0].default, None);
        assert_eq!(plan.target.primary_key.as_ref().unwrap().columns, vec!["id".to_string()]);
        assert_eq!(plan.target.primary_key.as_ref().unwrap().name, None);
        // The generated column is copied as a plain column between different products.
        assert_eq!(plan.copy.len(), 3);
        assert_eq!(plan.indexes.len(), 1);
        assert_eq!(plan.indexes[0].columns[0].name, "name");
    }
}
