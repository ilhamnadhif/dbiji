//! Export wizard (FR-T1): tables (with optional filters) or a query, every
//! export format with its options, output folder and file name pattern,
//! parallel workers; runs with progress and can be saved as a task.

use super::{
    MessageLog, Running, TaskSaver, char_edit, combo, connection_name, current_schema, default_output_folder, error_text, next_uid, not_connected, opt_char_edit, opt_number,
    opt_text, progress_ui, schema_combo, schema_tables, section, steps_ui,
};
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::transferexec::{AppSessionFactory, ExportTaskExtras, export_database, export_task_config};
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{ColumnMeta, DbError, DbKind, DbResult, Dialect, SharedRowStore, tr, trf};
use dbiji_tasks::TaskKind;
use dbiji_transfer::{
    BinaryEncoding, CancellationToken, CsvOptions, DbUnitOptions, EscapeStyle, ExportFormat, ExportItem, ExportItemSummary, ExportJob, ExportJobSummary, ExportOptions,
    ExportTarget, HeaderStyle, HtmlOptions, IdentifierQuoting, JsonLayout, JsonOptions, LineEnding, MarkdownOptions, ParquetCompression, ParquetOptions, PhpOptions, Progress,
    QuoteMode, RowStoreSource, SessionFactory, SqlInsertMode, SqlOptions, TxtOptions, ValueFormat, XlsxOptions, XmlNullMode, XmlOptions, create_sink, expand_file_name,
    run_export_batched, run_export_job_with,
};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::path::PathBuf;
use std::sync::Arc;

const PAGE_SOURCE: usize = 0;
const PAGE_FORMAT: usize = 1;
const PAGE_OUTPUT: usize = 2;
const PAGE_RUN: usize = 3;

/// Character encodings offered for text output (WHATWG labels).
const ENCODINGS: [&str; 14] =
    ["UTF-8", "UTF-16LE", "UTF-16BE", "windows-1252", "ISO-8859-1", "ISO-8859-15", "windows-1250", "windows-1251", "KOI8-R", "Shift_JIS", "EUC-JP", "GBK", "Big5", "EUC-KR"];

#[derive(Clone)]
struct SourceItem {
    enabled: bool,
    item: ExportItem,
}

/// Rows already fetched into a result grid ([`ExportTab::from_result`]).
struct Fetched {
    title: String,
    store: SharedRowStore,
    columns: Vec<ColumnMeta>,
    /// Selected rows (store indices).
    rows: Option<Vec<usize>>,
    /// Exported columns (indices into `columns`, in output order; empty = all).
    cols: Vec<usize>,
    sql: Option<String>,
}

/// What an export of a result grid writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSource {
    /// Every row fetched so far.
    Fetched,
    /// The rows selected in the grid.
    Selected,
    /// The query run again for all rows.
    Query,
}

/// Store indices of result grid display rows, for [`ExportTab::from_result`].
/// Display rows differ from store rows while a client-side sort or filter
/// is active; pending inserted rows are not in the store and are left out.
pub fn grid_store_rows(model: &dbiji_grid::ResultModel, display_rows: &[usize]) -> Vec<usize> {
    display_rows
        .iter()
        .filter_map(|&r| match model.row_key(r) {
            Some(dbiji_grid::model::RowKey::Stored(s)) => Some(s as usize),
            _ => None,
        })
        .collect()
}

/// The export wizard tab.
pub struct ExportTab {
    uid: u64,
    conn: String,
    page: usize,
    items: Vec<SourceItem>,
    /// Query item whose SQL editor is open.
    expanded: Option<usize>,
    picker_schema: Option<SchemaRef>,
    picker_filter: String,
    picker_views: bool,
    /// Format, options and output settings (the items live in `items`).
    settings: ExportJob,
    /// Product the SQL format writes for; `None` = the source's.
    sql_dialect: Option<DbKind>,
    run: Option<Running<ExportJobSummary>>,
    log: MessageLog,
    result: Option<Result<ExportJobSummary, String>>,
    error: Option<String>,
    saver: TaskSaver,
    /// Rows of a result grid (export without database access).
    fetched: Option<Fetched>,
    result_source: ResultSource,
}

impl ExportTab {
    /// Export `objects` (tables / views) of `conn`, or the result of `sql`
    /// (named after the single object when one is given).
    pub fn new(conn: String, objects: Vec<ObjectRef>, sql: Option<String>) -> Self {
        let mut items = Vec::new();
        let mut picker_schema = None;
        match sql.filter(|s| !s.trim().is_empty()) {
            Some(sql) => {
                let name = match objects.as_slice() {
                    [one] => Some(one.name.clone()),
                    _ => None,
                };
                items.push(SourceItem { enabled: true, item: ExportItem::Query { sql, name } });
            }
            None => {
                for o in objects {
                    if o.kind == ObjectKind::Schema {
                        picker_schema = Some(SchemaRef::new(o.database.clone(), Some(o.name.clone())));
                    } else if o.kind.has_data() {
                        if picker_schema.is_none() {
                            picker_schema = Some(o.schema_ref()).filter(|s| s.schema.is_some());
                        }
                        items.push(SourceItem { enabled: true, item: ExportItem::Table { table: o, where_clause: None } });
                    }
                }
            }
        }
        let settings = ExportJob { output_folder: default_output_folder(), ..Default::default() };
        ExportTab {
            uid: next_uid(),
            conn,
            page: PAGE_SOURCE,
            items,
            expanded: None,
            picker_schema,
            picker_filter: String::new(),
            picker_views: true,
            settings,
            sql_dialect: None,
            run: None,
            log: MessageLog::default(),
            result: None,
            error: None,
            saver: TaskSaver::default(),
            fetched: None,
            result_source: ResultSource::Fetched,
        }
    }

    /// Export the result shown in a grid (FR-T2): the rows already fetched
    /// into `store` (`columns` describes its rows), or only the `rows`
    /// selected in the grid (store indices, see [`grid_store_rows`]), limited
    /// to the columns `cols` (indices into `columns`, in output order; empty
    /// = all). Both run without database access. With `sql` the query can
    /// also be run again for all rows (then the export can be saved as a task).
    pub fn from_result(conn: String, title: String, store: SharedRowStore, columns: Vec<ColumnMeta>, rows: Option<Vec<usize>>, cols: Vec<usize>, sql: Option<String>) -> Self {
        let sql = sql.filter(|s| !s.trim().is_empty());
        let mut tab = Self::new(conn, Vec::new(), sql.clone());
        let name = Some(title.trim().to_string()).filter(|t| !t.is_empty());
        if let Some(SourceItem { item: ExportItem::Query { name: n, .. }, .. }) = tab.items.first_mut() {
            *n = name;
        }
        tab.result_source = if rows.is_some() { ResultSource::Selected } else { ResultSource::Fetched };
        tab.fetched = Some(Fetched { title, store, columns, rows, cols, sql });
        tab
    }

