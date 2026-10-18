//! Export sinks: the [`RowSink`] trait, output targets and one implementation
//! per [`ExportFormat`].

mod csv;
mod html;
mod json;
mod markdown;
mod parquet;
mod php;
mod sql;
mod txt;
mod xlsx;
mod xml;

pub(crate) use json::{is_json_number, is_valid_json};

use crate::encoding::{EncodingWriter, bom_bytes, resolve_encoding};
use crate::error::file_error;
use crate::options::{ExportFormat, ExportOptions};
use dbiji_core::ddl::TableDef;
use dbiji_core::dialect::GenericDialect;
use dbiji_core::{ColumnMeta, DbError, DbResult, Dialect, ObjectRef, Row, Value};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Receives the rows of an export and writes them in some format.
///
/// Protocol: [`begin`](RowSink::begin) once, [`write_row`](RowSink::write_row)
/// for every row, then [`finish`](RowSink::finish). When the export fails or
/// is cancelled [`abort`](RowSink::abort) is called instead of `finish`.
pub trait RowSink: Send {
    /// Start the output (headers, prologue). `table` is the exported table when known.
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()>;

    /// Write one row. Missing trailing values are treated as NULL, extra values are ignored.
    fn write_row(&mut self, row: &Row) -> DbResult<()>;

    /// Complete the output (epilogue, flush, close files).
    fn finish(&mut self) -> DbResult<ExportSummary>;

    /// Stop after a failure or cancellation. File targets are deleted unless
    /// [`ExportOptions::keep_partial_file`] is set.
    fn abort(&mut self) {}

    /// Bytes written to the target so far (for progress).
    fn bytes_written(&self) -> u64 {
        0
    }

    /// Full definition of the exported table, when the caller has it. Used by
    /// the SQL format for `CREATE TABLE` and upsert keys; ignored by others.
    fn set_table_definition(&mut self, def: &TableDef) {
        let _ = def;
    }
}

/// Where an export writes to.
pub enum ExportTarget {
    /// A file, created (or truncated) by the sink.
    Path(PathBuf),
    /// Any writer, e.g. the clipboard buffer or a pipe. XLSX is written to a
    /// temporary file first and then copied into the writer.
    Writer(Box<dyn Write + Send>),
}

impl ExportTarget {
    /// File target.
    pub fn path(path: impl Into<PathBuf>) -> Self {
        ExportTarget::Path(path.into())
    }

    /// Writer target.
    pub fn writer(writer: impl Write + Send + 'static) -> Self {
        ExportTarget::Writer(Box::new(writer))
    }
}

impl fmt::Debug for ExportTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportTarget::Path(p) => f.debug_tuple("Path").field(p).finish(),
            ExportTarget::Writer(_) => f.write_str("Writer(..)"),
        }
    }
}

/// Result of exporting one result set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSummary {
    /// Rows written.
    pub rows: u64,
    /// Bytes written to the target.
    pub bytes: u64,
    /// Files created (empty for writer targets).
    pub files: Vec<PathBuf>,
    /// Non fatal problems (values truncated, characters not representable ...).
    pub warnings: Vec<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

