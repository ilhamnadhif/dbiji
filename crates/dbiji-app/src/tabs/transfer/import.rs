//! Import wizard (FR-T2): CSV / TSV / XLSX / XML / JSON files into an
//! existing or a new table, with a live preview, type guessing, column
//! mapping, conversion options and a report of the rows that failed.

use super::{
    COMMON_KINDS, MessageLog, Running, TaskSaver, char_edit, combo, connection_name, current_schema, error_text, kind_label, lines_edit, next_uid, not_connected, on_error_options,
    opt_char_edit, opt_number, opt_text, progress_ui, reload_table_def, schema_combo, schema_tables, section, steps_ui, table_def_of, text_table, transaction_options,
};
use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::transferexec::{AppSessionFactory, database_of, import_task_config};
use dbiji_core::meta::{ObjectRef, SchemaRef};
use dbiji_core::{ColumnMeta, DbError, DbResult, Row, tr, trf};
use dbiji_tasks::TaskKind;
use dbiji_transfer::{
    BinaryEncoding, ColumnMapping, CsvHeader, CsvReadOptions, EscapeStyle, ImportJob, ImportMapping, ImportMode, ImportSourceConfig, ImportSummary, InferOptions, JsonInputFormat,
    JsonReadOptions, OnError, PeekableSource, RowSource, SessionFactory, SheetSelector, XlsxReadOptions, XmlReadOptions, infer_columns, run_import_job, xlsx_sheet_names,
};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PAGE_FILE: usize = 0;
const PAGE_TARGET: usize = 1;
const PAGE_OPTIONS: usize = 2;
const PAGE_RUN: usize = 3;

/// Rows shown in the preview table.
const PREVIEW_ROWS: usize = 50;

/// Encodings offered for input files (`auto` detects BOM / UTF-8 / windows-1252).
const ENCODINGS: [&str; 12] = ["auto", "UTF-8", "UTF-16LE", "UTF-16BE", "windows-1252", "ISO-8859-1", "ISO-8859-15", "windows-1250", "windows-1251", "Shift_JIS", "GBK", "Big5"];

/// Format of the file to import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFileFormat {
    Csv,
    Tsv,
    Xlsx,
    Xml,
    Json,
}

impl ImportFileFormat {
    pub const ALL: [ImportFileFormat; 5] = [ImportFileFormat::Csv, ImportFileFormat::Tsv, ImportFileFormat::Xlsx, ImportFileFormat::Xml, ImportFileFormat::Json];

    /// Format for a file name extension.
    pub fn from_path(path: &Path) -> Option<ImportFileFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" | "txt" => ImportFileFormat::Csv,
            "tsv" | "tab" => ImportFileFormat::Tsv,
            "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => ImportFileFormat::Xlsx,
            "xml" => ImportFileFormat::Xml,
            "json" | "jsonl" | "ndjson" => ImportFileFormat::Json,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ImportFileFormat::Csv => tr("CSV"),
            ImportFileFormat::Tsv => tr("TSV"),
            ImportFileFormat::Xlsx => tr("Excel (XLSX)"),
            ImportFileFormat::Xml => tr("XML"),
            ImportFileFormat::Json => tr("JSON / JSON Lines"),
        }
    }
}

/// Existing or new target table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportTarget {
    Existing,
    New,
}

/// First rows of the file read in the background.
struct Preview {
    config: ImportSourceConfig,
    columns: Vec<ColumnMeta>,
    rows: Vec<Row>,
    warnings: Vec<String>,
}

/// What the current column mapping was built for.
#[derive(Clone, PartialEq)]
enum MappingKey {
    New { columns: Vec<String>, infer: InferOptions },
    Existing { table: ObjectRef, columns: Vec<String> },
}

/// The import wizard tab.
pub struct ImportTab {
    uid: u64,
    conn: String,
    page: usize,
    // ---------------------------------------------------------------- file
    path: String,
    format: ImportFileFormat,
    csv: CsvReadOptions,
    xlsx: XlsxReadOptions,
    xml: XmlReadOptions,
    json: JsonReadOptions,
    /// Rows read for the preview and the type guess.
    sample_rows: usize,
    sheets_job: Option<(String, Job<DbResult<Vec<String>>>)>,
    sheets: Option<(String, Vec<String>)>,
    preview_job: Option<Job<Result<Preview, (ImportSourceConfig, String)>>>,
    preview: Option<Preview>,
    preview_error: Option<(ImportSourceConfig, String)>,
    // -------------------------------------------------------------- target
    target: ImportTarget,
    schema: Option<SchemaRef>,
    table: Option<ObjectRef>,
    new_name: String,
    columns: Vec<ColumnMapping>,
    key_columns: Vec<String>,
    mapping_for: Option<MappingKey>,
    columns_edited: bool,
    mapping_error: Option<String>,
    // ------------------------------------------------------------- options
    /// Mode, batches, transactions, errors and conversion (target and
    /// columns are taken from the fields above).
    options: ImportMapping,
    infer: InferOptions,
    // ----------------------------------------------------------------- run
    run: Option<Running<ImportSummary>>,
    log: MessageLog,
    result: Option<Result<ImportSummary, String>>,
    error: Option<String>,
    report_job: Option<Job<std::io::Result<PathBuf>>>,
    saver: TaskSaver,
}

impl ImportTab {
    /// Import into `table` (an existing table), or into a new table of
    /// `schema` when no table is given.
    pub fn new(conn: String, table: Option<ObjectRef>, schema: Option<SchemaRef>) -> Self {
        let table = table.filter(|t| t.kind.is_table_like());
        let schema = schema.or_else(|| table.as_ref().map(ObjectRef::schema_ref)).filter(|s| s.schema.is_some() || s.database.is_some());
        ImportTab {
            uid: next_uid(),
            conn,
            page: PAGE_FILE,
            path: String::new(),
            format: ImportFileFormat::Csv,
            csv: CsvReadOptions::default(),
            xlsx: XlsxReadOptions::default(),
            xml: XmlReadOptions::default(),
            json: JsonReadOptions::default(),
            sample_rows: 500,
            sheets_job: None,
            sheets: None,
            preview_job: None,
            preview: None,
            preview_error: None,
            target: if table.is_some() { ImportTarget::Existing } else { ImportTarget::New },
            schema,
            table,
            new_name: String::new(),
            columns: Vec::new(),
            key_columns: Vec::new(),
            mapping_for: None,
            columns_edited: false,
            mapping_error: None,
            options: ImportMapping::default(),
            infer: InferOptions::default(),
            run: None,
            log: MessageLog::default(),
            result: None,
            error: None,
            report_job: None,
            saver: TaskSaver::default(),
        }
    }