    /// What a result grid export writes (see [`ExportTab::from_result`]).
    pub fn set_result_source(&mut self, source: ResultSource) {
        self.result_source = source;
    }

    pub fn result_source(&self) -> ResultSource {
        self.result_source
    }

    // ------------------------------------------------------------ public API

    /// Format, options and output settings (everything but the objects).
    pub fn settings_mut(&mut self) -> &mut ExportJob {
        &mut self.settings
    }

    /// Database product of SQL output (`None`: the source's).
    pub fn set_sql_dialect(&mut self, kind: Option<DbKind>) {
        self.sql_dialect = kind;
    }

    /// The job as it would run now.
    pub fn job(&self) -> ExportJob {
        let mut job = self.settings.clone();
        job.items = self
            .items
            .iter()
            .filter(|i| i.enabled)
            .map(|i| match &i.item {
                ExportItem::Query { sql, name } => {
                    ExportItem::Query { sql: sql.trim().trim_end_matches(';').trim_end().to_string(), name: name.as_ref().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) }
                }
                ExportItem::Table { table, where_clause } => {
                    ExportItem::Table { table: table.clone(), where_clause: where_clause.as_ref().map(|w| w.trim().to_string()).filter(|w| !w.is_empty()) }
                }
            })
            .collect();
        job
    }

    /// Add a table or view.
    pub fn add_table(&mut self, table: ObjectRef) {
        let exists = self.items.iter().any(|i| matches!(&i.item, ExportItem::Table { table: t, .. } if *t == table));
        if !exists {
            self.items.push(SourceItem { enabled: true, item: ExportItem::Table { table, where_clause: None } });
        }
    }

    /// Wizard page (0 source, 1 format, 2 output, 3 run).
    pub fn page(&self) -> usize {
        self.page
    }

    pub fn set_page(&mut self, page: usize) {
        self.page = page.min(PAGE_RUN);
    }

    /// Run the export (the "Run" button).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.run.is_some() {
            return;
        }
        if let Err(e) = self.validate() {
            self.error = Some(e);
            return;
        }
        if self.fetched_mode() {
            self.start_fetched(cx);
            return;
        }
        if cx.conns.live(&self.conn).is_none() {
            self.error = Some(not_connected(cx, &self.conn));
            return;
        }
        let job = self.job();
        let database = export_database(&job);
        let Some(live) = cx.conns.live(&self.conn) else { return };
        let factory: Arc<dyn SessionFactory> = Arc::new(AppSessionFactory::from_live(live, database.as_deref()));
        let sql_dialect = if job.format == ExportFormat::Sql { self.sql_dialect.map(|k| cx.conns.registry.dialect(k)) } else { None };
        self.error = None;
        self.result = None;
        self.log.clear();
        self.page = PAGE_RUN;
        let title = self.title();
        self.run = Some(Running::spawn(cx, title, move |progress, cancel| run_export_job_with(job, factory, sql_dialect, progress, cancel)));
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// Cancel a running export.
    pub fn cancel(&self) {
        if let Some(r) = &self.run {
            r.cancel();
        }
    }

    /// Summary of the last finished run.
    pub fn last_summary(&self) -> Option<&ExportJobSummary> {
        self.result.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// Error of the last run (or of the last attempt to start one).
    pub fn last_error(&self) -> Option<&str> {
        match &self.result {
            Some(Err(e)) => Some(e),
            _ => self.error.as_deref(),
        }
    }

    /// Save the current configuration as a task (in the background).
    pub fn save_task(&mut self, cx: &mut AppContext, name: &str, folder: Option<String>) {
        match self.task_kind() {
            Ok(kind) => self.saver.save(cx, name.to_string(), folder, kind),
            Err(e) => self.saver.fail(e.full_text()),
        }
    }

    /// Id of the task saved last.
    pub fn saved_task_id(&self) -> Option<&str> {
        self.saver.saved.as_deref()
    }

    // --------------------------------------------------------------- helpers

    /// The rows of a result grid are exported (no database access).
    fn fetched_mode(&self) -> bool {
        self.fetched.is_some() && self.result_source != ResultSource::Query
    }

    /// Item naming the output file of a result grid export.
    fn fetched_item(&self) -> ExportItem {
        let name = self.fetched.as_ref().map(|f| f.title.trim().to_string()).filter(|t| !t.is_empty());
        ExportItem::Query { sql: String::new(), name }
    }

    fn fetched_rows(&self) -> usize {
        let Some(f) = &self.fetched else { return 0 };
        match (self.result_source, &f.rows) {
            (ResultSource::Selected, Some(rows)) => rows.len(),
            _ => f.store.lock().len(),
        }
    }

    fn start_fetched(&mut self, cx: &mut AppContext) {
        let Some(f) = &self.fetched else { return };
        let mut source = RowStoreSource::new(f.store.clone(), f.columns.clone());
        if self.result_source == ResultSource::Selected
            && let Some(rows) = &f.rows
        {
            source = source.with_rows(rows.clone());
        }
        if !f.cols.is_empty() {
            source = source.with_columns(f.cols.clone());
        }
        let kind = cx.conns.config(&self.conn).map(|c| c.kind).unwrap_or(DbKind::Generic);
        let mut options = self.settings.options.clone();
        if options.sql.source_kind.is_none() {
            options.sql.source_kind = Some(kind);
        }
        let format = self.settings.format;
        let dialect = cx.conns.registry.dialect(self.sql_dialect.unwrap_or(kind));
        let item = self.fetched_item();
        let file = expand_file_name(&self.settings.file_name_pattern, &item, 1, format, chrono::Local::now().naive_local());
        let path = self.settings.output_folder.join(file);
        let name = item.name();
        self.error = None;
        self.result = None;
        self.log.clear();
        self.page = PAGE_RUN;
        let title = self.title();
        self.run = Some(Running::spawn(cx, title, move |progress, cancel| export_fetched(source, name, format, options, path, dialect, progress, cancel)));
    }

    fn task_kind(&self) -> DbResult<TaskKind> {
        if self.fetched_mode() {
            return Err(DbError::config(tr("Rows fetched into the grid cannot be saved as a task; choose to run the query again instead.")));
        }
        self.validate().map_err(DbError::config)?;
        let job = self.job();
        let extras = ExportTaskExtras { sql_dialect: if job.format == ExportFormat::Sql { self.sql_dialect } else { None } };
        Ok(TaskKind::Export { connection_id: self.conn.clone(), config: export_task_config(&job, &extras)? })
    }

    fn items_error(&self) -> Option<String> {
        if self.fetched_mode() {
            return None;
        }
        let enabled: Vec<&SourceItem> = self.items.iter().filter(|i| i.enabled).collect();
        if enabled.is_empty() {
            return Some(tr("Choose at least one table, view or query to export.").to_string());
        }
        if enabled.iter().any(|i| matches!(&i.item, ExportItem::Query { sql, .. } if sql.trim().is_empty())) {
            return Some(tr("A query to export is empty.").to_string());
        }
        None
    }

    fn output_error(&self) -> Option<String> {
        if self.settings.output_folder.as_os_str().is_empty() {
            return Some(tr("Choose the output folder.").to_string());
        }
        if self.settings.format == ExportFormat::Csv && self.settings.options.csv.delimiter.is_empty() {
            return Some(tr("The CSV delimiter must not be empty.").to_string());
        }
        None
    }

    fn validate(&self) -> Result<(), String> {
        match self.items_error().or_else(|| self.output_error()) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn reachable(&self) -> usize {
        if self.items_error().is_some() {
            PAGE_SOURCE
        } else if self.output_error().is_some() {
            PAGE_OUTPUT
        } else {
            PAGE_RUN
        }
    }

    fn subject(&self) -> String {
        if let Some(f) = &self.fetched {
            return f.title.clone();
        }
        let enabled: Vec<&SourceItem> = self.items.iter().filter(|i| i.enabled).collect();
        match enabled.as_slice() {
            [] => String::new(),
            [one] => one.item.name(),
            many => trf("{} objects", &[&many.len()]),
        }
    }

    fn poll(&mut self, cx: &mut AppContext) {
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
                    if summary.failed() > 0 {
                        cx.toasts.warning(trf("Export finished, {} object(s) failed", &[&summary.failed()]), summary.first_error().map(str::to_string));
                    } else {
                        cx.toasts.success(describe(&summary));
                    }
                    self.result = Some(Ok(summary));
                }
                Err(e) => {
                    if e.kind != dbiji_core::ErrorKind::Cancelled {
                        cx.toasts.error(tr("Export failed"), Some(e.full_text()));
                    }
                    self.result = Some(Err(error_text(&e)));
                }
            }
        }
        self.saver.poll(cx);
    }

    // ------------------------------------------------------------------ pages

    fn result_source_page(&mut self, ui: &mut Ui) {
        let Some(f) = &self.fetched else { return };
        section(ui, tr("Rows to export"));
        let fetched = f.store.lock().len();
        let selected = f.rows.as_ref().map(Vec::len);
        let has_sql = f.sql.is_some();
        let columns = if f.cols.is_empty() { f.columns.len() } else { f.cols.len() };
        let sql = f.sql.clone().unwrap_or_default();
        ui.radio_value(&mut self.result_source, ResultSource::Fetched, trf("Fetched rows ({})", &[&fetched]));
        if let Some(n) = selected {
            ui.radio_value(&mut self.result_source, ResultSource::Selected, trf("Selected rows ({})", &[&n]));
        }
        if has_sql {
            ui.radio_value(&mut self.result_source, ResultSource::Query, tr("Run the query again (all rows)"));
        }
        ui.add_space(4.0);
        match self.result_source {
            ResultSource::Query => {
                ui.weak(tr("The query runs on a new connection and every row is exported, with all its columns."));
                if let Some(SourceItem { item: ExportItem::Query { name, .. }, .. }) = self.items.first_mut() {
                    ui.horizontal(|ui| {
                        ui.label(tr("Name"));
                        opt_text(ui, name, tr("name used for ${table}"), 200.0);
                    });
                }
                let mut text = sql.as_str();
                ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_rows(4).desired_width(f32::INFINITY));
            }
            _ => {
                ui.weak(trf("{} column(s); the rows are written from memory, without running the query again.", &[&columns]));
            }
        }
    }

    fn source_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.fetched.is_some() {
            self.result_source_page(ui);
            return;
        }
        section(ui, tr("Objects to export"));
        if self.items.is_empty() {
            ui.weak(tr("Nothing selected yet: add tables or views below, or a query."));
        }
        let mut remove = None;
        let mut toggle = None;
        egui::Grid::new(("exp_items", self.uid)).num_columns(4).spacing([8.0, 4.0]).striped(true).show(ui, |ui| {
            for (i, it) in self.items.iter_mut().enumerate() {
                ui.checkbox(&mut it.enabled, "");
                let name = it.item.name();
                match &mut it.item {
                    ExportItem::Table { table, where_clause } => {
                        let glyph = if table.kind == ObjectKind::View { icon::EYE } else { icon::TABLE };
                        ui.label(format!("{glyph} {}", table.display_path()));
                        ui.horizontal(|ui| {
                            ui.weak("WHERE");
                            opt_text(ui, where_clause, tr("optional filter, e.g. id > 100"), 280.0);
                        });
                    }
                    ExportItem::Query { name: qname, .. } => {
                        ui.label(format!("{} {name}", icon::FILE_SQL));
                        ui.horizontal(|ui| {
                            opt_text(ui, qname, tr("name used for ${table}"), 160.0);
                            let label = if self.expanded == Some(i) { tr("Hide SQL") } else { tr("Edit SQL") };
                            if ui.small_button(label).clicked() {
                                toggle = Some(i);
                            }
                        });
                    }
                }
                if ui.small_button(icon::TRASH).on_hover_text(tr("Remove")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = toggle {
            self.expanded = if self.expanded == Some(i) { None } else { Some(i) };
        }
        if let Some(i) = remove {
            self.items.remove(i);
            self.expanded = None;
        }
        if let Some(i) = self.expanded
            && let Some(SourceItem { item: ExportItem::Query { sql, .. }, .. }) = self.items.get_mut(i)
        {
            ui.add(egui::TextEdit::multiline(sql).code_editor().desired_rows(6).desired_width(f32::INFINITY).hint_text(tr("SELECT … (one statement returning rows)")));
        }
        ui.horizontal(|ui| {
            if ui.button(format!("{} {}", icon::PLUS, tr("Add query"))).clicked() {
                self.items.push(SourceItem { enabled: true, item: ExportItem::Query { sql: String::new(), name: None } });
                self.expanded = Some(self.items.len() - 1);
            }
            if !self.items.is_empty() {
                if ui.button(tr("Select all")).clicked() {
                    self.items.iter_mut().for_each(|i| i.enabled = true);
                }
                if ui.button(tr("Select none")).clicked() {
                    self.items.iter_mut().for_each(|i| i.enabled = false);
                }
            }
        });

        section(ui, tr("Add tables and views"));
        let conn = self.conn.clone();
        if cx.conns.live(&conn).is_none() {
            ui.horizontal(|ui| {
                ui.weak(tr("Connect to list the tables."));
                if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                    cx.push(crate::command::AppCommand::Connect(conn.clone()));
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label(tr("Schema"));
            schema_combo(ui, cx, &conn, ("exp_schema", self.uid), &mut self.picker_schema);
            ui.add(egui::TextEdit::singleline(&mut self.picker_filter).hint_text(tr("Filter…")).desired_width(160.0));
            ui.checkbox(&mut self.picker_views, tr("Views"));
        });
        let Some(schema) = self.picker_schema.clone().or_else(|| current_schema(cx, &conn)) else { return };
        match schema_tables(cx, &conn, &schema, self.picker_views) {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Loading…"));
                });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            Some(Err(e)) => crate::widgets::error_box(ui, &e.full_text()),
            Some(Ok(list)) => {
                let filter = self.picker_filter.to_lowercase();
                let shown: Vec<ObjectRef> =
                    list.iter().filter(|o| !o.is_system && (filter.is_empty() || o.obj.name.to_lowercase().contains(&filter))).map(|o| o.obj.clone()).collect();
                let added = |items: &[SourceItem], o: &ObjectRef| items.iter().any(|i| matches!(&i.item, ExportItem::Table { table, .. } if table == o));
                ui.horizontal(|ui| {
                    ui.weak(trf("{} objects", &[&shown.len()]));
                    if !shown.is_empty() && ui.button(tr("Add all shown")).clicked() {
                        for o in &shown {
                            self.add_table(o.clone());
                        }
                    }
                });
                egui::ScrollArea::vertical().id_salt(("exp_pick", self.uid)).max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
                    for o in &shown {
                        let already = added(&self.items, o);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!already, egui::Button::new(icon::PLUS).small()).clicked() {
                                self.add_table(o.clone());
                            }
                            let glyph = if o.kind == ObjectKind::View { icon::EYE } else { icon::TABLE };
                            let text = RichText::new(format!("{glyph} {}", o.name));
                            ui.label(if already { text.weak() } else { text });
                        });
                    }
                });
            }
        }
    }

    fn format_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Format"));
        ui.horizontal_wrapped(|ui| {
            for f in ExportFormat::ALL {
                ui.selectable_value(&mut self.settings.format, f, tr(f.label()));
            }
        });
        let format = self.settings.format;
        let id = self.uid;
        let o = &mut self.settings.options;
        ui.separator();
        match format {
            ExportFormat::Csv | ExportFormat::Tsv => csv_ui(ui, id, &mut o.csv, format == ExportFormat::Tsv),
            ExportFormat::Json | ExportFormat::JsonLines => json_ui(ui, id, &mut o.json, format == ExportFormat::Json),
            ExportFormat::Sql => {
                let source = cx.conns.config(&self.conn).map(|c| c.kind);
                let kinds = cx.conns.registry.kinds();
                sql_ui(ui, id, &mut o.sql, &mut self.sql_dialect, source, &kinds);
            }
            ExportFormat::Xlsx => xlsx_ui(ui, id, &mut o.xlsx),
            ExportFormat::Markdown => markdown_ui(ui, id, &mut o.markdown),
            ExportFormat::Html => html_ui(ui, id, &mut o.html),
            ExportFormat::Xml => xml_ui(ui, id, &mut o.xml),
            ExportFormat::Txt => txt_ui(ui, id, &mut o.txt),
            ExportFormat::DbUnit => dbunit_ui(ui, id, &mut o.dbunit),
            ExportFormat::Php => php_ui(ui, id, &mut o.php),
            ExportFormat::Parquet => parquet_ui(ui, id, &mut o.parquet),
        }
        if uses_value_format(format) {
            egui::CollapsingHeader::new(RichText::new(tr("Value formatting")).strong())
                .id_salt(("exp_values", id))
                .default_open(false)
                .show(ui, |ui| values_ui(ui, id, &mut o.values, format));
        }
        egui::CollapsingHeader::new(RichText::new(tr("File and reading")).strong())
            .id_salt(("exp_common", id))
            .default_open(!format.is_binary())
            .show(ui, |ui| common_ui(ui, id, o, format));
    }

    fn output_page(&mut self, ui: &mut Ui) {
        section(ui, tr("Output"));
        let id = self.uid;
        egui::Grid::new(("exp_out", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Folder"));
            ui.horizontal(|ui| {
                let mut folder = self.settings.output_folder.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut folder).desired_width(360.0)).changed() {
                    self.settings.output_folder = folder.into();
                }
                if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Browse…"))).clicked() {
                    let mut dialog = rfd::FileDialog::new().set_title(tr("Output folder"));
                    if self.settings.output_folder.is_dir() {
                        dialog = dialog.set_directory(&self.settings.output_folder);
                    }
                    if let Some(p) = dialog.pick_folder() {
                        self.settings.output_folder = p;
                    }
                }
            });
            ui.end_row();
            ui.label(tr("File name"));
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.settings.file_name_pattern).desired_width(360.0));
                ui.weak(tr("Variables: ${table} ${schema} ${database} ${timestamp} ${date} ${index} ${extension}"));
            });
            ui.end_row();
            ui.label(tr("Workers"));
            ui.horizontal(|ui| {
                ui.add_enabled(self.settings.open_new_connections, egui::DragValue::new(&mut self.settings.threads).range(1..=16));
                ui.weak(tr("objects exported in parallel"));
            });
            ui.end_row();
        });
        ui.checkbox(&mut self.settings.open_new_connections, tr("Open a separate connection for every worker"));
        ui.checkbox(&mut self.settings.continue_on_error, tr("Continue with the other objects when one fails"));
        ui.checkbox(&mut self.settings.count_rows, tr("Count the rows first (exact progress, one extra query per object)"));

        section(ui, tr("Files"));
        let now = chrono::Local::now().naive_local();
        if self.fetched_mode() {
            let name = expand_file_name(&self.settings.file_name_pattern, &self.fetched_item(), 1, self.settings.format, now);
            ui.monospace(self.settings.output_folder.join(name).display().to_string());
            return;
        }
        let job = self.job();
        if job.items.is_empty() {
            ui.weak(tr("No object selected."));
        }
        for (i, item) in job.items.iter().enumerate().take(50) {
            let name = expand_file_name(&job.file_name_pattern, item, i + 1, job.format, now);
            ui.monospace(job.output_folder.join(name).display().to_string());
        }
        if job.items.len() > 50 {
            ui.weak(trf("… and {} more", &[&(job.items.len() - 50)]));
        }
    }

    fn run_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if let Some(run) = &self.run {
            section(ui, tr("Exporting…"));
            if progress_ui(ui, &run.progress, run.is_cancelling()) {
                run.cancel();
            }
            ui.add_space(6.0);
            self.log.ui(ui, ("exp_log", self.uid), 260.0);
            return;
        }
        match &self.result {
            Some(Ok(summary)) => {
                let summary = summary.clone();
                self.result_ui(ui, &summary);
            }
            Some(Err(e)) => {
                section(ui, tr("The export failed"));
                crate::widgets::error_box(ui, e);
            }
            None => self.summary_ui(ui, cx),
        }
        if self.result.is_some() {
            ui.add_space(6.0);
            self.log.ui(ui, ("exp_log", self.uid), 220.0);
        }
    }

    fn summary_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Summary"));
        let job = self.job();
        egui::Grid::new(("exp_sum", self.uid)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.weak(tr("Connection"));
            ui.label(connection_name(cx, &self.conn));
            ui.end_row();
            ui.weak(tr("Objects"));
            if self.fetched_mode() {
                ui.label(trf("{} rows of {}", &[&self.fetched_rows(), &self.subject()]));
            } else {
                ui.label(job.items.iter().map(ExportItem::name).collect::<Vec<_>>().join(", "));
            }
            ui.end_row();
            ui.weak(tr("Format"));
            ui.label(tr(job.format.label()));
            ui.end_row();
            ui.weak(tr("Folder"));
            ui.label(job.output_folder.display().to_string());
            ui.end_row();
            ui.weak(tr("Workers"));
            ui.label(if job.open_new_connections { job.threads.to_string() } else { "1".to_string() });
            ui.end_row();
        });
        if let Some(e) = self.items_error().or_else(|| self.output_error()) {
            ui.add_space(4.0);
            crate::widgets::error_box(ui, &e);
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, summary: &ExportJobSummary) {
        let dark = ui.visuals().dark_mode;
        section(ui, &describe(summary));
        egui::Grid::new(("exp_res", self.uid)).num_columns(6).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
            ui.strong(tr("Object"));
            ui.strong(tr("File"));
            ui.strong(tr("Rows"));
            ui.strong(tr("Size"));
            ui.strong(tr("Time"));
            ui.strong(tr("Status"));
            ui.end_row();
            for item in &summary.items {
                ui.label(&item.name);
                match &item.file {
                    Some(f) => {
                        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if ui.link(name).on_hover_text(f.display().to_string()).clicked()
                            && let Err(e) = open::that_detached(f)
                        {
                            self.error = Some(e.to_string());
                        }
                    }
                    None => {
                        ui.weak("—");
                    }
                }
                ui.label(item.rows.to_string());
                ui.label(crate::widgets::format_bytes(item.bytes.min(i64::MAX as u64) as i64));
                ui.label(crate::widgets::format_duration(std::time::Duration::from_millis(item.duration_ms)));
                match &item.error {
                    Some(e) => {
                        ui.add(egui::Label::new(RichText::new(e).color(crate::theme::error_color(dark))).wrap());
                    }
                    None if !item.warnings.is_empty() => {
                        ui.label(RichText::new(trf("{} warning(s)", &[&item.warnings.len()])).color(crate::theme::warning_color(dark))).on_hover_text(item.warnings.join("\n"));
                    }
                    None => {
                        ui.label(RichText::new(tr("OK")).color(crate::theme::success_color(dark)));
                    }
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Open folder"))).clicked()
                && let Err(e) = open::that_detached(&self.settings.output_folder)
            {
                self.error = Some(e.to_string());
            }
            if ui.button(format!("{} {}", icon::ARROW_COUNTER_CLOCKWISE, tr("Change settings"))).clicked() {
                self.result = None;
                self.page = PAGE_SOURCE;
            }
        });
        if let Some(e) = &self.error {
            crate::widgets::error_box(ui, e);
        }
    }

    fn footer(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let running = self.run.is_some();
            if ui.add_enabled(self.page > PAGE_SOURCE && !running, egui::Button::new(format!("{} {}", icon::CARET_LEFT, tr("Back")))).clicked() {
                self.page -= 1;
            }
            let reachable = self.reachable();
            if self.page < PAGE_RUN && ui.add_enabled(self.page < reachable, egui::Button::new(format!("{} {}", tr("Next"), icon::CARET_RIGHT))).clicked() {
                self.page += 1;
            }
            ui.separator();
            let ok = self.validate().is_ok() && !running;
            if ui.add_enabled(ok, egui::Button::new(format!("{} {}", icon::PLAY, tr("Run export")))).clicked() {
                self.start(cx);
            }
            let savable = ok && !self.saver.is_saving() && !self.fetched_mode();
            if ui
                .add_enabled(savable, egui::Button::new(format!("{} {}", icon::FLOPPY_DISK, tr("Save as task…"))))
                .on_disabled_hover_text(tr("Exports of fetched rows cannot be saved as tasks."))
                .clicked()
            {
                self.saver.open(trf("Export {}", &[&self.subject()]));
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(crate::theme::error_color(ui.visuals().dark_mode)));
            }
        });
        ui.add_space(2.0);
    }
}

