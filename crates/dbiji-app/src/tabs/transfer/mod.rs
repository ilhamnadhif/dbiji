//! Data transfer tabs: the export wizard (FR-T1), the import wizard (FR-T2),
//! table migration between connections (FR-T3) and mock data generation.
//!
//! The tabs only collect a `dbiji-transfer` job, run it on the backend with
//! dedicated sessions opened by [`AppSessionFactory`](crate::transferexec::AppSessionFactory),
//! mirror its progress into the operations panel and show the summary.
//! Export, import and migration configurations can be saved as tasks and run
//! headless through [`crate::transferexec`].

// `DbError` (the error type of dbiji-core) is 144 bytes, so every `DbResult`
// trips this lint; boxing it is a decision for dbiji-core.
#![allow(clippy::result_large_err)]

pub mod export;
pub mod import;
pub mod migration;
pub mod mock;

pub use export::{ExportTab, ResultSource, grid_store_rows};
pub use import::{ImportFileFormat, ImportTab, ImportTarget};
pub use migration::MigrationTab;
pub use mock::MockDataTab;

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::ops::{OpProgress, OpState};
use dbiji_core::ddl::TableDef;
use dbiji_core::meta::{ObjectInfo, ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DataKind, DbError, DbResult, ErrorKind, tr, trf};
use dbiji_tasks::{TaskDef, TaskKind};
use dbiji_transfer::{CancellationToken, MessageLevel, OnError, Progress, ProgressMessage, TransactionScope};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Messages kept by a tab's log (older ones are dropped).
const MAX_LOG_MESSAGES: usize = 2000;

/// Unique id for the egui ids of one tab instance.
pub(crate) fn next_uid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ------------------------------------------------------------ running jobs

/// A transfer running on the backend, registered in the operations panel.
pub(crate) struct Running<T> {
    op: u64,
    pub progress: Arc<Progress>,
    cancel: CancellationToken,
    job: Job<DbResult<T>>,
}

impl<T: Send + 'static> Running<T> {
    /// Register an operation called `title` and run `make(progress, cancel)`
    /// on the backend. The operations panel's cancel button cancels it too.
    pub fn spawn<F, Fut>(cx: &mut AppContext, title: String, make: F) -> Self
    where
        F: FnOnce(Arc<Progress>, CancellationToken) -> Fut,
        Fut: Future<Output = DbResult<T>> + Send + 'static,
    {
        let handle = cx.ops.start(title);
        let progress = Progress::shared();
        let fut = make(progress.clone(), handle.cancel.clone());
        let job = cx.backend.spawn(mirror_progress(fut, progress.clone(), handle.progress.clone()));
        Running { op: handle.id, progress, cancel: handle.cancel, job }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The result once the job has finished; the operation is marked
    /// finished with `describe(result)` as its message.
    pub fn poll(&mut self, cx: &mut AppContext, describe: impl FnOnce(&T) -> String) -> Option<DbResult<T>> {
        let result = self.job.poll()?;
        let state = match &result {
            Ok(v) => OpState::Succeeded(describe(v)),
            Err(e) if e.kind == ErrorKind::Cancelled => OpState::Cancelled,
            Err(e) => OpState::Failed(e.full_text()),
        };
        cx.ops.finish(self.op, state);
        Some(result)
    }

    /// Cancel and mark the operation cancelled (the tab is being closed).
    pub fn abandon(&mut self, cx: &mut AppContext) {
        self.cancel.cancel();
        cx.ops.finish(self.op, OpState::Cancelled);
    }
}

/// Copy the transfer counters into the operations panel while `fut` runs,
/// so the panel stays current even when the tab is not visible.
async fn mirror_progress<T>(fut: impl Future<Output = DbResult<T>>, progress: Arc<Progress>, op: Arc<OpProgress>) -> DbResult<T> {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = &mut fut => {
                copy_progress(&progress, &op);
                return r;
            }
            _ = tick.tick() => copy_progress(&progress, &op),
        }
    }
}

fn copy_progress(progress: &Progress, op: &OpProgress) {
    let rows = progress.rows();
    op.set(rows, progress.total_rows());
    op.set_message(match progress.current_item() {
        Some(item) => trf("{} rows · {}", &[&rows, &item]),
        None => trf("{} rows", &[&rows]),
    });
}