/// Create a sink writing `format` to `output`.
///
/// `dialect` renders literals and identifiers for the SQL format (the target
/// database of the generated script); `None` uses ANSI SQL.
pub fn create_sink(
    format: ExportFormat,
    options: &ExportOptions,
    output: ExportTarget,
    dialect: Option<Arc<dyn Dialect>>,
) -> DbResult<Box<dyn RowSink + Send>> {
    let dialect = dialect.unwrap_or_else(|| Arc::new(GenericDialect::new()));
    Ok(match format {
        ExportFormat::Xlsx => Box::new(xlsx::XlsxSink::new(options, output)?),
        ExportFormat::Parquet => Box::new(parquet::ParquetSink::new(options, output)?),
        text_format => {
            let out = TextOutput::create(output, &options.encoding, options.insert_bom, options.keep_partial_file)?;
            match text_format {
                ExportFormat::Csv => Box::new(csv::CsvSink::new(out, options, false)),
                ExportFormat::Tsv => Box::new(csv::CsvSink::new(out, options, true)),
                ExportFormat::Json => Box::new(json::JsonSink::new(out, options, false)),
                ExportFormat::JsonLines => Box::new(json::JsonSink::new(out, options, true)),
                ExportFormat::Sql => Box::new(sql::SqlSink::new(out, options, dialect)),
                ExportFormat::Markdown => Box::new(markdown::MarkdownSink::new(out, options)),
                ExportFormat::Html => Box::new(html::HtmlSink::new(out, options)),
                ExportFormat::Xml => Box::new(xml::XmlSink::new(out, options)),
                ExportFormat::DbUnit => Box::new(xml::DbUnitSink::new(out, options)),
                ExportFormat::Txt => Box::new(txt::TxtSink::new(out, options)),
                ExportFormat::Php => Box::new(php::PhpSink::new(out, options)),
                ExportFormat::Xlsx | ExportFormat::Parquet => {
                    return Err(DbError::internal("binary format routed to the text writer"));
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// shared helpers

pub(crate) static NULL_VALUE: Value = Value::Null;

/// Value of column `i`, NULL when the row is shorter.
#[inline]
pub(crate) fn cell(row: &Row, i: usize) -> &Value {
    row.get(i).unwrap_or(&NULL_VALUE)
}

/// Counts bytes reaching the target.
pub(crate) struct CountingWriter<W: Write> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

type TextStack = BufWriter<EncodingWriter<CountingWriter<Box<dyn Write + Send>>>>;

/// Buffered, encoding aware output used by the text formats.
pub(crate) struct TextOutput {
    writer: Option<TextStack>,
    count: Arc<AtomicU64>,
    path: Option<PathBuf>,
    keep_partial: bool,
    encoding_name: &'static str,
    started: Instant,
}

impl TextOutput {
    /// Open `target` for text output in `encoding` (optionally writing a BOM).
    pub(crate) fn create(target: ExportTarget, encoding: &str, bom: bool, keep_partial: bool) -> DbResult<TextOutput> {
        let enc = resolve_encoding(encoding)?;
        let (raw, path): (Box<dyn Write + Send>, Option<PathBuf>) = match target {
            ExportTarget::Path(p) => {
                let file = create_file(&p)?;
                (Box::new(file), Some(p))
            }
            ExportTarget::Writer(w) => (w, None),
        };
        let count = Arc::new(AtomicU64::new(0));
        let counting = CountingWriter { inner: raw, count: count.clone() };
        let mut enc_writer = EncodingWriter::new(counting, enc);
        if bom {
            let b = bom_bytes(enc);
            if !b.is_empty() {
                enc_writer.get_mut().write_all(b).map_err(|e| DbError::io(format!("Cannot write output: {e}")))?;
            }
        }
        Ok(TextOutput {
            writer: Some(BufWriter::with_capacity(64 * 1024, enc_writer)),
            count,
            path,
            keep_partial,
            encoding_name: enc.name(),
            started: Instant::now(),
        })
    }

    /// Name of the output encoding (for XML / HTML declarations).
    pub(crate) fn encoding_name(&self) -> &'static str {
        self.encoding_name
    }

    /// Append text.
    pub(crate) fn write_str(&mut self, s: &str) -> DbResult<()> {
        match self.writer.as_mut() {
            Some(w) => w.write_all(s.as_bytes()).map_err(|e| self.write_error(e)),
            None => Err(DbError::internal("export output already closed")),
        }
    }

    fn write_error(&self, e: io::Error) -> DbError {
        match &self.path {
            Some(p) => file_error("Cannot write", p, e),
            None => DbError::io(format!("Cannot write export output: {e}")),
        }
    }

    /// Bytes that reached the target.
    pub(crate) fn bytes(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Flush and close; returns a summary skeleton (bytes, files, warnings, duration).
    pub(crate) fn finish(&mut self, rows: u64) -> DbResult<ExportSummary> {
        let Some(buffered) = self.writer.take() else {
            return Err(DbError::internal("export output already closed"));
        };
        let mut enc = buffered.into_inner().map_err(|e| self.write_error(e.into_error()))?;
        enc.finish().map_err(|e| self.write_error(e))?;
        let mut warnings = Vec::new();
        if enc.unmappable() > 0 {
            warnings.push(format!(
                "{} character(s) cannot be represented in {} and were replaced with '?'",
                enc.unmappable(),
                self.encoding_name
            ));
        }
        drop(enc);
        Ok(ExportSummary {
            rows,
            bytes: self.bytes(),
            files: self.path.iter().cloned().collect(),
            warnings,
            duration_ms: self.started.elapsed().as_millis() as u64,
        })
    }

    /// Drop the output, deleting a partially written file unless configured otherwise.
    pub(crate) fn abort(&mut self) {
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
        if let Some(p) = &self.path
            && !self.keep_partial
        {
            remove_partial(p);
        }
    }
}

/// Create (truncate) an output file, creating missing parent folders.
pub(crate) fn create_file(path: &Path) -> DbResult<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| file_error("Cannot create folder", parent, e))?;
    }
    File::create(path).map_err(|e| file_error("Cannot create", path, e))
}

/// Best effort removal of a partial output file.
pub(crate) fn remove_partial(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!("cannot remove partial export file {}: {e}", path.display());
    }
}

/// Column names made unique (`id`, `id_2`, ...) for formats keyed by name.
pub(crate) fn unique_names(columns: &[ColumnMeta]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let base = if c.name.is_empty() { format!("column{}", i + 1) } else { c.name.clone() };
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|o| o == &name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        out.push(name);
    }
    out
}

/// Table name of an export: the table being exported, else the common base
/// table of all result columns, else `None`.
pub(crate) fn source_table_name(columns: &[ColumnMeta], table: Option<&ObjectRef>) -> Option<(Option<String>, String)> {
    if let Some(t) = table {
        return Some((t.schema.clone(), t.name.clone()));
    }
    let first = columns.first()?.source.as_ref()?;
    if columns.iter().all(|c| c.source.as_ref().is_some_and(|s| s.table == first.table && s.schema == first.schema)) {
        return Some((first.schema.clone(), first.table.clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnSource, DataKind};

    #[test]
    fn names_are_unique() {
        let cols = vec![
            ColumnMeta::new("id", "int", DataKind::Int),
            ColumnMeta::new("id", "int", DataKind::Int),
            ColumnMeta::new("", "text", DataKind::Text),
            ColumnMeta::new("id_2", "int", DataKind::Int),
        ];
        assert_eq!(unique_names(&cols), vec!["id", "id_2", "column3", "id_2_2"]);
    }

    #[test]
    fn table_name_from_sources() {
        let src =
            |t: &str| ColumnSource { database: None, schema: Some("s".into()), table: t.into(), column: "c".into() };
        let cols = vec![
            ColumnMeta::new("a", "int", DataKind::Int).with_source(src("t")),
            ColumnMeta::new("b", "int", DataKind::Int).with_source(src("t")),
        ];
        assert_eq!(source_table_name(&cols, None), Some((Some("s".into()), "t".into())));
        let mixed = vec![cols[0].clone(), ColumnMeta::new("b", "int", DataKind::Int).with_source(src("u"))];
        assert_eq!(source_table_name(&mixed, None), None);
    }

    #[test]
    fn text_output_counts_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/out.txt");
        let mut out = TextOutput::create(ExportTarget::path(&p), "utf-8", true, false).unwrap();
        out.write_str("héllo").unwrap();
        let s = out.finish(1).unwrap();
        assert_eq!(s.bytes, 3 + "héllo".len() as u64);
        assert_eq!(std::fs::read(&p).unwrap()[..3], [0xEF, 0xBB, 0xBF]);
        let p2 = dir.path().join("partial.txt");
        let mut out = TextOutput::create(ExportTarget::path(&p2), "utf-8", false, false).unwrap();
        out.write_str("x").unwrap();
        out.abort();
        assert!(!p2.exists());
    }
}