/// Export rows already fetched into a grid (no database access).
#[allow(clippy::too_many_arguments)]
async fn export_fetched(
    source: RowStoreSource,
    name: String,
    format: ExportFormat,
    options: ExportOptions,
    path: PathBuf,
    dialect: Arc<dyn Dialect>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> DbResult<ExportJobSummary> {
    let started = std::time::Instant::now();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        tokio::fs::create_dir_all(parent).await.map_err(|e| DbError::io(trf("Cannot create the folder {}: {}", &[&parent.display(), &e])))?;
    }
    let mut source = source;
    progress.set_items_total(1);
    progress.item_started(&name);
    let result = async {
        let mut sink = create_sink(format, &options, ExportTarget::path(&path), Some(dialect))?;
        run_export_batched(&mut source, sink.as_mut(), options.fetch_size, &progress, &cancel).await
    }
    .await;
    progress.item_finished(&name);
    let s = result?;
    for w in &s.warnings {
        progress.warn(Some(&name), w.clone());
    }
    progress.info(Some(&name), trf("{} rows exported to {}", &[&s.rows, &path.display()]));
    let item = ExportItemSummary { name, file: Some(path), rows: s.rows, bytes: s.bytes, warnings: s.warnings, error: None, duration_ms: s.duration_ms };
    Ok(ExportJobSummary { rows: item.rows, bytes: item.bytes, items: vec![item], duration_ms: started.elapsed().as_millis() as u64 })
}