    // ------------------------------------------------------------ public API

    /// Choose the file; the format follows the extension.
    pub fn set_file(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        self.path = path.display().to_string();
        if let Some(f) = ImportFileFormat::from_path(path) {
            self.format = f;
        }
        if self.new_name.trim().is_empty()
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            self.new_name = table_name_from(stem);
        }
    }

    pub fn set_format(&mut self, format: ImportFileFormat) {
        self.format = format;
    }

    /// CSV / TSV reader options.
    pub fn csv_options_mut(&mut self) -> &mut CsvReadOptions {
        &mut self.csv
    }

    /// The preview of the current file and reader options is loaded.
    pub fn preview_ready(&self) -> bool {
        self.preview.as_ref().is_some_and(|p| p.config == self.source_config())
    }

    /// Error of the last preview attempt.
    pub fn preview_error(&self) -> Option<&str> {
        self.preview_error.as_ref().map(|(_, e)| e.as_str())
    }

    /// Column names of the preview.
    pub fn preview_columns(&self) -> Vec<String> {
        self.preview.as_ref().map(|p| p.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default()
    }

    /// Rows read by the preview (at most the sample size).
    pub fn preview_row_count(&self) -> usize {
        self.preview.as_ref().map(|p| p.rows.len()).unwrap_or(0)
    }

    /// Import into a new table `name` of `schema` (`None`: the default schema).
    pub fn set_new_table(&mut self, schema: Option<SchemaRef>, name: &str) {
        self.target = ImportTarget::New;
        self.schema = schema;
        self.new_name = name.to_string();
    }

    /// Import into an existing table.
    pub fn set_existing_table(&mut self, table: ObjectRef) {
        self.target = ImportTarget::Existing;
        self.schema = Some(table.schema_ref());
        self.table = Some(table);
    }

    /// The column mapping is built for the current preview and target.
    pub fn mapping_ready(&self) -> bool {
        self.preview_ready() && self.mapping_for == self.current_key() && !self.columns.is_empty()
    }

    /// Column mappings (editable).
    pub fn columns_mut(&mut self) -> &mut Vec<ColumnMapping> {
        self.columns_edited = true;
        &mut self.columns
    }

    /// Mode, batch size, transactions, error handling and conversion options.
    pub fn options_mut(&mut self) -> &mut ImportMapping {
        &mut self.options
    }

    /// The complete mapping as it would run now.
    pub fn mapping(&self) -> Option<ImportMapping> {
        let target = self.target_ref()?;
        let mut m = self.options.clone();
        m.target = target;
        m.create_table = self.target == ImportTarget::New;
        m.columns = self.columns.clone();
        m.key_columns = self.key_columns.clone();
        Some(m)
    }

    /// The job as it would run now.
    pub fn job(&self) -> Option<ImportJob> {
        Some(ImportJob { source: self.source_config(), mapping: self.mapping()?, infer: self.infer_options(), sample_rows: self.sample_rows })
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn set_page(&mut self, page: usize) {
        self.page = page.min(PAGE_RUN);
    }

    /// Run the import (the "Run" button).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.run.is_some() {
            return;
        }
        if let Err(e) = self.validate() {
            self.error = Some(e);
            return;
        }
        let Some(config) = cx.conns.config(&self.conn) else { return };
        if config.read_only {
            self.error = Some(trf("The connection \"{}\" is read-only.", &[&config.name]));
            return;
        }
        if cx.conns.live(&self.conn).is_none() {
            self.error = Some(not_connected(cx, &self.conn));
            return;
        }
        let Some(job) = self.job() else { return };
        let database = database_of([&job.mapping.target]);
        let Some(live) = cx.conns.live(&self.conn) else { return };
        let factory: Arc<dyn SessionFactory> = Arc::new(AppSessionFactory::from_live(live, database.as_deref()));
        self.error = None;
        self.result = None;
        self.log.clear();
        self.page = PAGE_RUN;
        let title = trf("Import into {}", &[&job.mapping.target.display_path()]);
        self.run = Some(Running::spawn(cx, title, move |progress, cancel| run_import_job(job, factory, progress, cancel)));
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub fn cancel(&self) {
        if let Some(r) = &self.run {
            r.cancel();
        }
    }

    /// Summary of the last finished run.
    pub fn last_summary(&self) -> Option<&ImportSummary> {
        self.result.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// Error of the last run (or of the last attempt to start one).
    pub fn last_error(&self) -> Option<&str> {
        match &self.result {
            Some(Err(e)) => Some(e),
            _ => self.error.as_deref().or(self.mapping_error.as_deref()),
        }
    }

    /// Save the current configuration as a task (in the background).
    pub fn save_task(&mut self, cx: &mut AppContext, name: &str, folder: Option<String>) {
        match self.task_kind() {
            Ok(kind) => self.saver.save(cx, name.to_string(), folder, kind),
            Err(e) => self.saver.fail(e.full_text()),
        }
    }

    pub fn saved_task_id(&self) -> Option<&str> {
        self.saver.saved.as_deref()
    }

    // --------------------------------------------------------------- helpers

    fn task_kind(&self) -> DbResult<TaskKind> {
        self.validate().map_err(DbError::config)?;
        let job = self.job().ok_or_else(|| DbError::config(tr("Choose the target table.")))?;
        Ok(TaskKind::Import { connection_id: self.conn.clone(), config: import_task_config(&job)? })
    }

    fn source_config(&self) -> ImportSourceConfig {
        let path = PathBuf::from(self.path.trim());
        match self.format {
            ImportFileFormat::Csv => ImportSourceConfig::Csv { path, options: self.csv.clone() },
            ImportFileFormat::Tsv => {
                let mut options = self.csv.clone();
                options.delimiter = "\t".into();
                ImportSourceConfig::Csv { path, options }
            }
            ImportFileFormat::Xlsx => ImportSourceConfig::Xlsx { path, options: self.xlsx.clone() },
            ImportFileFormat::Xml => ImportSourceConfig::Xml { path, options: self.xml.clone() },
            ImportFileFormat::Json => ImportSourceConfig::Json { path, options: self.json.clone() },
        }
    }

    fn infer_options(&self) -> InferOptions {
        let c = &self.options.convert;
        InferOptions {
            date_formats: c.date_formats.clone(),
            datetime_formats: c.datetime_formats.clone(),
            decimal_separator: c.decimal_separator,
            thousands_separator: c.thousands_separator,
            null_values: c.null_values.clone(),
            ..self.infer.clone()
        }
    }

    fn target_ref(&self) -> Option<ObjectRef> {
        match self.target {
            ImportTarget::Existing => self.table.clone(),
            ImportTarget::New => {
                let name = self.new_name.trim();
                if name.is_empty() {
                    return None;
                }
                let schema = self.schema.clone().unwrap_or_default();
                Some(ObjectRef::table(schema.schema, name).with_database(schema.database))
            }
        }
    }

    fn current_key(&self) -> Option<MappingKey> {
        let preview = self.preview.as_ref()?;
        let columns: Vec<String> = preview.columns.iter().map(|c| c.name.clone()).collect();
        Some(match self.target {
            ImportTarget::New => MappingKey::New { columns, infer: self.infer_options() },
            ImportTarget::Existing => MappingKey::Existing { table: self.table.clone()?, columns },
        })
    }

    fn file_error(&self) -> Option<String> {
        if self.path.trim().is_empty() {
            return Some(tr("Choose the file to import.").to_string());
        }
        if let Some((config, e)) = &self.preview_error
            && *config == self.source_config()
        {
            return Some(e.clone());
        }
        if !self.preview_ready() {
            return Some(tr("Reading the file…").to_string());
        }
        None
    }

    fn target_error(&self) -> Option<String> {
        match self.target {
            ImportTarget::Existing if self.table.is_none() => return Some(tr("Choose the target table.").to_string()),
            ImportTarget::New if self.new_name.trim().is_empty() => {
                return Some(tr("Enter the name of the new table.").to_string());
            }
            _ => {}
        }
        if let Some(e) = &self.mapping_error {
            return Some(e.clone());
        }
        if self.mapping_for != self.current_key() {
            return Some(tr("Preparing the column mapping…").to_string());
        }
        let active: Vec<&ColumnMapping> = self.columns.iter().filter(|c| !c.skip).collect();
        if active.is_empty() {
            return Some(tr("No column is imported.").to_string());
        }
        if let Some(c) = active.iter().find(|c| c.target_name.trim().is_empty()) {
            let source = self.preview_columns().get(c.source_index).cloned().unwrap_or_default();
            return Some(trf("The source column {} has no target column.", &[&source]));
        }
        for (i, a) in active.iter().enumerate() {
            if active[..i].iter().any(|b| b.target_name.eq_ignore_ascii_case(&a.target_name)) {
                return Some(trf("The target column {} is used twice.", &[&a.target_name]));
            }
        }
        if self.options.mode == ImportMode::Replace && self.key_columns.is_empty() {
            return Some(tr("Insert or replace needs key columns.").to_string());
        }
        None
    }

    fn validate(&self) -> Result<(), String> {
        match self.file_error().or_else(|| self.target_error()) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn reachable(&self) -> usize {
        if self.file_error().is_some() {
            PAGE_FILE
        } else if self.target_error().is_some() {
            PAGE_TARGET
        } else {
            PAGE_RUN
        }
    }

    fn poll(&mut self, cx: &mut AppContext) {
        // Preview.
        if let Some(job) = &mut self.preview_job
            && let Some(result) = job.poll()
        {
            self.preview_job = None;
            match result {
                Ok(p) => {
                    self.preview_error = None;
                    self.preview = Some(p);
                }
                Err((config, e)) => self.preview_error = Some((config, e)),
            }
        }
        let config = self.source_config();
        let current = self.preview.as_ref().is_some_and(|p| p.config == config);
        let failed = self.preview_error.as_ref().is_some_and(|(c, _)| *c == config);
        if self.preview_job.is_none() && !current && !failed && !self.path.trim().is_empty() {
            self.request_preview(cx, config);
        }
        // Sheet names.
        if self.format == ImportFileFormat::Xlsx {
            if let Some((path, job)) = &mut self.sheets_job
                && let Some(result) = job.poll()
            {
                let path = path.clone();
                self.sheets_job = None;
                self.sheets = Some((path, result.unwrap_or_default()));
            }
            let path = self.path.trim().to_string();
            let known = self.sheets.as_ref().is_some_and(|(p, _)| *p == path);
            if !known && self.sheets_job.is_none() && !path.is_empty() {
                let p = path.clone();
                let job = cx.backend.spawn(async move { tokio::task::spawn_blocking(move || xlsx_sheet_names(&p)).await.map_err(|e| DbError::internal(e.to_string()))? });
                self.sheets_job = Some((path, job));
            }
        }
        self.ensure_mapping(cx);
        // Run.
        let mut finished = None;
        if let Some(run) = &mut self.run {
            self.log.drain(&run.progress);
            if let Some(result) = run.poll(cx, describe) {
                self.log.drain(&run.progress);
                finished = Some(result);
            }
        }
        if let Some(result) = finished {
            self.run = None;
            match result {
                Ok(summary) => {
                    if summary.rows_failed > 0 {
                        cx.toasts.warning(describe(&summary), summary.errors.first().map(|e| e.message.clone()));
                    } else {
                        cx.toasts.success(describe(&summary));
                    }
                    self.refresh_metadata(cx, summary.created_table);
                    self.result = Some(Ok(summary));
                }
                Err(e) => {
                    if e.kind != dbiji_core::ErrorKind::Cancelled {
                        cx.toasts.error(tr("Import failed"), Some(e.full_text()));
                    }
                    self.result = Some(Err(error_text(&e)));
                }
            }
        }
        // Failed rows report.
        if let Some(job) = &mut self.report_job
            && let Some(result) = job.poll()
        {
            self.report_job = None;
            match result {
                Ok(path) => cx.toasts.success(trf("Report written to {}", &[&path.display()])),
                Err(e) => cx.toasts.error(tr("Cannot write the file"), Some(e.to_string())),
            }
        }
        self.saver.poll(cx);
    }

    fn request_preview(&mut self, cx: &mut AppContext, config: ImportSourceConfig) {
        let sample = self.sample_rows.clamp(10, 100_000);
        self.preview_job = Some(cx.backend.spawn(async move {
            let read = async {
                let source = config.open().await?;
                let mut source = PeekableSource::new(source);
                let rows = source.peek(sample).await?.to_vec();
                let columns = source.columns().to_vec();
                let warnings = source.take_warnings();
                let _ = source.close().await;
                Ok::<_, DbError>((columns, rows, warnings))
            };
            match read.await {
                Ok((columns, rows, warnings)) => Ok(Preview { config, columns, rows, warnings }),
                Err(e) => Err((config, e.full_text())),
            }
        }));
    }

    /// Build the column mapping for the current preview and target.
    fn ensure_mapping(&mut self, cx: &mut AppContext) {
        let Some(key) = self.current_key() else { return };
        if self.mapping_for.as_ref() == Some(&key) {
            return;
        }
        let Some(preview) = &self.preview else { return };
        match &key {
            MappingKey::New { columns, infer } => {
                let same_columns = matches!(&self.mapping_for, Some(MappingKey::New { columns: c, .. }) if c == columns);
                if self.columns_edited && same_columns {
                    // Keep the user's names and types; "Guess types again" rebuilds.
                    self.mapping_for = Some(key);
                    return;
                }
                let Some(live) = cx.conns.live(&self.conn) else { return };
                let inferred = infer_columns(&preview.columns, &preview.rows, infer);
                let target = self.target_ref().unwrap_or_else(|| ObjectRef::table(None, "import"));
                let m = ImportMapping::from_inferred(target, &inferred, live.dialect.as_ref());
                self.columns = m.columns;
                self.key_columns.clear();
            }
            MappingKey::Existing { table, .. } => {
                let table = table.clone();
                let conn = self.conn.clone();
                match table_def_of(cx, &conn, &table) {
                    None => return,
                    Some(Err(e)) => {
                        self.mapping_error = Some(trf("Cannot read the table definition: {}", &[&e.full_text()]));
                        return;
                    }
                    Some(Ok(def)) => {
                        let Some(preview) = &self.preview else { return };
                        let m = ImportMapping::match_table(&preview.columns, &def);
                        self.columns = m.columns;
                        self.key_columns = m.key_columns;
                    }
                }
            }
        }
        self.mapping_error = None;
        self.columns_edited = false;
        self.mapping_for = Some(key);
    }

    fn refresh_metadata(&self, cx: &mut AppContext, created: bool) {
        let Some(target) = self.target_ref() else { return };
        let schema = match &target.schema {
            Some(_) => target.schema_ref(),
            None => current_schema(cx, &self.conn).unwrap_or_default(),
        };
        let cmd = if created {
            AppCommand::RefreshMetadata { connection: self.conn.clone(), object: None, schema: Some(schema) }
        } else {
            AppCommand::RefreshMetadata { connection: self.conn.clone(), object: Some(target), schema: None }
        };
        cx.push(cmd);
    }

    // ------------------------------------------------------------------ pages

    fn file_page(&mut self, ui: &mut Ui) {
        section(ui, tr("File"));
        let id = self.uid;
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.path).hint_text(tr("path of the file")).desired_width(420.0));
            if r.changed()
                && let Some(f) = ImportFileFormat::from_path(Path::new(self.path.trim()))
            {
                self.format = f;
            }
            if ui.button((dbiji_icons::ic(icon::FOLDER_OPEN), tr("Browse…"))).clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .set_title(tr("File to import"))
                    .add_filter(tr("Data files"), &["csv", "tsv", "txt", "tab", "xlsx", "xlsm", "xls", "ods", "xml", "json", "jsonl", "ndjson"])
                    .add_filter(tr("All files"), &["*"])
                    .pick_file()
            {
                self.set_file(p);
            }
        });
        ui.horizontal(|ui| {
            ui.label(tr("Format"));
            for f in ImportFileFormat::ALL {
                ui.selectable_value(&mut self.format, f, f.label());
            }
        });
        ui.separator();
        egui::Grid::new(("imp_reader", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| match self.format {
            ImportFileFormat::Csv | ImportFileFormat::Tsv => csv_reader_ui(ui, id, &mut self.csv, self.format == ImportFileFormat::Tsv),
            ImportFileFormat::Xlsx => {
                let sheets = match &self.sheets {
                    Some((p, names)) if *p == self.path.trim() => names.clone(),
                    _ => Vec::new(),
                };
                xlsx_reader_ui(ui, id, &mut self.xlsx, &sheets);
            }
            ImportFileFormat::Xml => xml_reader_ui(ui, id, &mut self.xml),
            ImportFileFormat::Json => json_reader_ui(ui, id, &mut self.json),
        });
        ui.horizontal(|ui| {
            ui.label(tr("Rows read for the preview and the type guess"));
            ui.add(egui::DragValue::new(&mut self.sample_rows).range(10..=100_000));
        });

        section(ui, tr("Preview"));
        if self.path.trim().is_empty() {
            ui.weak(tr("Choose a file to see its first rows."));
            return;
        }
        if let Some((config, e)) = &self.preview_error
            && *config == self.source_config()
        {
            crate::widgets::error_box(ui, e);
            if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Retry"))).clicked() {
                self.preview_error = None;
            }
            return;
        }
        if self.preview_job.is_some() || !self.preview_ready() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Reading the file…"));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let Some(p) = &self.preview else { return };
        let more = if p.rows.len() >= self.sample_rows { "+" } else { "" };
        ui.weak(trf("{} columns, {}{} rows read", &[&p.columns.len(), &p.rows.len(), &more]));
        for w in p.warnings.iter().take(5) {
            ui.label(RichText::new(format!("{} {w}", icon::WARNING)).color(crate::theme::warning_color(ui.visuals().dark_mode)));
        }
        let headers: Vec<String> = p.columns.iter().map(|c| c.name.clone()).collect();
        let rows: Vec<Vec<String>> = p.rows.iter().take(PREVIEW_ROWS).map(|r| r.iter().map(preview_text).collect()).collect();
        text_table(ui, ("imp_preview", id), &headers, &rows, 260.0);
    }

    fn target_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let id = self.uid;
        let conn = self.conn.clone();
        section(ui, tr("Target table"));
        if cx.conns.live(&conn).is_none() {
            ui.horizontal(|ui| {
                ui.weak(tr("Connect to choose the target table."));
                if ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked() {
                    cx.push(AppCommand::Connect(conn.clone()));
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.target, ImportTarget::Existing, tr("Existing table"));
            ui.radio_value(&mut self.target, ImportTarget::New, tr("New table"));
        });
        egui::Grid::new(("imp_target", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Schema"));
            if schema_combo(ui, cx, &conn, ("imp_schema", id), &mut self.schema) && self.target == ImportTarget::Existing {
                self.table = None;
            }
            ui.end_row();
            match self.target {
                ImportTarget::Existing => {
                    ui.label(tr("Table"));
                    self.table_combo(ui, cx);
                    ui.end_row();
                }
                ImportTarget::New => {
                    ui.label(tr("Name"));
                    ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text(tr("new_table")).desired_width(240.0));
                    ui.end_row();
                }
            }
        });
        if let Some(e) = &self.mapping_error {
            crate::widgets::error_box(ui, e);
            if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Retry"))).clicked()
                && let Some(t) = self.table.clone()
            {
                reload_table_def(cx, &conn, &t);
                self.mapping_error = None;
                self.mapping_for = None;
            }
        }
        section(ui, tr("Columns"));
        if self.preview.is_none() {
            ui.weak(tr("The columns appear once the file has been read."));
            return;
        }
        if self.mapping_for != self.current_key() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Preparing the column mapping…"));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        match self.target {
            ImportTarget::New => self.new_columns_ui(ui, cx),
            ImportTarget::Existing => self.existing_columns_ui(ui, cx),
        }
    }

    fn table_combo(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let conn = self.conn.clone();
        let Some(schema) = self.schema.clone().or_else(|| current_schema(cx, &conn)) else { return };
        let tables = schema_tables(cx, &conn, &schema, false);
        let label = self.table.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| tr("Choose…").to_string());
        egui::ComboBox::from_id_salt(("imp_table", self.uid)).selected_text(label).width(240.0).height(360.0).show_ui(ui, |ui| match &tables {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Loading…"));
                });
            }
            Some(Err(e)) => {
                ui.label(e.full_text());
            }
            Some(Ok(list)) => {
                for o in list.iter().filter(|o| !o.is_system) {
                    let selected = self.table.as_ref() == Some(&o.obj);
                    if ui.selectable_label(selected, &o.obj.name).clicked() && !selected {
                        self.table = Some(o.obj.clone());
                    }
                }
            }
        });
    }

    fn sample_values(&self, index: usize) -> String {
        let Some(p) = &self.preview else { return String::new() };
        p.rows.iter().filter_map(|r| r.get(index)).filter(|v| !v.is_null()).take(3).map(preview_text).collect::<Vec<_>>().join(", ")
    }

    fn new_columns_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let id = self.uid;
        let dialect = cx.conns.live(&self.conn).map(|l| l.dialect.clone());
        let names = self.preview_columns();
        ui.horizontal(|ui| {
            if ui.button((dbiji_icons::ic(icon::MAGIC_WAND), tr("Guess types again"))).clicked() {
                self.columns_edited = false;
                self.mapping_for = None;
            }
            ui.weak(tr("Key columns become the primary key of the new table."));
        });
        let samples: Vec<String> = (0..self.columns.len()).map(|i| self.sample_values(self.columns[i].source_index)).collect();
        let mut edited = false;
        egui::ScrollArea::horizontal().id_salt(("imp_cols_h", id)).show(ui, |ui| {
            egui::Grid::new(("imp_cols", id)).num_columns(7).spacing([10.0, 4.0]).striped(true).show(ui, |ui| {
                ui.strong(tr("Import"));
                ui.strong(tr("Source column"));
                ui.strong(tr("Target column"));
                ui.strong(tr("Type"));
                ui.strong(tr("NULL"));
                ui.strong(tr("Key"));
                ui.strong(tr("Sample values"));
                ui.end_row();
                for (i, c) in self.columns.iter_mut().enumerate() {
                    let mut import = !c.skip;
                    if ui.checkbox(&mut import, "").changed() {
                        c.skip = !import;
                        edited = true;
                    }
                    ui.label(names.get(c.source_index).cloned().unwrap_or_default());
                    let old_name = c.target_name.clone();
                    if ui.add(egui::TextEdit::singleline(&mut c.target_name).desired_width(160.0)).changed() {
                        edited = true;
                        if let Some(k) = self.key_columns.iter_mut().find(|k| **k == old_name) {
                            *k = c.target_name.clone();
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.add(egui::TextEdit::singleline(&mut c.target_type).desired_width(150.0)).changed() {
                            c.target_kind = None;
                            edited = true;
                        }
                        if let Some(d) = &dialect {
                            ui.menu_button(icon::CARET_DOWN, |ui| {
                                for kind in COMMON_KINDS {
                                    let t = d.ddl().default_type_for(kind);
                                    if ui.button(format!("{t}  ({})", kind_label(kind))).clicked() {
                                        c.target_type = t;
                                        c.target_kind = Some(kind);
                                        edited = true;
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                    if ui.checkbox(&mut c.nullable, "").changed() {
                        edited = true;
                    }
                    let mut key = self.key_columns.iter().any(|k| k.eq_ignore_ascii_case(&c.target_name));
                    if ui.checkbox(&mut key, "").changed() {
                        edited = true;
                        if key {
                            self.key_columns.push(c.target_name.clone());
                        } else {
                            self.key_columns.retain(|k| !k.eq_ignore_ascii_case(&c.target_name));
                        }
                    }
                    ui.weak(samples.get(i).cloned().unwrap_or_default());
                    ui.end_row();
                }
            });
        });
        if edited {
            self.columns_edited = true;
        }
    }

    fn existing_columns_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let id = self.uid;
        let Some(table) = self.table.clone() else { return };
        let conn = self.conn.clone();
        let Some(Ok(def)) = table_def_of(cx, &conn, &table) else { return };
        let names = self.preview_columns();
        ui.horizontal(|ui| {
            if ui.button((dbiji_icons::ic(icon::ARROWS_CLOCKWISE), tr("Match by name again"))).clicked() {
                self.mapping_for = None;
            }
            if !self.key_columns.is_empty() {
                ui.weak(trf("Key: {}", &[&self.key_columns.join(", ")]));
            }
        });
        let samples: Vec<String> = (0..self.columns.len()).map(|i| self.sample_values(self.columns[i].source_index)).collect();
        egui::Grid::new(("imp_map", id)).num_columns(4).spacing([10.0, 4.0]).striped(true).show(ui, |ui| {
            ui.strong(tr("Source column"));
            ui.strong(tr("Target column"));
            ui.strong(tr("Type"));
            ui.strong(tr("Sample values"));
            ui.end_row();
            for (i, c) in self.columns.iter_mut().enumerate() {
                ui.label(names.get(c.source_index).cloned().unwrap_or_default());
                let label = if c.skip { tr("(not imported)").to_string() } else { c.target_name.clone() };
                egui::ComboBox::from_id_salt(("imp_map_col", id, i)).selected_text(label).width(200.0).show_ui(ui, |ui| {
                    if ui.selectable_label(c.skip, tr("(not imported)")).clicked() {
                        c.skip = true;
                    }
                    for col in &def.columns {
                        let selected = !c.skip && c.target_name == col.name;
                        if ui.selectable_label(selected, &col.name).clicked() {
                            c.skip = col.generated.is_some();
                            c.target_name = col.name.clone();
                            c.target_type = col.data_type.clone();
                            c.target_kind = Some(col.kind);
                            c.nullable = col.nullable;
                        }
                    }
                });
                ui.weak(if c.skip { String::new() } else { c.target_type.clone() });
                ui.weak(samples.get(i).cloned().unwrap_or_default());
                ui.end_row();
            }
        });
        let unmapped: Vec<&str> =
            def.columns.iter().filter(|col| col.generated.is_none() && !self.columns.iter().any(|c| !c.skip && c.target_name == col.name)).map(|col| col.name.as_str()).collect();
        if !unmapped.is_empty() {
            ui.weak(trf("Table columns without a source (default value or NULL): {}", &[&unmapped.join(", ")]));
        }
    }

    fn options_page(&mut self, ui: &mut Ui) {
        let id = self.uid;
        section(ui, tr("Writing"));
        let o = &mut self.options;
        egui::Grid::new(("imp_opts", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Existing rows"));
            combo(
                ui,
                ("imp_mode", id),
                &mut o.mode,
                &[
                    (ImportMode::Append, tr("Keep them and add the new rows").to_string()),
                    (ImportMode::TruncateFirst, tr("Delete them first").to_string()),
                    (ImportMode::Replace, tr("Insert or replace rows by key").to_string()),
                ],
            );
            ui.end_row();
            if o.mode == ImportMode::Replace {
                ui.label(tr("Key columns"));
                if self.key_columns.is_empty() {
                    ui.label(RichText::new(tr("None: mark key columns on the Target page.")).color(crate::theme::warning_color(ui.visuals().dark_mode)));
                } else {
                    ui.label(self.key_columns.join(", "));
                }
                ui.end_row();
            }
            ui.label(tr("Rows per batch"));
            ui.add(egui::DragValue::new(&mut o.batch_size).range(1..=1_000_000));
            ui.end_row();
            ui.label(tr("Rows per INSERT"));
            ui.add(egui::DragValue::new(&mut o.rows_per_statement).range(1..=1000));
            ui.end_row();
            ui.label(tr("Transactions"));
            combo(ui, ("imp_tx", id), &mut o.transaction, &transaction_options());
            ui.end_row();
            ui.label(tr("On error"));
            combo(ui, ("imp_err", id), &mut o.on_error, &on_error_options());
            ui.end_row();
            if o.on_error != OnError::Abort {
                ui.label(tr("Stop after"));
                ui.horizontal(|ui| {
                    opt_number(ui, &mut o.max_errors, tr("no limit"));
                    ui.label(tr("failed rows"));
                });
                ui.end_row();
            }
        });

        section(ui, tr("Reading values"));
        let c = &mut o.convert;
        egui::Grid::new(("imp_conv", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Date formats"));
            lines_edit(ui, &mut c.date_formats, tr("one per line, e.g. %d/%m/%Y"), 260.0);
            ui.end_row();
            ui.label(tr("Timestamp formats"));
            lines_edit(ui, &mut c.datetime_formats, tr("one per line, e.g. %d/%m/%Y %H:%M"), 260.0);
            ui.end_row();
            ui.label(tr("Time formats"));
            lines_edit(ui, &mut c.time_formats, tr("one per line, e.g. %H.%M"), 260.0);
            ui.end_row();
            ui.label(tr("Decimal separator"));
            char_edit(ui, &mut c.decimal_separator);
            ui.end_row();
            ui.label(tr("Thousands separator"));
            opt_char_edit(ui, &mut c.thousands_separator, "");
            ui.end_row();
            ui.label(tr("NULL markers"));
            lines_edit(ui, &mut c.null_values, tr("one per line, e.g. NULL or \\N"), 260.0);
            ui.end_row();
            ui.label(tr("Binary values"));
            combo(
                ui,
                ("imp_bin", id),
                &mut c.binary,
                &[
                    (None, tr("Detect").to_string()),
                    (Some(BinaryEncoding::Hex), tr("Hexadecimal").to_string()),
                    (Some(BinaryEncoding::Base64), tr("Base64").to_string()),
                    (Some(BinaryEncoding::Text), tr("As text (UTF-8)").to_string()),
                ],
            );
            ui.end_row();
            ui.label(tr("Text"));
            ui.horizontal(|ui| {
                ui.checkbox(&mut c.trim_text, tr("Trim spaces"));
                ui.checkbox(&mut c.empty_text_as_null, tr("Empty text is NULL"));
            });
            ui.end_row();
            ui.label(tr("Time zone of timestamps"));
            ui.horizontal(|ui| {
                opt_number(ui, &mut c.default_offset_minutes, tr("local"));
                ui.weak(tr("minutes east of UTC, for timestamps without an offset"));
            });
            ui.end_row();
        });

        section(ui, tr("Type guessing (new tables)"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.infer.detect_bool, tr("Booleans"));
            ui.checkbox(&mut self.infer.detect_json, tr("JSON"));
            ui.checkbox(&mut self.infer.detect_uuid, tr("UUIDs"));
        });
    }

    fn run_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if let Some(run) = &self.run {
            section(ui, tr("Importing…"));
            if progress_ui(ui, &run.progress, run.is_cancelling()) {
                run.cancel();
            }
            ui.add_space(6.0);
            self.log.ui(ui, ("imp_log", self.uid), 260.0);
            return;
        }
        match &self.result {
            Some(Ok(summary)) => {
                let summary = summary.clone();
                self.result_ui(ui, cx, &summary);
            }
            Some(Err(e)) => {
                section(ui, tr("The import failed"));
                crate::widgets::error_box(ui, e);
                if ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Change settings"))).clicked() {
                    self.result = None;
                    self.page = PAGE_FILE;
                }
            }
            None => self.summary_ui(ui, cx),
        }
        if self.result.is_some() {
            ui.add_space(6.0);
            self.log.ui(ui, ("imp_log", self.uid), 220.0);
        }
    }

    fn summary_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Summary"));
        let mapped = self.columns.iter().filter(|c| !c.skip).count();
        egui::Grid::new(("imp_sum", self.uid)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.weak(tr("File"));
            ui.label(self.path.trim());
            ui.end_row();
            ui.weak(tr("Format"));
            ui.label(self.format.label());
            ui.end_row();
            ui.weak(tr("Connection"));
            ui.label(connection_name(cx, &self.conn));
            ui.end_row();
            ui.weak(tr("Target table"));
            let target = self.target_ref().map(|t| t.display_path()).unwrap_or_default();
            ui.label(if self.target == ImportTarget::New { trf("{} (new)", &[&target]) } else { target });
            ui.end_row();
            ui.weak(tr("Columns"));
            ui.label(trf("{} of {}", &[&mapped, &self.columns.len()]));
            ui.end_row();
        });
        if let Err(e) = self.validate() {
            ui.add_space(4.0);
            crate::widgets::error_box(ui, &e);
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, cx: &mut AppContext, s: &ImportSummary) {
        let dark = ui.visuals().dark_mode;
        section(ui, &describe(s));
        egui::Grid::new(("imp_res", self.uid)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.weak(tr("Rows read"));
            ui.label(s.rows_read.to_string());
            ui.end_row();
            ui.weak(tr("Rows written"));
            ui.label(RichText::new(s.rows_written.to_string()).color(crate::theme::success_color(dark)));
            ui.end_row();
            ui.weak(tr("Rows failed"));
            let failed = RichText::new(s.rows_failed.to_string());
            ui.label(if s.rows_failed > 0 { failed.color(crate::theme::error_color(dark)) } else { failed });
            ui.end_row();
            if s.created_table {
                ui.weak(tr("Table"));
                ui.label(tr("created"));
                ui.end_row();
            }
            ui.weak(tr("Statements"));
            ui.label(s.statements.to_string());
            ui.end_row();
            ui.weak(tr("Time"));
            ui.label(crate::widgets::format_duration(std::time::Duration::from_millis(s.duration_ms)));
            ui.end_row();
        });
        for w in &s.warnings {
            ui.label(RichText::new(format!("{} {w}", icon::WARNING)).color(crate::theme::warning_color(dark)));
        }
        if !s.errors.is_empty() {
            section(ui, tr("Failed rows"));
            if s.errors_truncated {
                ui.weak(trf("Only the first {} failed rows are listed.", &[&s.errors.len()]));
            }
            let headers = vec![tr("Row").to_string(), tr("Column").to_string(), tr("Error").to_string()];
            let rows: Vec<Vec<String>> = s.errors.iter().map(|e| vec![e.row.to_string(), e.column.clone().unwrap_or_default(), e.message.clone()]).collect();
            text_table(ui, ("imp_errors", self.uid), &headers, &rows, 240.0);
            if ui.add_enabled(self.report_job.is_none(), egui::Button::new((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save report…")))).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("import-errors.csv").save_file()
            {
                let text = error_report(s);
                self.report_job = Some(cx.backend.spawn(async move { tokio::fs::write(&path, text).await.map(|_| path) }));
            }
        }
        ui.horizontal(|ui| {
            if let Some(target) = self.target_ref()
                && ui.button((dbiji_icons::ic(icon::TABLE), tr("Open the table"))).clicked()
            {
                cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: target, page: crate::command::ObjectPage::Data });
            }
            if ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Change settings"))).clicked() {
                self.result = None;
                self.page = PAGE_FILE;
                // A created table exists now: import into it next time.
                if s.created_table
                    && self.target == ImportTarget::New
                    && let Some(t) = self.target_ref()
                {
                    self.table = Some(t);
                    self.target = ImportTarget::Existing;
                }
            }
        });
    }

    fn footer(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let running = self.run.is_some();
            if ui.add_enabled(self.page > PAGE_FILE && !running, egui::Button::new((dbiji_icons::ic(icon::CARET_LEFT), tr("Back")))).clicked() {
                self.page -= 1;
            }
            let reachable = self.reachable();
            if self.page < PAGE_RUN && ui.add_enabled(self.page < reachable && !running, egui::Button::new(format!("{} {}", tr("Next"), icon::CARET_RIGHT))).clicked() {
                self.page += 1;
            }
            ui.separator();
            let ok = reachable == PAGE_RUN && !running;
            if ui.add_enabled(ok, egui::Button::new((dbiji_icons::ic(icon::PLAY), tr("Run import")))).clicked() {
                self.start(cx);
            }
            if ui.add_enabled(ok && !self.saver.is_saving(), egui::Button::new((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save as task…")))).clicked() {
                let target = self.target_ref().map(|t| t.name).unwrap_or_default();
                self.saver.open(trf("Import {}", &[&target]));
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(crate::theme::error_color(ui.visuals().dark_mode)));
            }
        });
        ui.add_space(2.0);
    }
}