// ----------------------------------------------------------------- progress

/// Live counters of a running transfer. Returns true when Cancel was clicked.
pub(crate) fn progress_ui(ui: &mut Ui, progress: &Progress, cancelling: bool) -> bool {
    let s = progress.snapshot();
    let rows_text = match s.total_rows {
        Some(t) => trf("{} of {} rows", &[&s.rows, &t]),
        None => trf("{} rows", &[&s.rows]),
    };
    match s.fraction {
        Some(f) => {
            ui.add(egui::ProgressBar::new(f as f32).desired_width(ui.available_width().min(520.0)).text(rows_text));
        }
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(rows_text);
            });
        }
    }
    egui::Grid::new(ui.next_auto_id()).num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
        ui.weak(tr("Speed"));
        ui.label(trf("{} rows/s", &[&format!("{:.0}", s.rows_per_second)]));
        ui.end_row();
        if s.bytes > 0 {
            ui.weak(tr("Written"));
            ui.label(crate::widgets::format_bytes(s.bytes.min(i64::MAX as u64) as i64));
            ui.end_row();
        }
        if s.items_total > 0 {
            ui.weak(tr("Objects"));
            ui.label(trf("{} of {}", &[&s.items_done, &s.items_total]));
            ui.end_row();
        }
        if !s.active_items.is_empty() {
            ui.weak(tr("Current"));
            ui.label(s.active_items.join(", "));
            ui.end_row();
        }
        if s.errors > 0 {
            ui.weak(tr("Errors"));
            ui.label(RichText::new(s.errors.to_string()).color(crate::theme::error_color(ui.visuals().dark_mode)));
            ui.end_row();
        }
        ui.weak(tr("Elapsed"));
        ui.label(crate::widgets::format_duration(Duration::from_secs_f64(s.elapsed_secs.max(0.0))));
        ui.end_row();
    });
    let label = if cancelling { tr("Cancelling…") } else { tr("Cancel") };
    let clicked = ui.add_enabled(!cancelling, egui::Button::new(format!("{} {label}", icon::STOP))).clicked();
    ui.ctx().request_repaint_after(Duration::from_millis(100));
    clicked
}

/// Messages of a transfer, drained from its [`Progress`] every frame.
#[derive(Default)]
pub(crate) struct MessageLog {
    messages: Vec<ProgressMessage>,
    dropped: usize,
}

impl MessageLog {
    pub fn clear(&mut self) {
        self.messages.clear();
        self.dropped = 0;
    }

    pub fn drain(&mut self, progress: &Progress) {
        self.push(progress.take_messages());
    }

    /// Append messages taken from a progress by the caller.
    pub fn push(&mut self, messages: Vec<ProgressMessage>) {
        self.messages.extend(messages);
        if self.messages.len() > MAX_LOG_MESSAGES {
            let extra = self.messages.len() - MAX_LOG_MESSAGES;
            self.messages.drain(..extra);
            self.dropped += extra;
        }
    }

    pub fn text(&self) -> String {
        self.messages.iter().map(format_message).collect::<Vec<_>>().join("\n")
    }

    pub fn ui(&self, ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, max_height: f32) {
        if self.messages.is_empty() {
            return;
        }
        let dark = ui.visuals().dark_mode;
        ui.horizontal(|ui| {
            ui.strong(tr("Messages"));
            if self.dropped > 0 {
                ui.weak(trf("({} older messages not shown)", &[&self.dropped]));
            }
            if ui.small_button((dbiji_icons::ic(icon::COPY), tr("Copy"))).clicked() {
                ui.ctx().copy_text(self.text());
            }
        });
        egui::ScrollArea::vertical().id_salt(id).max_height(max_height).auto_shrink([false, true]).stick_to_bottom(true).show(ui, |ui| {
            for m in &self.messages {
                let (glyph, color) = match m.level {
                    MessageLevel::Info => (icon::INFO, ui.visuals().weak_text_color()),
                    MessageLevel::Warning => (icon::WARNING, crate::theme::warning_color(dark)),
                    MessageLevel::Error => (icon::X_CIRCLE, crate::theme::error_color(dark)),
                };
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(glyph).color(color));
                    ui.add(egui::Label::new(RichText::new(format_message(m)).small()).wrap().selectable(true));
                });
            }
        });
    }
}