fn describe(s: &ExportJobSummary) -> String {
    let files = s.items.iter().filter(|i| i.error.is_none() && i.file.is_some()).count();
    match s.failed() {
        0 => trf("{} rows exported to {} file(s)", &[&s.rows, &files]),
        n => trf("{} rows exported to {} file(s), {} failed", &[&s.rows, &files, &n]),
    }
}

fn uses_value_format(f: ExportFormat) -> bool {
    !matches!(f, ExportFormat::Sql | ExportFormat::Php | ExportFormat::Parquet | ExportFormat::Xlsx)
}

// ------------------------------------------------------------ option editors

fn grid(id: u64, name: &str) -> egui::Grid {
    egui::Grid::new(("exp_opts", id, name.to_string())).num_columns(2).spacing([10.0, 6.0])
}

fn csv_ui(ui: &mut Ui, id: u64, o: &mut CsvOptions, tsv: bool) {
    grid(id, "csv").show(ui, |ui| {
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
        ui.label(tr("Quote character"));
        char_edit(ui, &mut o.quote_char);
        ui.end_row();
        ui.label(tr("Quote values"));
        combo(
            ui,
            ("csv_quote", id),
            &mut o.quote_mode,
            &[
                (QuoteMode::Needed, tr("When needed").to_string()),
                (QuoteMode::Strings, tr("Text values").to_string()),
                (QuoteMode::Always, tr("Always").to_string()),
                (QuoteMode::Never, tr("Never").to_string()),
            ],
        );
        ui.end_row();
        ui.label(tr("Quotes inside values"));
        combo(ui, ("csv_escape", id), &mut o.escape, &[(EscapeStyle::Doubled, tr("Doubled (\"\")").to_string()), (EscapeStyle::Backslash, tr("Backslash (\\\")").to_string())]);
        ui.end_row();
        ui.label(tr("Header row"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut o.header, "");
            ui.add_enabled_ui(o.header, |ui| {
                combo(
                    ui,
                    ("csv_header", id),
                    &mut o.header_style,
                    &[
                        (HeaderStyle::Label, tr("Column names").to_string()),
                        (HeaderStyle::Type, tr("Column types").to_string()),
                        (HeaderStyle::Both, tr("Names and types").to_string()),
                    ],
                );
            });
        });
        ui.end_row();
        ui.label(tr("NULL written as"));
        ui.add(egui::TextEdit::singleline(&mut o.null_string).hint_text(tr("nothing")).desired_width(100.0));
        ui.end_row();
        ui.label(tr("Line ending"));
        combo(
            ui,
            ("csv_eol", id),
            &mut o.line_ending,
            &[(LineEnding::Lf, tr("LF (macOS, Linux)").to_string()), (LineEnding::CrLf, tr("CRLF (Windows)").to_string()), (LineEnding::Cr, tr("CR").to_string())],
        );
        ui.end_row();
    });
}