fn describe(s: &ImportSummary) -> String {
    if s.rows_failed > 0 { trf("{} rows imported, {} failed", &[&s.rows_written, &s.rows_failed]) } else { trf("{} rows imported", &[&s.rows_written]) }
}

/// Text of a preview cell (canonical text, NULL marked).
fn preview_text(v: &dbiji_core::Value) -> String {
    if v.is_null() { "NULL".to_string() } else { v.to_text().into_owned() }
}

/// CSV with the failed rows of an import.
fn error_report(s: &ImportSummary) -> String {
    let quote = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    let mut out = String::from("row,column,error\n");
    for e in &s.errors {
        out.push_str(&format!("{},{},{}\n", e.row, quote(e.column.as_deref().unwrap_or("")), quote(&e.message)));
    }
    out
}

/// A table name derived from a file name (`Sales 2024.csv` → `sales_2024`).
fn table_name_from(stem: &str) -> String {
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_string();
    match out.chars().next() {
        None => "import".to_string(),
        Some(c) if c.is_ascii_digit() => format!("t_{out}"),
        Some(_) => out,
    }
}

// ------------------------------------------------------------ reader options

fn encoding_combo(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut String) {
    egui::ComboBox::from_id_salt(id).selected_text(value.clone()).show_ui(ui, |ui| {
        for e in ENCODINGS {
            if ui.selectable_label(value.eq_ignore_ascii_case(e), e).clicked() {
                *value = e.to_string();
            }
        }
    });
}