fn format_message(m: &ProgressMessage) -> String {
    match &m.item {
        Some(item) => format!("[{:.1}s] {item}: {}", m.elapsed_secs, m.text),
        None => format!("[{:.1}s] {}", m.elapsed_secs, m.text),
    }
}

// -------------------------------------------------------------- save tasks

/// "Save as task" dialog plus the background write to the task store.
#[derive(Default)]
pub(crate) struct TaskSaver {
    open: bool,
    name: String,
    folder: String,
    error: Option<String>,
    job: Option<Job<DbResult<TaskDef>>>,
    /// Id of the task saved last.
    pub saved: Option<String>,
}

impl TaskSaver {
    pub fn open(&mut self, default_name: String) {
        self.open = true;
        self.name = default_name;
        self.error = None;
    }

    pub fn is_saving(&self) -> bool {
        self.job.is_some()
    }

    /// Draw the dialog; returns the confirmed name and folder.
    pub fn ui(&mut self, ctx: &egui::Context, id: egui::Id) -> Option<(String, Option<String>)> {
        if !self.open {
            return None;
        }
        let mut confirmed = None;
        let mut close = false;
        let modal = egui::Modal::new(id).show(ctx, |ui| {
            ui.set_min_width(360.0);
            ui.heading(tr("Save as task"));
            ui.label(tr("The task runs the same transfer from the Tasks view, on a schedule or with dbiji --run-task."));
            egui::Grid::new(id.with("grid")).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label(tr("Name"));
                ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(240.0));
                ui.end_row();
                ui.label(tr("Folder"));
                ui.add(egui::TextEdit::singleline(&mut self.folder).hint_text(tr("optional, e.g. exports/nightly")).desired_width(240.0));
                ui.end_row();
            });
            if let Some(e) = &self.error {
                crate::widgets::error_box(ui, e);
            }
            ui.horizontal(|ui| {
                let ok = !self.name.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save")))).clicked() {
                    let folder = Some(self.folder.trim().to_string()).filter(|f| !f.is_empty());
                    confirmed = Some((self.name.trim().to_string(), folder));
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.open = false;
        }
        confirmed
    }

    /// Store a new task in the background.
    pub fn save(&mut self, cx: &AppContext, name: String, folder: Option<String>, kind: TaskKind) {
        let Some(store) = cx.services.tasks.clone() else {
            self.error = Some(tr("The task store is not available.").to_string());
            return;
        };
        let mut task = TaskDef::new(name, kind);
        task.folder = folder;
        self.error = None;
        self.job = Some(cx.backend.spawn(async move { store.create(task) }));
    }

    /// Show the error of a failed attempt in the dialog.
    pub fn fail(&mut self, error: String) {
        self.open = true;
        self.error = Some(error);
    }

    pub fn poll(&mut self, cx: &mut AppContext) {
        let Some(result) = self.job.as_mut().and_then(|j| j.poll()) else { return };
        self.job = None;
        match result {
            Ok(task) => {
                self.open = false;
                cx.toasts.success(trf("Task \"{}\" saved", &[&task.name]));
                self.saved = Some(task.id);
            }
            Err(e) => {
                self.open = true;
                self.error = Some(e.full_text());
            }
        }
    }
}

// ------------------------------------------------------------------ widgets

/// Wizard step buttons; returns the step the user clicked.
pub(crate) fn steps_ui(ui: &mut Ui, steps: &[&str], current: usize, reachable: usize) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        for (i, s) in steps.iter().enumerate() {
            if i > 0 {
                ui.weak(icon::CARET_RIGHT);
            }
            let r = ui.add_enabled(i <= reachable, egui::Button::selectable(i == current, format!("{} {s}", i + 1)));
            if r.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// Combo box over a fixed list of (value, label) pairs.
pub(crate) fn combo<T: PartialEq + Clone>(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut T, options: &[(T, String)]) -> bool {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| l.clone()).unwrap_or_default();
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
        for (v, label) in options {
            if ui.selectable_label(v == value, label).clicked() && v != value {
                *value = v.clone();
                changed = true;
            }
        }
    });
    changed
}

/// Single line editor of an optional text (empty = `None`).
pub(crate) fn opt_text(ui: &mut Ui, value: &mut Option<String>, hint: &str, width: f32) -> bool {
    let mut s = value.clone().unwrap_or_default();
    let changed = ui.add(egui::TextEdit::singleline(&mut s).hint_text(hint).desired_width(width)).changed();
    if changed {
        *value = Some(s).filter(|s| !s.is_empty());
    }
    changed
}

