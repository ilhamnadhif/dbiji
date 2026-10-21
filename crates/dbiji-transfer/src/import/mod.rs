//! Import (FR-T3): readers for CSV, XLSX, XML and JSON, type inference,
//! column mapping and the import engine writing into a table.

pub mod csv;
pub mod infer;
pub mod json;
pub mod xlsx;
pub mod xml;

use crate::convert::{ConvertOptions, convert_value};
use crate::export::{SessionFactory, cancellable};
use crate::progress::Progress;
use crate::source::{PeekableSource, RowSource};
use crate::writer::{BatchWriter, OnError, RowError, TableTarget, TransactionScope};
use dbiji_core::ddl::{ColumnDef, KeyDef, TableDef};
use dbiji_core::driver::execute_update;
use dbiji_core::{ColumnMeta, DataKind, DbError, DbKind, DbResult, Dialect, ErrorKind, ObjectRef, Session};
use infer::{InferOptions, InferredColumn, generic_type_name, infer_columns};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// How imported rows are combined with existing table data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// Insert the rows.
    #[default]
    Append,
    /// Delete all rows first, then insert.
    TruncateFirst,
    /// Insert or replace rows by key ([`ImportMapping::key_columns`]) via
    /// [`Dialect::upsert`]; dialects without upsert get `DELETE` + `INSERT`.
    Replace,
}

/// Mapping of one source column to a target column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColumnMapping {
    /// Index of the source column.
    pub source_index: usize,
    /// Target column name.
    pub target_name: String,
    /// Target database type, used when the table is created (empty: derived from the kind).
    pub target_type: String,
    /// Kind values are converted to; `None` derives it from `target_type`
    /// (or the source column when no type is given).
    pub target_kind: Option<DataKind>,
    /// Do not import this column.
    pub skip: bool,
    /// Column allows NULL (only used when the table is created).
    pub nullable: bool,
}

impl ColumnMapping {
    /// Map source column `source_index` to `target_name`.
    pub fn new(source_index: usize, target_name: impl Into<String>) -> Self {
        ColumnMapping { source_index, target_name: target_name.into(), nullable: true, ..Default::default() }
    }

    /// Set the target type and kind.
    pub fn typed(mut self, target_type: impl Into<String>, kind: DataKind) -> Self {
        self.target_type = target_type.into();
        self.target_kind = Some(kind);
        self
    }
}

/// Complete description of an import into one table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportMapping {
    /// Target table.
    pub target: ObjectRef,
    /// Create the table before importing (with the mapped types).
    pub create_table: bool,
    /// Column mappings; source columns without a mapping are not imported.
    pub columns: Vec<ColumnMapping>,
    /// Append, truncate first or replace by key.
    pub mode: ImportMode,
    /// Target key columns for [`ImportMode::Replace`] (also the primary key of a created table).
    pub key_columns: Vec<String>,
    /// Rows read and committed per batch.
    pub batch_size: usize,
    /// Rows per `INSERT` statement (capped at 1000 rows and 2000 parameters per statement).
    pub rows_per_statement: usize,
    /// Transaction boundaries.
    pub transaction: TransactionScope,
    /// Error handling.
    pub on_error: OnError,
    /// Stop after this many failed rows (with `SkipRow` / `LogAndContinue`).
    pub max_errors: Option<u64>,
    /// Parsing of text values (date formats, decimal separator, NULL markers ...).
    pub convert: ConvertOptions,
}

impl Default for ImportMapping {
    fn default() -> Self {
        ImportMapping {
            target: ObjectRef::table(None, ""),
            create_table: false,
            columns: Vec::new(),
            mode: ImportMode::Append,
            key_columns: Vec::new(),
            batch_size: 1000,
            rows_per_statement: 200,
            transaction: TransactionScope::PerBatch,
            on_error: OnError::Abort,
            max_errors: None,
            convert: ConvertOptions::default(),
        }
    }
}

impl ImportMapping {
    /// Empty mapping into `target`.
    pub fn new(target: ObjectRef) -> Self {
        ImportMapping { target, ..Default::default() }
    }