fn csv_reader_ui(ui: &mut Ui, id: u64, o: &mut CsvReadOptions, tsv: bool) {
    if !tsv {
        ui.label(tr("Delimiter"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut o.delimiter).desired_width(60.0));
            for (d, label) in [(",", ","), (";", ";"), ("\t", "Tab"), ("|", "|")] {
                if ui.selectable_label(o.delimiter == d, tr(label)).clicked() {
                    o.delimiter = d.to_string();
                }
            }
        });
        ui.end_row();
    }
    ui.label(tr("Header"));
    combo(
        ui,
        ("csv_rd_header", id),
        &mut o.header,
        &[
            (CsvHeader::FirstRow, tr("The first row holds the column names").to_string()),
            (CsvHeader::None, tr("No header row").to_string()),
            (CsvHeader::Auto, tr("Detect").to_string()),
        ],
    );
    ui.end_row();
    ui.label(tr("Skip rows"));
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut o.skip_rows).range(0..=1_000_000));
        ui.weak(tr("before the header"));
    });
    ui.end_row();
    ui.label(tr("Quote character"));
    opt_char_edit(ui, &mut o.quote_char, tr("none"));
    ui.end_row();
    ui.label(tr("Quotes inside values"));
    combo(ui, ("csv_rd_escape", id), &mut o.escape, &[(EscapeStyle::Doubled, tr("Doubled (\"\")").to_string()), (EscapeStyle::Backslash, tr("Backslash (\\\")").to_string())]);
    ui.end_row();
    ui.label(tr("NULL text"));
    ui.add(egui::TextEdit::singleline(&mut o.null_string).hint_text(tr("empty")).desired_width(100.0));
    ui.end_row();
    ui.label(tr("Comment lines"));
    opt_char_edit(ui, &mut o.comment_char, tr("none"));
    ui.end_row();
    ui.label(tr("Spaces"));
    ui.checkbox(&mut o.trim, tr("Trim spaces around values"));
    ui.end_row();
    ui.label(tr("Encoding"));
    encoding_combo(ui, ("csv_rd_enc", id), &mut o.encoding);
    ui.end_row();
}