/// Text field over a value that is parsed from text. The typed text is kept
/// while the field has focus, so intermediate (invalid) input such as `-` or
/// an unfinished date can be typed; `apply` receives the text after every change.
pub(crate) fn parsed_edit(ui: &mut Ui, shown: String, hint: &str, width: f32, multiline: bool, apply: impl FnOnce(&str)) -> bool {
    let key = ui.next_auto_id().with("dbiji_parsed_edit");
    let mut text = ui.data(|d| d.get_temp::<String>(key)).unwrap_or(shown);
    let edit = if multiline { egui::TextEdit::multiline(&mut text).desired_rows(2) } else { egui::TextEdit::singleline(&mut text) };
    let r = ui.add(edit.hint_text(hint).desired_width(width));
    let changed = r.changed();
    if changed {
        apply(&text);
    }
    if r.has_focus() {
        ui.data_mut(|d| d.insert_temp(key, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(key));
    }
    changed
}

/// Editor of a single character (keeps the old value while empty).
pub(crate) fn char_edit(ui: &mut Ui, value: &mut char) -> bool {
    parsed_edit(ui, value.to_string(), "", 28.0, false, |t| {
        if let Some(c) = t.chars().next() {
            *value = c;
        }
    })
}

/// Editor of an optional character (empty = `None`).
pub(crate) fn opt_char_edit(ui: &mut Ui, value: &mut Option<char>, hint: &str) -> bool {
    parsed_edit(ui, value.map(String::from).unwrap_or_default(), hint, 28.0, false, |t| *value = t.chars().next())
}

/// Editor of a list of texts, one per line.
pub(crate) fn lines_edit(ui: &mut Ui, value: &mut Vec<String>, hint: &str, width: f32) -> bool {
    parsed_edit(ui, value.join("\n"), hint, width, true, |t| {
        *value = t.lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect();
    })
}

/// Editor of an optional number (empty = `None`).
pub(crate) fn opt_number<T: std::str::FromStr + ToString>(ui: &mut Ui, value: &mut Option<T>, hint: &str) -> bool {
    let shown = value.as_ref().map(T::to_string).unwrap_or_default();
    parsed_edit(ui, shown, hint, 90.0, false, |t| {
        let t = t.trim();
        if t.is_empty() {
            *value = None;
        } else if let Ok(v) = t.parse::<T>() {
            *value = Some(v);
        }
    })
}

/// Editor of a number (invalid input keeps the old value).
pub(crate) fn number_edit<T: std::str::FromStr + ToString>(ui: &mut Ui, value: &mut T, width: f32) -> bool {
    parsed_edit(ui, value.to_string(), "", width, false, |t| {
        if let Ok(v) = t.trim().parse::<T>() {
            *value = v;
        }
    })
}

/// Small table of text cells with a bounded height (previews, summaries).
pub(crate) fn text_table(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, headers: &[String], rows: &[Vec<String>], max_height: f32) {
    use egui_extras::{Column, TableBuilder};
    if headers.is_empty() {
        return;
    }
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    egui::ScrollArea::horizontal().id_salt(egui::Id::new(&id).with("h")).show(ui, |ui| {
        let mut tb = TableBuilder::new(ui)
            .id_salt(egui::Id::new(&id).with("t"))
            .striped(true)
            .resizable(true)
            .max_scroll_height(max_height)
            .auto_shrink([true, true])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for _ in headers {
            tb = tb.column(Column::auto().at_least(40.0).at_most(360.0).clip(true).resizable(true));
        }
        tb.header(row_h + 2.0, |mut header| {
            for h in headers {
                header.col(|ui| {
                    ui.strong(h);
                });
            }
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let i = row.index();
                if let Some(cells) = rows.get(i) {
                    for c in cells.iter().take(headers.len()) {
                        row.col(|ui| {
                            let first: String = c.chars().take(200).collect();
                            let first = first.lines().next().unwrap_or("").to_string();
                            let r = ui.add(egui::Label::new(first.clone()).truncate());
                            if c.len() > first.len() {
                                r.on_hover_text(c.chars().take(2000).collect::<String>());
                            }
                        });
                    }
                }
            });
        });
    });
}