    /// Mapping creating `target` with one column per inferred column; types
    /// are mapped into `dialect` from their ANSI names. Columns are created
    /// nullable: a sample without NULLs does not prove the rest has none
    /// (key columns become NOT NULL through the primary key).
    pub fn from_inferred(target: ObjectRef, inferred: &[InferredColumn], dialect: &dyn Dialect) -> Self {
        let columns = inferred
            .iter()
            .enumerate()
            .map(|(i, c)| ColumnMapping {
                source_index: i,
                target_name: c.name.clone(),
                target_type: dialect.ddl().map_type(DbKind::Generic, &generic_type_name(c), c.kind),
                target_kind: Some(c.kind),
                skip: false,
                nullable: true,
            })
            .collect();
        ImportMapping { target, create_table: true, columns, ..Default::default() }
    }

    /// Mapping into an existing table: source columns are matched to table
    /// columns by name (case-insensitive); unmatched source columns are skipped.
    pub fn match_table(source: &[ColumnMeta], table: &TableDef) -> Self {
        let columns = source
            .iter()
            .enumerate()
            .map(|(i, s)| match table.columns.iter().find(|c| c.name.eq_ignore_ascii_case(s.name.trim())) {
                Some(c) => ColumnMapping {
                    source_index: i,
                    target_name: c.name.clone(),
                    target_type: c.data_type.clone(),
                    target_kind: Some(c.kind),
                    skip: c.generated.is_some(),
                    nullable: c.nullable,
                },
                None => ColumnMapping { source_index: i, target_name: s.name.clone(), skip: true, nullable: true, ..Default::default() },
            })
            .collect();
        let target = ObjectRef::table(table.schema.clone(), table.name.clone());
        let key_columns = table.row_key().map(|k| k.columns.clone()).unwrap_or_default();
        ImportMapping { target, columns, key_columns, ..Default::default() }
    }
}

/// Result of an import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImportSummary {
    /// Rows read from the source.
    pub rows_read: u64,
    /// Rows written (inserted or replaced).
    pub rows_written: u64,
    /// Rows that failed and were skipped.
    pub rows_failed: u64,
    /// Failed rows with their 1-based source row numbers.
    pub errors: Vec<RowError>,
    /// More errors happened than are listed.
    pub errors_truncated: bool,
    /// The target table was created.
    pub created_table: bool,
    /// SQL statements executed.
    pub statements: u64,
    /// Warnings from the reader and the writer.
    pub warnings: Vec<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

/// Import every row of `source` into the table described by `mapping`.
///
/// The table is created first when requested (`ddl().create_table`), then
/// rows are converted to the target kinds ([`convert_value`]) and written with
/// multi-row `INSERT`s using placeholders. When a statement fails and
/// `on_error` allows continuing, its rows are retried one by one (inside
/// savepoints where the database supports them) to isolate the bad rows.
pub async fn run_import(
    source: &mut dyn RowSource,
    session: &mut dyn Session,
    mapping: &ImportMapping,
    progress: &Progress,
    cancel: &CancellationToken,
) -> DbResult<ImportSummary> {
    let started = Instant::now();
    let result = import_rows(source, session, mapping, progress, cancel).await;
    let closed = source.close().await;
    let mut summary = result?;
    summary.warnings.extend(source.take_warnings());
    if let Err(e) = closed {
        summary.warnings.push(format!("Closing the source failed: {e}"));
    }
    summary.duration_ms = started.elapsed().as_millis() as u64;
    Ok(summary)
}