fn xlsx_reader_ui(ui: &mut Ui, id: u64, o: &mut XlsxReadOptions, sheets: &[String]) {
    ui.label(tr("Sheet"));
    let current = match &o.sheet {
        SheetSelector::Name(n) => n.clone(),
        SheetSelector::Index(i) => sheets.get(*i).cloned().unwrap_or_else(|| trf("Sheet {}", &[&(i + 1)])),
    };
    egui::ComboBox::from_id_salt(("xlsx_sheet", id)).selected_text(current.clone()).show_ui(ui, |ui| {
        if sheets.is_empty() {
            ui.weak(tr("Loading…"));
        }
        for s in sheets {
            if ui.selectable_label(*s == current, s).clicked() {
                o.sheet = SheetSelector::Name(s.clone());
            }
        }
    });
    ui.end_row();
    ui.label(tr("Header"));
    ui.checkbox(&mut o.header, tr("The first row holds the column names"));
    ui.end_row();
    ui.label(tr("Skip rows"));
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut o.skip_rows).range(0..=1_000_000));
        ui.weak(tr("before the header"));
    });
    ui.end_row();
    ui.label(tr("Empty rows"));
    ui.checkbox(&mut o.skip_empty_rows, tr("Ignore"));
    ui.end_row();
}

fn xml_reader_ui(ui: &mut Ui, id: u64, o: &mut XmlReadOptions) {
    ui.label(tr("Row element"));
    opt_text(ui, &mut o.record_element, tr("every child of the root element"), 220.0);
    ui.end_row();
    ui.label(tr("Attributes"));
    ui.checkbox(&mut o.attributes_as_columns, tr("Attributes of the row element are columns"));
    ui.end_row();
    ui.label(tr("Empty elements"));
    ui.checkbox(&mut o.empty_as_null, tr("Read as NULL"));
    ui.end_row();
    ui.label(tr("Encoding"));
    encoding_combo(ui, ("xml_rd_enc", id), &mut o.encoding);
    ui.end_row();
}