/// Section heading.
pub(crate) fn section(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).strong());
}

/// Options of [`OnError`].
pub(crate) fn on_error_options() -> Vec<(OnError, String)> {
    vec![
        (OnError::Abort, tr("Stop at the first error").to_string()),
        (OnError::SkipRow, tr("Skip failing rows").to_string()),
        (OnError::LogAndContinue, tr("Skip and log every failing row").to_string()),
    ]
}

/// Options of [`TransactionScope`].
pub(crate) fn transaction_options() -> Vec<(TransactionScope, String)> {
    vec![
        (TransactionScope::PerBatch, tr("Commit after every batch").to_string()),
        (TransactionScope::Whole, tr("One transaction for everything").to_string()),
        (TransactionScope::None, tr("Auto-commit every statement").to_string()),
    ]
}

/// Human readable name of a value kind.
pub(crate) fn kind_label(kind: DataKind) -> &'static str {
    match kind {
        DataKind::Bool => tr("Boolean"),
        DataKind::Int => tr("Integer"),
        DataKind::Float => tr("Floating point"),
        DataKind::Decimal => tr("Decimal"),
        DataKind::Text => tr("Text"),
        DataKind::Binary => tr("Binary"),
        DataKind::Date => tr("Date"),
        DataKind::Time => tr("Time"),
        DataKind::TimeTz => tr("Time with time zone"),
        DataKind::DateTime => tr("Timestamp"),
        DataKind::DateTimeTz => tr("Timestamp with time zone"),
        DataKind::Interval => tr("Interval"),
        DataKind::Uuid => tr("UUID"),
        DataKind::Json => tr("JSON"),
        DataKind::Xml => tr("XML"),
        DataKind::Array => tr("Array"),
        DataKind::Geometry => tr("Geometry"),
        DataKind::Enum => tr("Enumeration"),
        DataKind::Network => tr("Network address"),
        DataKind::Bit => tr("Bit string"),
        DataKind::Range => tr("Range"),
        DataKind::Other => tr("Other"),
    }
}

/// Kinds offered when choosing a column type.
pub(crate) const COMMON_KINDS: [DataKind; 12] = [
    DataKind::Text,
    DataKind::Int,
    DataKind::Decimal,
    DataKind::Float,
    DataKind::Bool,
    DataKind::Date,
    DataKind::Time,
    DataKind::DateTime,
    DataKind::DateTimeTz,
    DataKind::Uuid,
    DataKind::Json,
    DataKind::Binary,
];

/// Folder offered for new exports: `~/Downloads`, else the home folder.
pub(crate) fn default_output_folder() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    match home {
        Some(h) => {
            let downloads = h.join("Downloads");
            if downloads.is_dir() { downloads } else { h }
        }
        None => PathBuf::from("."),
    }
}

/// Error shown when the connection is closed; asks the app to connect.
pub(crate) fn not_connected(cx: &mut AppContext, conn: &str) -> String {
    cx.push(AppCommand::Connect(conn.to_string()));
    tr("The connection is closed; connecting… try again when it is connected.").to_string()
}

/// Display name of a connection.
pub(crate) fn connection_name(cx: &AppContext, conn: &str) -> String {
    cx.conns.config(conn).map(|c| c.name.clone()).unwrap_or_else(|| conn.to_string())
}

/// Schemas of a connected connection (requested in the background); `None`
/// while loading or when not connected.
pub(crate) fn schema_names(cx: &mut AppContext, conn: &str) -> Option<Vec<String>> {
    let backend = cx.backend;
    let live = cx.conns.live_mut(conn)?;
    let main = live.main.clone();
    live.meta.request_schemas(backend, &main, None);
    let schemas = live.meta.schemas_of(None);
    if schemas.is_loading() || schemas.is_not_loaded() {
        return None;
    }
    Some(schemas.get().map(|v| v.iter().filter(|s| !s.is_system).map(|s| s.name.clone()).collect()).unwrap_or_default())
}

/// Default schema of a connected connection (current schema or database).
pub(crate) fn current_schema(cx: &AppContext, conn: &str) -> Option<SchemaRef> {
    let live = cx.conns.live(conn)?;
    let schema = live.current_schema.clone().or_else(|| live.current_database.clone());
    Some(SchemaRef::new(None, schema))
}