fn binary_options() -> Vec<(BinaryEncoding, String)> {
    vec![(BinaryEncoding::Hex, tr("Hexadecimal").to_string()), (BinaryEncoding::Base64, tr("Base64").to_string()), (BinaryEncoding::Text, tr("As text (UTF-8)").to_string())]
}

fn json_ui(ui: &mut Ui, id: u64, o: &mut JsonOptions, document: bool) {
    grid(id, "json").show(ui, |ui| {
        if document {
            ui.label(tr("Structure"));
            combo(
                ui,
                ("json_layout", id),
                &mut o.layout,
                &[(JsonLayout::ArrayOfObjects, tr("Array of objects").to_string()), (JsonLayout::ColumnsAndRows, tr("Columns and rows").to_string())],
            );
            ui.end_row();
            ui.label(tr("Indent"));
            ui.checkbox(&mut o.pretty, tr("Pretty print"));
            ui.end_row();
        }
        ui.label(tr("Table name"));
        ui.checkbox(&mut o.include_table_name, tr("Include the table name"));
        ui.end_row();
        ui.label(tr("Binary values"));
        combo(ui, ("json_bin", id), &mut o.binary, &binary_options());
        ui.end_row();
        ui.label(tr("Dates"));
        ui.checkbox(&mut o.format_dates, tr("Use the date formats of the value formatting"));
        ui.end_row();
    });
}