fn json_reader_ui(ui: &mut Ui, id: u64, o: &mut JsonReadOptions) {
    ui.label(tr("Structure"));
    combo(
        ui,
        ("json_rd_fmt", id),
        &mut o.format,
        &[
            (JsonInputFormat::Auto, tr("Detect").to_string()),
            (JsonInputFormat::Array, tr("Array of objects").to_string()),
            (JsonInputFormat::Lines, tr("One object per line").to_string()),
        ],
    );
    ui.end_row();
    ui.label(tr("Encoding"));
    encoding_combo(ui, ("json_rd_enc", id), &mut o.encoding);
    ui.end_row();
}

impl TabView for ImportTab {
    fn popup_size(&self) -> Option<egui::Vec2> {
        Some(egui::vec2(820.0, 600.0))
    }

    fn title(&self) -> String {
        match self.target_ref() {
            Some(t) => trf("Import into {}", &[&t.name]),
            None => tr("Import data").to_string(),
        }
    }

    fn icon(&self) -> &'static str {
        icon::DOWNLOAD
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll(cx);
        let uid = self.uid;
        egui::Panel::top(egui::Id::new(("imp_top", uid))).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                crate::widgets::icon_heading(ui, icon::DOWNLOAD, tr("Import data"));
                ui.weak(connection_name(cx, &self.conn));
            });
            let steps = [tr("File"), tr("Target"), tr("Options"), tr("Run")];
            let reachable = if self.run.is_some() { self.page } else { self.reachable() };
            if let Some(p) = steps_ui(ui, &steps, self.page, reachable.max(self.page))
                && self.run.is_none()
            {
                self.page = p;
            }
            ui.add_space(2.0);
        });
        egui::Panel::bottom(egui::Id::new(("imp_bottom", uid))).show(ui, |ui| self.footer(ui, cx));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("imp_body", uid)).auto_shrink([false, false]).show(ui, |ui| match self.page {
                PAGE_FILE => self.file_page(ui),
                PAGE_TARGET => self.target_page(ui, cx),
                PAGE_OPTIONS => self.options_page(ui),
                _ => self.run_page(ui, cx),
            });
        });
        if let Some((name, folder)) = self.saver.ui(ui.ctx(), egui::Id::new(("imp_save", uid))) {
            self.save_task(cx, &name, folder);
        }
        if self.preview_job.is_some() || self.sheets_job.is_some() || self.saver.is_saving() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        if let Some(run) = &mut self.run {
            run.abandon(cx);
        }
        true
    }

    crate::impl_tab_any!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_formats() {
        assert_eq!(table_name_from("Sales 2024"), "sales_2024");
        assert_eq!(table_name_from("2024-orders"), "t_2024_orders");
        assert_eq!(table_name_from("***"), "import");
        assert_eq!(ImportFileFormat::from_path(Path::new("a/b.JSONL")), Some(ImportFileFormat::Json));
        assert_eq!(ImportFileFormat::from_path(Path::new("a.tsv")), Some(ImportFileFormat::Tsv));
        assert_eq!(ImportFileFormat::from_path(Path::new("a.bin")), None);
    }

    #[test]
    fn mapping_follows_target() {
        let mut tab = ImportTab::new("c".into(), None, Some(SchemaRef::schema("public")));
        tab.set_file("/tmp/My Data.csv");
        assert_eq!(tab.new_name, "my_data");
        let m = tab.mapping().unwrap();
        assert!(m.create_table);
        assert_eq!(m.target, ObjectRef::table(Some("public".into()), "my_data"));
        tab.set_existing_table(ObjectRef::table(Some("s".into()), "t"));
        let m = tab.mapping().unwrap();
        assert!(!m.create_table);
        assert_eq!(m.target.name, "t");
        let report =
            error_report(&ImportSummary { errors: vec![dbiji_transfer::RowError { row: 3, column: Some("a".into()), message: "bad \"x\"".into() }], ..Default::default() });
        assert_eq!(report, "row,column,error\n3,\"a\",\"bad \"\"x\"\"\"\n");
    }
}