/// Schema picker; returns true when the selection changed.
pub(crate) fn schema_combo(ui: &mut Ui, cx: &mut AppContext, conn: &str, id: impl std::hash::Hash + std::fmt::Debug, value: &mut Option<SchemaRef>) -> bool {
    let names = schema_names(cx, conn);
    let label = value.as_ref().and_then(|s| s.schema.clone()).unwrap_or_else(|| tr("(default)").to_string());
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(label).width(180.0).show_ui(ui, |ui| {
        if ui.selectable_label(value.as_ref().is_none_or(|v| v.schema.is_none()), tr("(default)")).clicked() {
            *value = None;
            changed = true;
        }
        match &names {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Loading…"));
                });
            }
            Some(names) => {
                for n in names {
                    let selected = value.as_ref().and_then(|v| v.schema.as_deref()) == Some(n.as_str());
                    if ui.selectable_label(selected, n).clicked() && !selected {
                        let database = value.as_ref().and_then(|v| v.database.clone());
                        *value = Some(SchemaRef::new(database, Some(n.clone())));
                        changed = true;
                    }
                }
            }
        }
    });
    changed
}

/// Tables (and optionally views) of a schema from the metadata cache;
/// `None` while loading.
pub(crate) fn schema_tables(cx: &mut AppContext, conn: &str, schema: &SchemaRef, views: bool) -> Option<DbResult<Vec<ObjectInfo>>> {
    let backend = cx.backend;
    let live = cx.conns.live_mut(conn)?;
    let session = live.ensure_session(backend, schema.database.as_deref())?;
    let mut kinds = vec![ObjectKind::Table];
    if views {
        kinds.push(ObjectKind::View);
    }
    for kind in &kinds {
        live.meta.request_objects(backend, &session, schema, *kind);
    }
    let mut out = Vec::new();
    for kind in kinds {
        let entry = live.meta.objects.get(&(schema.clone(), kind))?;
        if let Some(e) = entry.error() {
            return Some(Err(e.clone()));
        }
        out.extend(entry.get()?.iter().cloned());
    }
    Some(Ok(out))
}

/// Definition of a table from the metadata cache (requested in the
/// background); `None` while loading or when not connected.
pub(crate) fn table_def_of(cx: &mut AppContext, conn: &str, table: &ObjectRef) -> Option<DbResult<Arc<TableDef>>> {
    let backend = cx.backend;
    let live = cx.conns.live_mut(conn)?;
    let session = live.ensure_session(backend, table.database.as_deref())?;
    live.meta.request_table_def(backend, &session, table);
    let entry = live.meta.table_defs.get(table)?;
    if let Some(e) = entry.error() {
        return Some(Err(e.clone()));
    }
    entry.get().cloned().map(Ok)
}

/// Forget a cached table definition so it is loaded again.
pub(crate) fn reload_table_def(cx: &mut AppContext, conn: &str, table: &ObjectRef) {
    if let Some(live) = cx.conns.live_mut(conn) {
        live.meta.table_defs.remove(table);
    }
}

/// Connection picker over all configured connections.
pub(crate) fn connection_combo(ui: &mut Ui, cx: &AppContext, id: impl std::hash::Hash + std::fmt::Debug, value: &mut Option<String>) -> bool {
    let conns: Vec<(String, String)> = cx.conns.configs().iter().map(|c| (c.id.clone(), c.name.clone())).collect();
    let current = value.as_ref().and_then(|v| conns.iter().find(|(i, _)| i == v)).map(|(_, n)| n.clone()).unwrap_or_else(|| tr("Choose…").to_string());
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(current).width(200.0).show_ui(ui, |ui| {
        for (cid, name) in &conns {
            let glyph = if cx.conns.is_connected(cid) { icon::PLUGS_CONNECTED } else { icon::PLUGS };
            let selected = value.as_deref() == Some(cid.as_str());
            if ui.selectable_label(selected, format!("{glyph} {name}")).clicked() && !selected {
                *value = Some(cid.clone());
                changed = true;
            }
        }
    });
    changed
}

/// The error of a failed run, for the summary page.
pub(crate) fn error_text(e: &DbError) -> String {
    if e.kind == ErrorKind::Cancelled { tr("Cancelled by the user.").to_string() } else { e.full_text() }
}