fn sql_ui(ui: &mut Ui, id: u64, o: &mut SqlOptions, dialect: &mut Option<DbKind>, source: Option<DbKind>, kinds: &[DbKind]) {
    grid(id, "sql").show(ui, |ui| {
        ui.label(tr("Target database"));
        let mut options: Vec<(Option<DbKind>, String)> = vec![(
            None,
            match source {
                Some(k) => trf("Same as the source ({})", &[&k.display_name()]),
                None => tr("Same as the source").to_string(),
            },
        )];
        for k in kinds {
            options.push((Some(*k), k.display_name().to_string()));
        }
        options.push((Some(DbKind::Generic), tr("Standard SQL").to_string()));
        combo(ui, ("sql_dialect", id), dialect, &options);
        ui.end_row();
        ui.label(tr("Statements"));
        combo(ui, ("sql_mode", id), &mut o.mode, &[(SqlInsertMode::Insert, tr("INSERT").to_string()), (SqlInsertMode::Upsert, tr("INSERT or UPDATE (upsert)").to_string())]);
        ui.end_row();
        if o.mode == SqlInsertMode::Upsert {
            ui.label(tr("Key columns"));
            let mut keys = o.key_columns.join(", ");
            if ui.add(egui::TextEdit::singleline(&mut keys).hint_text(tr("primary key")).desired_width(240.0)).changed() {
                o.key_columns = keys.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
            }
            ui.end_row();
        }
        ui.label(tr("Rows per INSERT"));
        ui.add(egui::DragValue::new(&mut o.rows_per_statement).range(1..=1000));
        ui.end_row();
        ui.label(tr("Target table"));
        opt_text(ui, &mut o.table_name, tr("name of the source table"), 200.0);
        ui.end_row();
        ui.label(tr("Target schema"));
        opt_text(ui, &mut o.schema_name, tr("schema of the source table"), 200.0);
        ui.end_row();
        ui.label(tr("Qualify names"));
        ui.checkbox(&mut o.qualify_with_schema, tr("Prefix the table with its schema"));
        ui.end_row();
        ui.label(tr("CREATE TABLE"));
        ui.checkbox(&mut o.include_create_table, tr("Write the table definition before the data"));
        ui.end_row();
        ui.label(tr("Quote identifiers"));
        combo(ui, ("sql_quote", id), &mut o.quoting, &[(IdentifierQuoting::AsNeeded, tr("When needed").to_string()), (IdentifierQuoting::Always, tr("Always").to_string())]);
        ui.end_row();
        ui.label(tr("Statement terminator"));
        ui.add(egui::TextEdit::singleline(&mut o.terminator).desired_width(60.0));
        ui.end_row();
    });
}

