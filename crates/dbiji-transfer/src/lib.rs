//! Data transfer for DBiji: export, import, migration between databases and
//! mock data generation (PRD FR-T1..FR-T5 and FR-D8).
//!
//! Everything streams in bounded batches: rows flow from a [`RowSource`]
//! (a query cursor, rows already fetched into the grid, an import file ...)
//! into a [`RowSink`] (an export file) or into a table of a database session,
//! so memory stays proportional to one batch whatever the number of rows.
//! Long operations report into a shared [`Progress`] and stop promptly when
//! their [`CancellationToken`] is cancelled.
//!
//! The crate talks to databases only through [`dbiji_core::Session`] /
//! [`dbiji_core::QueryCursor`] and renders SQL only through
//! [`dbiji_core::Dialect`], so it works with every driver.
//!
//! # Overview
//!
//! * **Export** – [`create_sink`] builds a sink for one of the
//!   [`ExportFormat`]s; [`run_export`] pumps a source into it. Multi-object
//!   jobs (many tables or queries, several threads, file name patterns) run
//!   with [`run_export_job`].
//! * **Import** – readers for CSV ([`CsvSource`]), XLSX ([`XlsxSource`]),
//!   XML ([`XmlSource`]) and JSON ([`JsonSource`]) produce row sources;
//!   [`infer_columns`] guesses column types from sample rows and
//!   [`run_import`] writes the rows into a table following an
//!   [`ImportMapping`].
//! * **Migration** – [`run_migration`] copies tables (definition, data,
//!   indexes, foreign keys) between two connections, possibly of different
//!   database products.
//! * **Mock data** – [`run_mock`] fills a table with generated test data,
//!   [`auto_generators`] picks sensible generators from column names and types.
//!
//! All configuration types implement `Serialize`/`Deserialize` and `Default`
//! so that transfers can be saved as tasks (FR-T5).
//!
//! # Example
//!
//! Export a query to CSV, then import the file into a new table typed from
//! a sample of its rows:
//!
//! ```no_run
//! use dbiji_core::{ObjectRef, Session};
//! use dbiji_transfer::*;
//!
//! # async fn example(session: &mut dyn Session) -> dbiji_core::DbResult<()> {
//! let progress = Progress::new();
//! let cancel = CancellationToken::new();
//!
//! let mut source = CursorSource::execute(session, "SELECT * FROM users", &[]).await?;
//! let mut sink = create_sink(ExportFormat::Csv, &ExportOptions::default(), ExportTarget::path("users.csv"), None)?;
//! let exported = run_export(&mut source, sink.as_mut(), &progress, &cancel).await?;
//! drop(source); // one open cursor per session
//!
//! let mut csv = PeekableSource::new(CsvSource::open("users.csv", CsvReadOptions::default())?);
//! let columns = csv.columns().to_vec();
//! let sample = csv.peek(100).await?.to_vec();
//! let inferred = infer_columns(&columns, &sample, &InferOptions::default());
//! let mapping = ImportMapping::from_inferred(ObjectRef::table(None, "users_copy"), &inferred, session.dialect().as_ref());
//! let imported = run_import(&mut csv, session, &mapping, &progress, &cancel).await?;
//! assert_eq!(exported.rows, imported.rows_written + imported.rows_failed);
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
// `DbError` (the error type mandated by dbiji-core) is 144 bytes, so every
// `DbResult` trips this lint; boxing it is a decision for dbiji-core.
#![allow(clippy::result_large_err)]

mod convert;
mod encoding;
mod error;
mod export;
mod format;
mod import;
mod migrate;
mod mock;
mod options;
mod progress;
mod sink;
mod source;
mod writer;

pub use tokio_util::sync::CancellationToken;

pub use convert::{ConvertOptions, convert_value};
pub use export::{
    DEFAULT_BATCH_SIZE, DEFAULT_FILE_NAME_PATTERN, ExportItem, ExportItemSummary, ExportJob, ExportJobSummary, SessionFactory, expand_file_name, run_export, run_export_batched,
    run_export_job, run_export_job_with,
};
pub use format::{BinaryEncoding, ValueFormat, format_value};
pub use import::csv::{CsvHeader, CsvReadOptions, CsvSource};
pub use import::infer::{InferOptions, InferredColumn, generic_type_name, infer_columns};
pub use import::json::{JsonInputFormat, JsonReadOptions, JsonSource};
pub use import::xlsx::{SheetSelector, XlsxReadOptions, XlsxSource, xlsx_sheet_names};
pub use import::xml::{XmlReadOptions, XmlSource};
pub use import::{ColumnMapping, ImportJob, ImportMapping, ImportMode, ImportSourceConfig, ImportSummary, run_import, run_import_job};
pub use migrate::{MigrationJob, MigrationMode, MigrationSummary, NameCase, TableMigrationSummary, run_migration};
pub use mock::{ColumnGenerator, GeneratorKind, MockConfig, MockMode, MockSummary, auto_generators, run_mock};
pub use options::{
    CsvOptions, DbUnitOptions, EscapeStyle, ExportFormat, ExportOptions, HeaderStyle, HtmlOptions, IdentifierQuoting, JsonLayout, JsonOptions, LineEnding, MarkdownOptions,
    ParquetCompression, ParquetOptions, PhpOptions, QuoteMode, SqlInsertMode, SqlOptions, TxtOptions, XlsxOptions, XmlNullMode, XmlOptions,
};
pub use progress::{MessageLevel, Progress, ProgressMessage, ProgressSnapshot};
pub use sink::{ExportSummary, ExportTarget, RowSink, create_sink};
pub use source::{CursorSource, PeekableSource, RowSource, RowStoreSource, VecSource};
pub use writer::{OnError, RowError, TransactionScope};