async fn import_rows(source: &mut dyn RowSource, session: &mut dyn Session, mapping: &ImportMapping, progress: &Progress, cancel: &CancellationToken) -> DbResult<ImportSummary> {
    let dialect = session.dialect();
    let src_cols = source.columns().to_vec();
    let mut convert = mapping.convert.clone();
    convert.prefer_base64 = source.binary_is_base64();
    let active: Vec<&ColumnMapping> = mapping.columns.iter().filter(|c| !c.skip).collect();
    if mapping.target.name.trim().is_empty() {
        return Err(DbError::config("The import has no target table"));
    }
    if active.is_empty() {
        return Err(DbError::config("No column is mapped for import"));
    }
    for c in &active {
        if c.source_index >= src_cols.len() {
            return Err(DbError::config(format!(
                "Column mapping '{}' refers to source column {} but the source has {} columns",
                c.target_name,
                c.source_index + 1,
                src_cols.len()
            )));
        }
        if c.target_name.trim().is_empty() {
            return Err(DbError::config(format!("Source column {} has no target name", c.source_index + 1)));
        }
    }
    for (i, a) in active.iter().enumerate() {
        if active[..i].iter().any(|b| b.target_name.eq_ignore_ascii_case(&a.target_name)) {
            return Err(DbError::config(format!("Target column '{}' is mapped twice", a.target_name)));
        }
    }
    let kinds: Vec<DataKind> = active
        .iter()
        .map(|c| c.target_kind.unwrap_or_else(|| if c.target_type.trim().is_empty() { src_cols[c.source_index].kind } else { dialect.type_kind(&c.target_type) }))
        .collect();
    let item = mapping.target.display_path();
    if let Some(total) = source.total_rows() {
        progress.add_total_rows(total);
    }

    let mut summary = ImportSummary::default();
    if mapping.create_table {
        let mut def = TableDef::new(mapping.target.schema.clone(), mapping.target.name.clone());
        for (i, (c, kind)) in active.iter().zip(&kinds).enumerate() {
            let data_type = if c.target_type.trim().is_empty() { dialect.ddl().default_type_for(*kind) } else { c.target_type.clone() };
            def.columns.push(ColumnDef {
                name: c.target_name.clone(),
                data_type,
                kind: *kind,
                nullable: c.nullable && !mapping.key_columns.iter().any(|k| k.eq_ignore_ascii_case(&c.target_name)),
                ordinal: i as u32 + 1,
                ..Default::default()
            });
        }
        if !mapping.key_columns.is_empty() {
            def.primary_key = Some(KeyDef { name: None, columns: mapping.key_columns.clone() });
        }
        let handle = session.cancel_handle();
        for stmt in dialect.ddl().create_table(&def, false) {
            cancellable(execute_update(session, &stmt, &[]), cancel, handle.clone()).await?;
        }
        summary.created_table = true;
        progress.info(Some(&item), "Table created");
    }

    let key_index: Vec<usize> = if mapping.mode == ImportMode::Replace {
        if mapping.key_columns.is_empty() {
            return Err(DbError::config("Replace mode needs key columns"));
        }
        mapping
            .key_columns
            .iter()
            .map(|k| {
                active.iter().position(|c| c.target_name.eq_ignore_ascii_case(k)).ok_or_else(|| DbError::config(format!("Key column '{k}' is not among the imported columns")))
            })
            .collect::<DbResult<_>>()?
    } else {
        Vec::new()
    };
    let target = TableTarget { table_sql: dialect.qualified_ref(&mapping.target), columns: active.iter().map(|c| dialect.quote_ident(&c.target_name)).collect(), key_index };
    let handle = session.cancel_handle();
    let mut writer = BatchWriter::new(session, target, mapping.transaction, mapping.on_error, mapping.rows_per_statement, item.clone());
    writer.begin().await?;
    let outcome = async {
        if mapping.mode == ImportMode::TruncateFirst {
            writer.clear_table(&mapping.target).await?;
        }
        let batch_size = mapping.batch_size.max(1);
        let mut row_no = 0u64;
        loop {
            let batch = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(DbError::cancelled()),
                b = source.next_batch(batch_size) => b?,
            };
            if batch.is_empty() {
                break;
            }
            let n = batch.len() as u64;
            summary.rows_read += n;
            let mut converted = Vec::with_capacity(batch.len());
            for row in batch {
                row_no += 1;
                let mut out = Vec::with_capacity(active.len());
                let mut failure = None;
                for (c, kind) in active.iter().zip(&kinds) {
                    let v = row.get(c.source_index).unwrap_or(&crate::sink::NULL_VALUE);
                    match convert_value(v, *kind, &convert) {
                        Ok(x) => out.push(x),
                        Err(msg) => {
                            failure = Some((c.target_name.clone(), msg));
                            break;
                        }
                    }
                }
                match failure {
                    None => converted.push((row_no, out)),
                    Some((col, msg)) => {
                        if mapping.on_error == OnError::Abort {
                            return Err(DbError::config(format!("Row {row_no}, column {col}: {msg}")));
                        }
                        writer.record(row_no, Some(col), msg, progress);
                    }
                }
            }
            writer.write(converted, progress).await?;
            writer.end_batch().await?;
            progress.add_rows(n);
            if let Some(max) = mapping.max_errors
                && writer.failed > max
            {
                return Err(DbError::config(format!("Import stopped after {} failed rows", writer.failed)));
            }
            if cancel.is_cancelled() {
                return Err(DbError::cancelled());
            }
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => writer.finish().await?,
        Err(e) => {
            writer.abort().await;
            if e.kind == ErrorKind::Cancelled
                && let Some(h) = handle
            {
                let _ = h.cancel().await;
            }
            return Err(crate::error::with_item(e, &item));
        }
    }
    summary.rows_written = writer.written;
    summary.rows_failed = writer.failed;
    summary.errors = std::mem::take(&mut writer.errors);
    summary.errors_truncated = writer.errors_truncated;
    summary.statements = writer.statements;
    summary.warnings.extend(std::mem::take(&mut writer.warnings));
    Ok(summary)
}