fn xlsx_ui(ui: &mut Ui, id: u64, o: &mut XlsxOptions) {
    grid(id, "xlsx").show(ui, |ui| {
        ui.label(tr("Sheet name"));
        opt_text(ui, &mut o.sheet_name, tr("name of the table"), 200.0);
        ui.end_row();
        ui.label(tr("Header row"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut o.header, tr("Write"));
            ui.add_enabled_ui(o.header, |ui| {
                ui.checkbox(&mut o.bold_header, tr("Bold"));
                ui.checkbox(&mut o.freeze_header, tr("Freeze"));
                ui.checkbox(&mut o.autofilter, tr("Filter buttons"));
            });
        });
        ui.end_row();
        ui.label(tr("Column width"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut o.auto_column_width, tr("Fit to the content, at most"));
            ui.add_enabled(o.auto_column_width, egui::DragValue::new(&mut o.max_column_width).range(5..=255));
        });
        ui.end_row();
        ui.label(tr("Date format"));
        ui.add(egui::TextEdit::singleline(&mut o.date_format).desired_width(160.0));
        ui.end_row();
        ui.label(tr("Time format"));
        ui.add(egui::TextEdit::singleline(&mut o.time_format).desired_width(160.0));
        ui.end_row();
        ui.label(tr("Timestamp format"));
        ui.add(egui::TextEdit::singleline(&mut o.datetime_format).desired_width(160.0));
        ui.end_row();
        ui.label(tr("Number format"));
        opt_text(ui, &mut o.number_format, tr("General"), 160.0);
        ui.end_row();
        ui.label(tr("Integer format"));
        opt_text(ui, &mut o.integer_format, tr("General"), 160.0);
        ui.end_row();
        ui.label(tr("Rows per sheet"));
        ui.add(egui::DragValue::new(&mut o.max_rows_per_sheet).range(1..=1_048_575));
        ui.end_row();
    });
}

fn markdown_ui(ui: &mut Ui, id: u64, o: &mut MarkdownOptions) {
    grid(id, "md").show(ui, |ui| {
        ui.label(tr("Numbers"));
        ui.checkbox(&mut o.right_align_numbers, tr("Align right"));
        ui.end_row();
        ui.label(tr("Pipes"));
        ui.checkbox(&mut o.escape_pipes, tr("Escape | inside values"));
        ui.end_row();
        ui.label(tr("Line breaks become"));
        ui.add(egui::TextEdit::singleline(&mut o.line_break).desired_width(80.0));
        ui.end_row();
    });
}

fn html_ui(ui: &mut Ui, id: u64, o: &mut HtmlOptions) {
    grid(id, "html").show(ui, |ui| {
        ui.label(tr("Title"));
        opt_text(ui, &mut o.title, tr("name of the table"), 220.0);
        ui.end_row();
        ui.label(tr("Style"));
        ui.checkbox(&mut o.embed_css, tr("Embed a style sheet"));
        ui.end_row();
        ui.label(tr("Header row"));
        ui.checkbox(&mut o.header, tr("Write"));
        ui.end_row();
    });
}

fn xml_ui(ui: &mut Ui, id: u64, o: &mut XmlOptions) {
    grid(id, "xml").show(ui, |ui| {
        ui.label(tr("Root element"));
        ui.add(egui::TextEdit::singleline(&mut o.root_element).desired_width(160.0));
        ui.end_row();
        ui.label(tr("Row element"));
        ui.add(egui::TextEdit::singleline(&mut o.record_element).desired_width(160.0));
        ui.end_row();
        ui.label(tr("NULL values"));
        combo(
            ui,
            ("xml_null", id),
            &mut o.null_mode,
            &[
                (XmlNullMode::NullAttribute, tr("Element with null=\"true\"").to_string()),
                (XmlNullMode::Omit, tr("Leave the element out").to_string()),
                (XmlNullMode::Empty, tr("Empty element").to_string()),
                (XmlNullMode::Text, tr("NULL text of the value formatting").to_string()),
            ],
        );
        ui.end_row();
        ui.label(tr("Document type"));
        ui.checkbox(&mut o.include_dtd, tr("Include a DTD"));
        ui.end_row();
        ui.label(tr("Indent"));
        ui.checkbox(&mut o.indent, tr("Pretty print"));
        ui.end_row();
    });
}

fn txt_ui(ui: &mut Ui, id: u64, o: &mut TxtOptions) {
    grid(id, "txt").show(ui, |ui| {
        ui.label(tr("Borders"));
        ui.checkbox(&mut o.borders, tr("Draw borders"));
        ui.end_row();
        ui.label(tr("Header row"));
        ui.checkbox(&mut o.header, tr("Write"));
        ui.end_row();
        ui.label(tr("Column width"));
        ui.horizontal(|ui| {
            ui.label(tr("from"));
            ui.add(egui::DragValue::new(&mut o.min_column_width).range(1..=200));
            ui.label(tr("to"));
            ui.add(egui::DragValue::new(&mut o.max_column_width).range(1..=1000));
        });
        ui.end_row();
        ui.label(tr("NULL values"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut o.show_nulls, tr("Show as"));
            ui.add_enabled(o.show_nulls, egui::TextEdit::singleline(&mut o.null_text).desired_width(80.0));
        });
        ui.end_row();
        ui.label(tr("Numbers"));
        ui.checkbox(&mut o.right_align_numbers, tr("Align right"));
        ui.end_row();
        ui.label(tr("Rows measured for widths"));
        ui.add(egui::DragValue::new(&mut o.width_sample_rows).range(1..=100_000));
        ui.end_row();
    });
}

fn dbunit_ui(ui: &mut Ui, id: u64, o: &mut DbUnitOptions) {
    grid(id, "dbunit").show(ui, |ui| {
        ui.label(tr("Table element"));
        opt_text(ui, &mut o.table_name, tr("name of the table"), 200.0);
        ui.end_row();
        ui.label(tr("Upper case"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut o.upper_case_table_name, tr("Table name"));
            ui.checkbox(&mut o.upper_case_column_names, tr("Column names"));
        });
        ui.end_row();
        ui.label(tr("NULL values"));
        opt_text(ui, &mut o.null_value, tr("attribute left out"), 160.0);
        ui.end_row();
    });
}

fn php_ui(ui: &mut Ui, id: u64, o: &mut PhpOptions) {
    grid(id, "php").show(ui, |ui| {
        ui.label(tr("Variable name"));
        opt_text(ui, &mut o.variable_name, tr("name of the table"), 200.0);
        ui.end_row();
        ui.label(tr("Arrays"));
        ui.checkbox(&mut o.short_array_syntax, tr("Short syntax [ ]"));
        ui.end_row();
        ui.label(tr("Opening tag"));
        ui.checkbox(&mut o.open_tag, tr("Start with <?php"));
        ui.end_row();
    });
}

fn parquet_ui(ui: &mut Ui, id: u64, o: &mut ParquetOptions) {
    grid(id, "parquet").show(ui, |ui| {
        ui.label(tr("Compression"));
        combo(ui, ("pq_comp", id), &mut o.compression, &[(ParquetCompression::Snappy, tr("Snappy").to_string()), (ParquetCompression::None, tr("None").to_string())]);
        ui.end_row();
        ui.label(tr("Rows per row group"));
        ui.add(egui::DragValue::new(&mut o.row_group_rows).range(1..=10_000_000));
        ui.end_row();
        ui.label(tr("Row group size (MB)"));
        let mut mb = (o.row_group_bytes / (1024 * 1024)).max(1);
        if ui.add(egui::DragValue::new(&mut mb).range(1..=4096)).changed() {
            o.row_group_bytes = mb * 1024 * 1024;
        }
        ui.end_row();
    });
}

fn values_ui(ui: &mut Ui, id: u64, v: &mut ValueFormat, format: ExportFormat) {
    if matches!(format, ExportFormat::Json | ExportFormat::JsonLines) {
        ui.weak(tr("JSON uses the date formats only with \"Use the date formats of the value formatting\"."));
    }
    grid(id, "values").show(ui, |ui| {
        ui.label(tr("NULL text"));
        ui.add(egui::TextEdit::singleline(&mut v.null_text).hint_text(tr("empty")).desired_width(100.0));
        ui.end_row();
        ui.label(tr("Date format"));
        opt_text(ui, &mut v.date_format, tr("ISO, e.g. %d.%m.%Y"), 160.0);
        ui.end_row();
        ui.label(tr("Time format"));
        opt_text(ui, &mut v.time_format, tr("ISO, e.g. %H:%M"), 160.0);
        ui.end_row();
        ui.label(tr("Timestamp format"));
        opt_text(ui, &mut v.datetime_format, tr("ISO, e.g. %d.%m.%Y %H:%M:%S"), 160.0);
        ui.end_row();
        ui.label(tr("Timestamp with time zone"));
        opt_text(ui, &mut v.timestamptz_format, tr("ISO, e.g. %Y-%m-%d %H:%M %z"), 160.0);
        ui.end_row();
        ui.label(tr("Decimal separator"));
        opt_char_edit(ui, &mut v.decimal_separator, ".");
        ui.end_row();
        ui.label(tr("Thousands separator"));
        opt_char_edit(ui, &mut v.thousands_separator, "");
        ui.end_row();
        ui.label(tr("Fraction digits"));
        opt_number(ui, &mut v.float_decimals, tr("all"));
        ui.end_row();
        ui.label(tr("Binary values"));
        combo(ui, ("val_bin", id), &mut v.binary, &binary_options());
        ui.end_row();
        ui.label(tr("Booleans"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut v.true_text).desired_width(70.0));
            ui.add(egui::TextEdit::singleline(&mut v.false_text).desired_width(70.0));
        });
        ui.end_row();
        ui.label(tr("Text"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut v.trim, tr("Trim spaces"));
            ui.label(tr("at most"));
            opt_number(ui, &mut v.max_length, tr("any length"));
            ui.label(tr("characters"));
        });
        ui.end_row();
    });
    ui.weak(tr("Date patterns use chrono syntax: %Y year, %m month, %d day, %H hour, %M minute, %S second."));
}

fn common_ui(ui: &mut Ui, id: u64, o: &mut ExportOptions, format: ExportFormat) {
    grid(id, "common").show(ui, |ui| {
        if !format.is_binary() {
            ui.label(tr("Encoding"));
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("exp_enc", id)).selected_text(o.encoding.clone()).show_ui(ui, |ui| {
                    for e in ENCODINGS {
                        if ui.selectable_label(o.encoding.eq_ignore_ascii_case(e), e).clicked() {
                            o.encoding = e.to_string();
                        }
                    }
                });
                let bom = o.encoding.to_ascii_uppercase().starts_with("UTF");
                ui.add_enabled(bom, egui::Checkbox::new(&mut o.insert_bom, tr("Byte order mark (BOM)")));
            });
            ui.end_row();
        }
        ui.label(tr("Rows fetched per batch"));
        ui.add(egui::DragValue::new(&mut o.fetch_size).range(1..=100_000));
        ui.end_row();
        ui.label(tr("After a failure"));
        ui.checkbox(&mut o.keep_partial_file, tr("Keep the partially written file"));
        ui.end_row();
    });
}

impl TabView for ExportTab {
    fn title(&self) -> String {
        let subject = self.subject();
        if subject.is_empty() { tr("Export data").to_string() } else { trf("Export {}", &[&subject]) }
    }

    fn icon(&self) -> &'static str {
        icon::EXPORT
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll(cx);
        let uid = self.uid;
        egui::Panel::top(egui::Id::new(("exp_top", uid))).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(format!("{} {}", icon::EXPORT, tr("Export data")));
                ui.weak(connection_name(cx, &self.conn));
            });
            let steps = [tr("Source"), tr("Format"), tr("Output"), tr("Run")];
            let reachable = if self.run.is_some() { PAGE_RUN } else { self.reachable().max(PAGE_FORMAT.min(self.reachable())) };
            if let Some(p) = steps_ui(ui, &steps, self.page, reachable.max(self.page))
                && self.run.is_none()
            {
                self.page = p;
            }
            ui.add_space(2.0);
        });
        egui::Panel::bottom(egui::Id::new(("exp_bottom", uid))).show(ui, |ui| self.footer(ui, cx));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("exp_body", uid)).auto_shrink([false, false]).show(ui, |ui| match self.page {
                PAGE_SOURCE => self.source_page(ui, cx),
                PAGE_FORMAT => self.format_page(ui, cx),
                PAGE_OUTPUT => self.output_page(ui),
                _ => self.run_page(ui, cx),
            });
        });
        if let Some((name, folder)) = self.saver.ui(ui.ctx(), egui::Id::new(("exp_save", uid))) {
            self.save_task(cx, &name, folder);
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
    fn constructor_builds_items() {
        let t = ObjectRef::table(Some("public".into()), "users");
        let tab = ExportTab::new("c".into(), vec![t.clone()], None);
        let job = tab.job();
        assert_eq!(job.items, vec![ExportItem::Table { table: t.clone(), where_clause: None }]);
        let q = ExportTab::new("c".into(), vec![t], Some("select * from users;".into()));
        assert_eq!(q.job().items, vec![ExportItem::Query { sql: "select * from users".into(), name: Some("users".into()) }]);
        assert!(q.validate().is_ok() || q.settings.output_folder.as_os_str().is_empty());
        let empty = ExportTab::new("c".into(), Vec::new(), None);
        assert!(empty.validate().is_err());
        assert_eq!(empty.reachable(), PAGE_SOURCE);
    }
}