/// Source file of a saved import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImportSourceConfig {
    /// CSV / TSV file.
    Csv {
        /// File path.
        path: PathBuf,
        /// Reader options.
        #[serde(default)]
        options: csv::CsvReadOptions,
    },
    /// Excel workbook.
    Xlsx {
        /// File path.
        path: PathBuf,
        /// Reader options.
        #[serde(default)]
        options: xlsx::XlsxReadOptions,
    },
    /// XML document.
    Xml {
        /// File path.
        path: PathBuf,
        /// Reader options.
        #[serde(default)]
        options: xml::XmlReadOptions,
    },
    /// JSON / JSON Lines document.
    Json {
        /// File path.
        path: PathBuf,
        /// Reader options.
        #[serde(default)]
        options: json::JsonReadOptions,
    },
}

impl Default for ImportSourceConfig {
    fn default() -> Self {
        ImportSourceConfig::Csv { path: PathBuf::new(), options: csv::CsvReadOptions::default() }
    }
}

impl ImportSourceConfig {
    /// Open the file as a row source.
    pub async fn open(&self) -> DbResult<Box<dyn RowSource>> {
        Ok(match self {
            ImportSourceConfig::Csv { path, options } => Box::new(csv::CsvSource::open(path, options.clone())?),
            ImportSourceConfig::Xlsx { path, options } => Box::new(xlsx::XlsxSource::open(path, options.clone()).await?),
            ImportSourceConfig::Xml { path, options } => Box::new(xml::XmlSource::open(path, options.clone())?),
            ImportSourceConfig::Json { path, options } => Box::new(json::JsonSource::open(path, options.clone())?),
        })
    }
}

/// A saved import task (FR-T5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImportJob {
    /// Source file.
    pub source: ImportSourceConfig,
    /// Target table and mapping. With an empty column list the columns are
    /// mapped automatically: by name onto the existing table, or (with
    /// `create_table`) as a new table typed from a sample of the rows.
    pub mapping: ImportMapping,
    /// Type inference options for automatic mappings.
    pub infer: InferOptions,
    /// Rows sampled for automatic type inference.
    pub sample_rows: usize,
}

/// Run a saved import: open the file and a session, complete an automatic
/// mapping when needed, and import.
pub async fn run_import_job(job: ImportJob, factory: Arc<dyn SessionFactory>, progress: Arc<Progress>, cancel: CancellationToken) -> DbResult<ImportSummary> {
    let source = job.source.open().await?;
    let mut source = PeekableSource::new(source);
    let mut session = cancellable(factory.open(), &cancel, None).await?;
    let name = job.mapping.target.display_path();
    progress.set_items_total(1);
    progress.item_started(&name);
    let mut mapping = job.mapping.clone();
    let result = async {
        if mapping.columns.is_empty() {
            let columns = source.columns().to_vec();
            if mapping.create_table {
                let sample = source.peek(job.sample_rows.max(1)).await?.to_vec();
                let inferred = infer_columns(&columns, &sample, &job.infer);
                let auto = ImportMapping::from_inferred(mapping.target.clone(), &inferred, session.dialect().as_ref());
                mapping.columns = auto.columns;
            } else {
                let def = session.table_definition(&mapping.target).await?;
                let auto = ImportMapping::match_table(&columns, &def);
                mapping.columns = auto.columns;
                if mapping.key_columns.is_empty() {
                    mapping.key_columns = auto.key_columns;
                }
            }
        }
        run_import(&mut source, session.as_mut(), &mapping, &progress, &cancel).await
    }
    .await;
    progress.item_finished(&name);
    let _ = session.close().await;
    result
}
