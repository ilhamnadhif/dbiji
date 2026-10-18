//! Backup and restore with the vendor tools (pg_dump / pg_restore / psql,
//! mysqldump / mysql): option forms (shared with the task editor), the
//! command line preview, live log output, cancellation and "save as task".

use super::{
    ToolFlavor, dialog_near, error_lines, file_safe, home_dir, is_unknown_setting_error, join_args, join_list, newer_tool_warning, spawn_blocking, split_args, split_list,
    tool_version,
};
use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::ops::{OpHandle, OpState};
use crate::tabs::TabView;
use dbiji_core::admin::{BackupFormat, BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::error::ErrorKind;
use dbiji_core::meta::SchemaRef;
use dbiji_core::{DbError, DbResult, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Log lines kept for a run.
const MAX_LOG_LINES: usize = 20_000;

fn format_label(f: BackupFormat) -> &'static str {
    match f {
        BackupFormat::Plain => tr("Plain SQL script"),
        BackupFormat::Custom => tr("Custom archive"),
        BackupFormat::Tar => tr("Tar archive"),
        BackupFormat::Directory => tr("Directory"),
    }
}

/// Formats offered for a tool flavor; SQLite has a SQL dump and a file copy.
fn formats(flavor: ToolFlavor) -> &'static [BackupFormat] {
    match flavor {
        ToolFlavor::Sqlite => &[BackupFormat::Plain, BackupFormat::Custom],
        _ => &[BackupFormat::Plain, BackupFormat::Custom, BackupFormat::Tar, BackupFormat::Directory],
    }
}

fn flavor_format_label(flavor: ToolFlavor, f: BackupFormat) -> &'static str {
    match (flavor, f) {
        (ToolFlavor::Sqlite, BackupFormat::Custom) => tr("Database file copy"),
        _ => format_label(f),
    }
}

/// Usual file extension of a backup format.
fn format_extension(f: BackupFormat) -> &'static str {
    match f {
        BackupFormat::Plain => "sql",
        BackupFormat::Custom => "dump",
        BackupFormat::Tar => "tar",
        BackupFormat::Directory => "",
    }
}

/// Swap the default extension of `from` for the one of `to`.
/// A SQLite file copy is a `.db` file.
fn retarget_extension_for(flavor: ToolFlavor, path: &str, from: BackupFormat, to: BackupFormat) -> String {
    let ext_of = |f| if flavor == ToolFlavor::Sqlite && f == BackupFormat::Custom { "db" } else { format_extension(f) };
    let (old, new) = (ext_of(from), ext_of(to));
    if path.trim().is_empty() {
        return path.to_string();
    }
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    if ext != old {
        // a name the user chose: keep it
        return path.to_string();
    }
    let stem = if ext.is_empty() { path } else { &path[..path.len() - ext.len() - 1] };
    if new.is_empty() { stem.to_string() } else { format!("{stem}.{new}") }
}

fn browse_row(ui: &mut Ui, value: &mut String, hint: &str, pick: impl FnOnce(&str) -> Option<PathBuf>) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui.add(egui::TextEdit::singleline(value).desired_width(340.0).hint_text(hint)).changed();
        if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Browse…"))).clicked()
            && let Some(p) = pick(value.as_str())
        {
            *value = p.display().to_string();
            changed = true;
        }
    });
    changed
}

fn tool_row(ui: &mut Ui, tool_path: &mut String, extra_args: &mut String) -> bool {
    let mut changed = false;
    ui.label(tr("Tool path"));
    changed |= browse_row(ui, tool_path, tr("empty = search PATH"), |cur| dialog_near(cur).pick_file());
    ui.end_row();
    ui.label(tr("Extra arguments"));
    changed |= ui.add(egui::TextEdit::singleline(extra_args).desired_width(340.0).hint_text(tr("passed to the tool as they are"))).changed();
    ui.end_row();
    changed
}

/// Editable form of [`BackupOptions`].
#[derive(Debug, Clone, PartialEq)]
pub struct BackupForm {
    pub output: String,
    pub format: BackupFormat,
    pub database: String,
    /// Comma separated schema names (MySQL: databases).
    pub schemas: String,
    /// Comma separated table names.
    pub tables: String,
    pub schema_only: bool,
    pub data_only: bool,
    pub clean: bool,
    pub create_database: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    pub inserts: bool,
    pub compress: bool,
    pub compression: u8,
    pub single_transaction: bool,
    pub routines: bool,
    pub tool_path: String,
    /// Shell-like argument list.
    pub extra_args: String,
}

impl Default for BackupForm {
    fn default() -> Self {
        BackupForm::from_options(&BackupOptions { single_transaction: true, routines: true, ..Default::default() })
    }
}

impl BackupForm {
    pub fn from_options(o: &BackupOptions) -> Self {
        BackupForm {
            output: if o.output.as_os_str().is_empty() { String::new() } else { o.output.display().to_string() },
            format: o.format,
            database: o.database.clone().unwrap_or_default(),
            schemas: join_list(&o.schemas),
            tables: join_list(&o.tables),
            schema_only: o.schema_only,
            data_only: o.data_only,
            clean: o.clean,
            create_database: o.create_database,
            no_owner: o.no_owner,
            no_privileges: o.no_privileges,
            inserts: o.inserts,
            compress: o.compression.is_some(),
            compression: o.compression.unwrap_or(6).min(9),
            single_transaction: o.single_transaction,
            routines: o.routines,
            tool_path: o.tool_path.clone().unwrap_or_default(),
            extra_args: join_args(&o.extra_args),
        }
    }

    pub fn to_options(&self) -> BackupOptions {
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        BackupOptions {
            output: PathBuf::from(self.output.trim()),
            format: self.format,
            database: opt(&self.database),
            schemas: split_list(&self.schemas),
            tables: split_list(&self.tables),
            schema_only: self.schema_only,
            data_only: self.data_only,
            clean: self.clean,
            create_database: self.create_database,
            no_owner: self.no_owner,
            no_privileges: self.no_privileges,
            inserts: self.inserts,
            compression: self.compress.then_some(self.compression.min(9)),
            single_transaction: self.single_transaction,
            routines: self.routines,
            tool_path: opt(&self.tool_path),
            extra_args: split_args(&self.extra_args),
        }
    }

    /// Problems to fix before running.
    pub fn validate(&self) -> Result<(), String> {
        if self.output.trim().is_empty() {
            return Err(tr("Choose the output file of the backup").to_string());
        }
        if self.schema_only && self.data_only {
            return Err(tr("Schema only and data only cannot be combined").to_string());
        }
        Ok(())
    }

    /// The option form; returns true when something changed.
    pub fn ui(&mut self, ui: &mut Ui, flavor: ToolFlavor, id: &str) -> bool {
        let mut changed = false;
        let sqlite = flavor == ToolFlavor::Sqlite;
        let pg = matches!(flavor, ToolFlavor::Postgres | ToolFlavor::Other);
        let my = matches!(flavor, ToolFlavor::MySql | ToolFlavor::Other);
        egui::Grid::new((id, "backup_form")).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Output file"));
            let directory = self.format == BackupFormat::Directory;
            changed |= browse_row(ui, &mut self.output, tr("File to write"), |cur| if directory { dialog_near(cur).pick_folder() } else { dialog_near(cur).save_file() });
            ui.end_row();
            if pg || sqlite {
                ui.label(tr("Format"));
                egui::ComboBox::from_id_salt((id, "format")).selected_text(flavor_format_label(flavor, self.format)).width(200.0).show_ui(ui, |ui| {
                    for &f in formats(flavor) {
                        let old = self.format;
                        if ui.selectable_value(&mut self.format, f, flavor_format_label(flavor, f)).changed() {
                            self.output = retarget_extension_for(flavor, &self.output, old, f);
                            changed = true;
                        }
                    }
                });
                ui.end_row();
            }
            ui.label(tr("Database"));
            changed |= ui.add(egui::TextEdit::singleline(&mut self.database).desired_width(220.0).hint_text(tr("(connection database)"))).changed();
            ui.end_row();
            if !sqlite {
                ui.label(if flavor == ToolFlavor::MySql { tr("Databases") } else { tr("Schemas") });
                changed |= ui.add(egui::TextEdit::singleline(&mut self.schemas).desired_width(340.0).hint_text(tr("comma separated; empty = all"))).changed();
                ui.end_row();
            }
            ui.label(tr("Tables"));
            changed |= ui.add(egui::TextEdit::singleline(&mut self.tables).desired_width(340.0).hint_text(tr("comma separated; empty = all"))).changed();
            ui.end_row();
            ui.label(tr("Content"));
            ui.horizontal(|ui| {
                let mut mode = if self.schema_only {
                    1
                } else if self.data_only {
                    2
                } else {
                    0
                };
                let before = mode;
                ui.radio_value(&mut mode, 0, tr("Schema and data"));
                ui.radio_value(&mut mode, 1, tr("Schema only"));
                ui.radio_value(&mut mode, 2, tr("Data only"));
                if mode != before {
                    self.schema_only = mode == 1;
                    self.data_only = mode == 2;
                    changed = true;
                }
            });
            ui.end_row();
            if sqlite {
                // sqlite3 has no further dump options.
                changed |= tool_row(ui, &mut self.tool_path, &mut self.extra_args);
                return;
            }
            ui.label(tr("Options"));
            ui.vertical(|ui| {
                changed |= ui.checkbox(&mut self.clean, tr("Drop objects before creating them")).changed();
                changed |= ui.checkbox(&mut self.create_database, tr("Include CREATE DATABASE")).changed();
                if pg {
                    changed |= ui.checkbox(&mut self.no_owner, tr("Do not restore object owners")).changed();
                    changed |= ui.checkbox(&mut self.no_privileges, tr("Do not dump privileges (GRANT / REVOKE)")).changed();
                }
                let inserts = if flavor == ToolFlavor::MySql { tr("One INSERT statement per row") } else { tr("INSERT statements instead of COPY") };
                changed |= ui.checkbox(&mut self.inserts, inserts).changed();
                if my {
                    changed |= ui.checkbox(&mut self.single_transaction, tr("Single transaction (consistent snapshot)")).changed();
                    changed |= ui.checkbox(&mut self.routines, tr("Include routines and events")).changed();
                }
                if pg && self.format != BackupFormat::Tar {
                    ui.horizontal(|ui| {
                        changed |= ui.checkbox(&mut self.compress, tr("Compression level")).changed();
                        ui.add_enabled_ui(self.compress, |ui| {
                            changed |= ui.add(egui::Slider::new(&mut self.compression, 0..=9)).changed();
                        });
                    });
                }
            });
            ui.end_row();
            changed |= tool_row(ui, &mut self.tool_path, &mut self.extra_args);
        });
        changed
    }
}

/// Editable form of [`RestoreOptions`].
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreForm {
    pub input: String,
    pub format: BackupFormat,
    pub database: String,
    pub clean: bool,
    pub create_database: bool,
    pub no_owner: bool,
    pub single_transaction: bool,
    pub exit_on_error: bool,
    pub tool_path: String,
    pub extra_args: String,
}

impl Default for RestoreForm {
    fn default() -> Self {
        // Stopping at the first error is off: dumps of newer tools contain
        // settings older servers reject (e.g. `SET transaction_timeout`).
        RestoreForm::from_options(&RestoreOptions { exit_on_error: false, ..Default::default() })
    }
}

impl RestoreForm {
    pub fn from_options(o: &RestoreOptions) -> Self {
        RestoreForm {
            input: if o.input.as_os_str().is_empty() { String::new() } else { o.input.display().to_string() },
            format: o.format,
            database: o.database.clone().unwrap_or_default(),
            clean: o.clean,
            create_database: o.create_database,
            no_owner: o.no_owner,
            single_transaction: o.single_transaction,
            exit_on_error: o.exit_on_error,
            tool_path: o.tool_path.clone().unwrap_or_default(),
            extra_args: join_args(&o.extra_args),
        }
    }

    pub fn to_options(&self) -> RestoreOptions {
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        RestoreOptions {
            input: PathBuf::from(self.input.trim()),
            format: self.format,
            database: opt(&self.database),
            clean: self.clean,
            create_database: self.create_database,
            no_owner: self.no_owner,
            single_transaction: self.single_transaction,
            exit_on_error: self.exit_on_error,
            tool_path: opt(&self.tool_path),
            extra_args: split_args(&self.extra_args),
        }
    }

    /// Problems to fix before running.
    pub fn validate(&self) -> Result<(), String> {
        if self.input.trim().is_empty() {
            return Err(tr("Choose the backup file to restore").to_string());
        }
        Ok(())
    }

    /// Guess the format from the file (archives start with `PGDMP`).
    pub fn detect_format(&mut self) {
        let p = std::path::Path::new(self.input.trim());
        if p.is_dir() {
            self.format = BackupFormat::Directory;
            return;
        }
        let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        self.format = match ext.as_str() {
            "tar" => BackupFormat::Tar,
            "sql" | "txt" => BackupFormat::Plain,
            _ => {
                let mut head = [0u8; 5];
                let read = std::fs::File::open(p).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head));
                if read.is_ok() && &head == b"PGDMP" { BackupFormat::Custom } else { self.format }
            }
        };
    }

    /// The option form; returns true when something changed.
    pub fn ui(&mut self, ui: &mut Ui, flavor: ToolFlavor, id: &str) -> bool {
        let mut changed = false;
        let sqlite = flavor == ToolFlavor::Sqlite;
        let pg = matches!(flavor, ToolFlavor::Postgres | ToolFlavor::Other);
        egui::Grid::new((id, "restore_form")).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Backup file"));
            let directory = self.format == BackupFormat::Directory;
            let picked = browse_row(ui, &mut self.input, tr("File to restore"), |cur| if directory { dialog_near(cur).pick_folder() } else { dialog_near(cur).pick_file() });
            if picked && pg {
                self.detect_format();
            }
            changed |= picked;
            ui.end_row();
            if pg || sqlite {
                ui.label(tr("Format"));
                egui::ComboBox::from_id_salt((id, "rformat")).selected_text(flavor_format_label(flavor, self.format)).width(200.0).show_ui(ui, |ui| {
                    for &f in formats(flavor) {
                        changed |= ui.selectable_value(&mut self.format, f, flavor_format_label(flavor, f)).changed();
                    }
                });
                ui.end_row();
            }
            ui.label(tr("Target database"));
            changed |= ui.add(egui::TextEdit::singleline(&mut self.database).desired_width(220.0).hint_text(tr("(connection database)"))).changed();
            ui.end_row();
            if sqlite {
                changed |= tool_row(ui, &mut self.tool_path, &mut self.extra_args);
                return;
            }
            ui.label(tr("Options"));
            ui.vertical(|ui| {
                if pg {
                    changed |= ui.checkbox(&mut self.clean, tr("Drop objects before recreating them")).changed();
                }
                let create = if pg { tr("Create the database first") } else { tr("The script creates the database (no target database)") };
                changed |= ui.checkbox(&mut self.create_database, create).changed();
                if pg {
                    changed |= ui.checkbox(&mut self.no_owner, tr("Do not restore object owners")).changed();
                    changed |= ui.checkbox(&mut self.single_transaction, tr("Restore in a single transaction")).changed();
                }
                changed |= ui.checkbox(&mut self.exit_on_error, tr("Stop at the first error")).changed();
            });
            ui.end_row();
            changed |= tool_row(ui, &mut self.tool_path, &mut self.extra_args);
        });
        changed
    }
}

/// How a tool run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    Succeeded,
    /// The tool reported errors but the run counts as completed: either it
    /// exited successfully, or every error was about a setting the server
    /// does not know (`only_settings`).
    Warnings {
        errors: Vec<String>,
        only_settings: bool,
    },
    Failed(String),
    Cancelled,
}

/// Classify the end of a tool run from its result and output.
pub fn classify(result: &DbResult<()>, lines: &[String]) -> ToolOutcome {
    let errors = error_lines(lines);
    let only_settings = !errors.is_empty() && errors.iter().all(|l| is_unknown_setting_error(l));
    match result {
        Ok(()) if errors.is_empty() => ToolOutcome::Succeeded,
        Ok(()) => ToolOutcome::Warnings { errors, only_settings },
        Err(e) if e.kind == ErrorKind::Cancelled => ToolOutcome::Cancelled,
        Err(_) if only_settings => ToolOutcome::Warnings { errors, only_settings },
        Err(e) => ToolOutcome::Failed(e.full_text()),
    }
}

struct ToolRun {
    op: OpHandle,
    log: Arc<Mutex<Vec<String>>>,
    job: Option<Job<DbResult<()>>>,
    started: Instant,
    ended: Option<Instant>,
    outcome: Option<ToolOutcome>,
    output: Option<PathBuf>,
}

#[derive(Clone, PartialEq)]
struct PreviewKey {
    connected_at: Option<Instant>,
    backup: Option<BackupOptions>,
    restore: Option<RestoreOptions>,
}

struct Preview {
    key: PreviewKey,
    /// (program, display text) or the error.
    result: Result<(String, String), String>,
}

enum ToolVersion {
    Loading(Job<Option<String>>),
    Done(Option<String>),
}

/// Backup (`restore = false`) or restore tab of a connection.
pub struct BackupTab {
    conn: String,
    schema: Option<SchemaRef>,
    restore: bool,
    backup: BackupForm,
    restore_form: RestoreForm,
    defaults_done: bool,
    preview: Option<Preview>,
    versions: HashMap<String, ToolVersion>,
    run: Option<ToolRun>,
    confirm: bool,
    save_prompt: Option<(String, String)>,
    save_job: Option<Job<DbResult<dbiji_tasks::TaskDef>>>,
    saved_task: Option<String>,
    form_error: Option<String>,
}

impl BackupTab {
    pub fn new(conn: String, schema: Option<SchemaRef>, restore: bool) -> Self {
        BackupTab {
            conn,
            schema,
            restore,
            backup: BackupForm::default(),
            restore_form: RestoreForm::default(),
            defaults_done: false,
            preview: None,
            versions: HashMap::new(),
            run: None,
            confirm: false,
            save_prompt: None,
            save_job: None,
            saved_task: None,
            form_error: None,
        }
    }

    /// The backup options form.
    pub fn backup_form_mut(&mut self) -> &mut BackupForm {
        &mut self.backup
    }

    /// The restore options form.
    pub fn restore_form_mut(&mut self) -> &mut RestoreForm {
        &mut self.restore_form
    }

    /// Fill database, schema and output file from the connection (once).
    fn init_defaults(&mut self, cx: &AppContext) {
        if self.defaults_done {
            return;
        }
        let Some(cfg) = cx.conns.config(&self.conn) else { return };
        self.defaults_done = true;
        let flavor = ToolFlavor::of(cfg.kind);
        let live = cx.conns.live(&self.conn);
        let current = live.and_then(|l| l.current_database.clone()).or_else(|| cfg.database.clone()).filter(|d| !d.is_empty());
        let schema_db = self.schema.as_ref().and_then(|s| s.database.clone()).filter(|d| !d.is_empty());
        let schema_name = self.schema.as_ref().and_then(|s| s.schema.clone()).filter(|s| !s.is_empty());
        let database = match flavor {
            ToolFlavor::MySql => schema_name.clone().or(current),
            _ => schema_db.or(current),
        };
        self.backup.database = database.clone().unwrap_or_default();
        self.restore_form.database = database.clone().unwrap_or_default();
        if flavor == ToolFlavor::Postgres
            && let Some(s) = schema_name
        {
            self.backup.schemas = s;
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M");
        let base = format!("{}-{}-{stamp}", file_safe(&cfg.name), file_safe(database.as_deref().unwrap_or("all")));
        let ext = format_extension(self.backup.format);
        self.backup.output = home_dir().join(if ext.is_empty() { base } else { format!("{base}.{ext}") }).display().to_string();
    }

    fn flavor(&self, cx: &AppContext) -> ToolFlavor {
        cx.conns.config(&self.conn).map(|c| ToolFlavor::of(c.kind)).unwrap_or(ToolFlavor::Other)
    }

    fn key(&self, cx: &AppContext) -> PreviewKey {
        PreviewKey {
            connected_at: cx.conns.live(&self.conn).map(|l| l.connected_at),
            backup: (!self.restore).then(|| self.backup.to_options()),
            restore: self.restore.then(|| self.restore_form.to_options()),
        }
    }

    /// Build the command with the live connection's driver and parameters.
    fn spec(&self, cx: &AppContext) -> DbResult<CommandSpec> {
        let live = cx.conns.live(&self.conn).ok_or_else(|| DbError::connection(tr("Connect first: the tools use the connection's settings")))?;
        if self.restore {
            live.driver.restore_command(live.params(), &self.restore_form.to_options())
        } else {
            live.driver.backup_command(live.params(), &self.backup.to_options())
        }
    }

    /// Command line that will run (secrets are never part of it).
    pub fn command_line(&mut self, cx: &AppContext) -> Result<String, String> {
        self.refresh_preview(cx);
        match self.preview.as_ref().map(|p| &p.result) {
            Some(Ok((_, display))) => Ok(display.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(tr("Connect first: the tools use the connection's settings").to_string()),
        }
    }

    fn refresh_preview(&mut self, cx: &AppContext) {
        let key = self.key(cx);
        if self.preview.as_ref().is_some_and(|p| p.key == key) {
            return;
        }
        let result = self.spec(cx).map(|s| (s.program.clone(), s.display())).map_err(|e| e.message);
        self.preview = Some(Preview { key, result });
    }

    /// Version line of the tool, probed once per program in the background.
    fn tool_version_line(&mut self, cx: &AppContext, program: &str) -> Option<String> {
        let entry = self.versions.entry(program.to_string()).or_insert_with(|| ToolVersion::Loading(cx.backend.spawn(tool_version(program.to_string()))));
        if let ToolVersion::Loading(job) = entry
            && let Some(v) = job.poll()
        {
            *entry = ToolVersion::Done(v);
        }
        match entry {
            ToolVersion::Done(v) => v.clone(),
            ToolVersion::Loading(_) => None,
        }
    }

    /// Tool name, version line and the newer-than-server warning.
    fn tool_status(&mut self, cx: &AppContext) -> Option<(String, Option<String>, Option<String>)> {
        let program = match self.preview.as_ref().map(|p| &p.result) {
            Some(Ok((program, _))) => program.clone(),
            _ => return None,
        };
        let name = std::path::Path::new(&program).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| program.clone());
        let line = self.tool_version_line(cx, &program);
        let warning = match (&line, cx.conns.live(&self.conn)) {
            (Some(l), Some(live)) => newer_tool_warning(&name, l, &live.server),
            _ => None,
        };
        Some((name, line, warning))
    }

    /// Start the tool (backups run immediately, restores after confirmation).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.run.as_ref().is_some_and(|r| r.job.is_some()) {
            return;
        }
        let valid = if self.restore { self.restore_form.validate() } else { self.backup.validate() };
        if let Err(e) = valid {
            self.form_error = Some(e);
            return;
        }
        self.form_error = None;
        let spec = match self.spec(cx) {
            Ok(s) => s,
            Err(e) => {
                self.form_error = Some(e.message);
                return;
            }
        };
        let name = cx.conns.config(&self.conn).map(|c| c.name.clone()).unwrap_or_default();
        let title = if self.restore { trf("Restore into {}", &[&name]) } else { trf("Backup of {}", &[&name]) };
        let op = cx.ops.start(title);
        let mut first = vec![format!("$ {}", spec.display())];
        let program = spec.program.clone();
        let tool = std::path::Path::new(&program).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if let (Some(ToolVersion::Done(Some(line))), Some(live)) = (self.versions.get(&program), cx.conns.live(&self.conn)) {
            first.push(format!("# {line}"));
            if let Some(w) = newer_tool_warning(&tool, line, &live.server) {
                first.push(format!("# {} {w}", tr("Warning:")));
            }
        }
        let log = Arc::new(Mutex::new(first));
        let sink = log.clone();
        let repaint = cx.backend.repainter();
        let progress = op.progress.clone();
        let cancel = op.cancel.clone();
        let job = cx.backend.spawn(async move {
            crate::tools::run_command(spec, cancel, move |line| {
                progress.set_message(line.chars().take(120).collect::<String>());
                let mut l = sink.lock();
                if l.len() < MAX_LOG_LINES {
                    l.push(line);
                } else if l.len() == MAX_LOG_LINES {
                    l.push(tr("… further output is not shown").to_string());
                }
                drop(l);
                repaint.request();
            })
            .await
        });
        let output = (!self.restore).then(|| PathBuf::from(self.backup.output.trim()));
        self.run = Some(ToolRun { op, log, job: Some(job), started: Instant::now(), ended: None, outcome: None, output });
    }

    /// A run is in progress.
    pub fn is_running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.job.is_some())
    }

    /// How the last run ended.
    pub fn outcome(&self) -> Option<&ToolOutcome> {
        self.run.as_ref().and_then(|r| r.outcome.as_ref())
    }

    /// Output of the last run.
    pub fn log_text(&self) -> String {
        self.run.as_ref().map(|r| r.log.lock().join("\n")).unwrap_or_default()
    }

    /// Cancel the running tool.
    pub fn cancel(&mut self) {
        if let Some(r) = &self.run {
            r.op.cancel.cancel();
        }
    }

    fn poll(&mut self, cx: &mut AppContext) {
        if let Some(run) = self.run.as_mut()
            && let Some(result) = run.job.as_mut().and_then(|j| j.poll())
        {
            run.job = None;
            run.ended = Some(Instant::now());
            let lines = run.log.lock().clone();
            let outcome = classify(&result, &lines);
            let state = match &outcome {
                ToolOutcome::Succeeded => OpState::Succeeded(if self.restore { tr("Restore completed").to_string() } else { tr("Backup completed").to_string() }),
                ToolOutcome::Warnings { errors, .. } => OpState::Succeeded(trf("Completed with {} warning(s)", &[&errors.len()])),
                ToolOutcome::Failed(e) => OpState::Failed(e.lines().next().unwrap_or_default().to_string()),
                ToolOutcome::Cancelled => OpState::Cancelled,
            };
            cx.ops.finish(run.op.id, state);
            run.outcome = Some(outcome);
        }
        if let Some(j) = self.save_job.as_mut()
            && let Some(r) = j.poll()
        {
            self.save_job = None;
            match r {
                Ok(t) => {
                    cx.toasts.success(trf("Saved as task \"{}\"", &[&t.name]));
                    self.saved_task = Some(t.name);
                }
                Err(e) => cx.toasts.error(tr("Cannot save the task"), Some(e.full_text())),
            }
        }
    }

    fn header(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let name = cx.conns.config(&self.conn).map(|c| c.name.clone()).unwrap_or_else(|| self.conn.clone());
        ui.horizontal(|ui| {
            ui.heading(if self.restore { tr("Restore") } else { tr("Backup") });
            ui.label(RichText::new(name).strong());
            if let Some(live) = cx.conns.live(&self.conn) {
                ui.weak(format!("{} {}", live.server.product, live.server.version.split_whitespace().next().unwrap_or("")));
            }
        });
        if !cx.conns.is_connected(&self.conn) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::INFO).color(crate::theme::warning_color(ui.visuals().dark_mode)));
                ui.label(tr("Connect to build the command: the tools use the connection's host, user and password."));
                if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                    cx.push(AppCommand::Connect(self.conn.clone()));
                }
            });
        } else if let Some(live) = cx.conns.live(&self.conn) {
            let caps = &live.driver.info().capabilities;
            let supported = if self.restore { caps.restore } else { caps.backup };
            if !supported {
                crate::widgets::error_box(ui, if self.restore { tr("Restore is not supported for this database.") } else { tr("Backup is not supported for this database.") });
            }
        }
    }

    fn command_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.refresh_preview(cx);
        ui.strong(tr("Command"));
        match self.preview.as_ref().map(|p| p.result.clone()) {
            Some(Ok((_, display))) => {
                ui.horizontal(|ui| {
                    if ui.small_button(icon::COPY).on_hover_text(tr("Copy")).clicked() {
                        ui.ctx().copy_text(display.clone());
                    }
                    ui.add(egui::Label::new(RichText::new(&display).monospace()).wrap().selectable(true));
                });
                ui.weak(tr("The password is passed through the environment and is not part of the command line."));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(crate::theme::error_color(ui.visuals().dark_mode)));
            }
            None => {}
        }
        if let Some((tool, line, warning)) = self.tool_status(cx) {
            ui.horizontal(|ui| {
                ui.weak(tr("Tool:"));
                match &line {
                    Some(l) => ui.weak(l.as_str()),
                    None => {
                        if self.versions.values().any(|v| matches!(v, ToolVersion::Loading(_))) {
                            ui.spinner();
                            ui.ctx().request_repaint_after(Duration::from_millis(200));
                            ui.weak(tool.as_str())
                        } else {
                            ui.label(
                                RichText::new(trf("{} was not found. Install the client tools or set the tool path.", &[&tool]))
                                    .color(crate::theme::error_color(ui.visuals().dark_mode)),
                            )
                        }
                    }
                };
            });
            if let Some(w) = warning {
                warning_box(ui, &w);
            }
        }
    }

    fn run_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let running = self.is_running();
        ui.horizontal(|ui| {
            let label = if self.restore { format!("{} {}", icon::ARROW_COUNTER_CLOCKWISE, tr("Start restore")) } else { format!("{} {}", icon::PLAY, tr("Start backup")) };
            let ready = cx.conns.is_connected(&self.conn) && matches!(self.preview.as_ref().map(|p| &p.result), Some(Ok(_)));
            if ui.add_enabled(!running && ready, egui::Button::new(label)).clicked() {
                if self.restore {
                    self.confirm = true;
                } else {
                    self.start(cx);
                }
            }
            if running && ui.button(format!("{} {}", icon::STOP, tr("Cancel"))).clicked() {
                self.cancel();
            }
            if ui.add_enabled(!running, egui::Button::new(format!("{} {}", icon::FLOPPY_DISK, tr("Save as task…")))).clicked() {
                let name = cx.conns.config(&self.conn).map(|c| c.name.clone()).unwrap_or_default();
                let default = if self.restore { trf("Restore into {}", &[&name]) } else { trf("Backup of {}", &[&name]) };
                self.save_prompt = Some((default, String::new()));
            }
            if self.saved_task.is_some() && ui.link(tr("Open tasks")).clicked() {
                cx.push(AppCommand::OpenTasks);
            }
        });
        if let Some(e) = &self.form_error {
            crate::widgets::error_box(ui, e);
        }
        let Some(run) = self.run.as_ref() else { return };
        let dark = ui.visuals().dark_mode;
        let elapsed = run.ended.unwrap_or_else(Instant::now).duration_since(run.started);
        ui.horizontal(|ui| {
            match &run.outcome {
                None => {
                    ui.spinner();
                    ui.label(trf("Running… {}", &[&crate::widgets::format_duration(elapsed)]));
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                Some(ToolOutcome::Succeeded) => {
                    let text = match &run.output {
                        Some(p) => trf("Backup written to {}", &[&p.display()]),
                        None => tr("Restore completed").to_string(),
                    };
                    ui.label(RichText::new(format!("{} {text}", icon::CHECK_CIRCLE)).color(crate::theme::success_color(dark)));
                    ui.weak(crate::widgets::format_duration(elapsed));
                }
                Some(ToolOutcome::Warnings { errors, only_settings }) => {
                    let text = if *only_settings {
                        tr("Completed with warnings: the server does not know some settings written by a newer tool version; they were skipped.").to_string()
                    } else {
                        trf("Completed, but the tool reported {} error(s).", &[&errors.len()])
                    };
                    ui.label(RichText::new(format!("{} {text}", icon::WARNING)).color(crate::theme::warning_color(dark)));
                }
                Some(ToolOutcome::Failed(_)) => {
                    ui.label(RichText::new(format!("{} {}", icon::X_CIRCLE, tr("The tool failed"))).color(crate::theme::error_color(dark)));
                }
                Some(ToolOutcome::Cancelled) => {
                    ui.label(RichText::new(format!("{} {}", icon::PROHIBIT, tr("Cancelled"))).color(crate::theme::warning_color(dark)));
                }
            }
            if let (Some(ToolOutcome::Succeeded | ToolOutcome::Warnings { .. }), Some(p)) = (&run.outcome, &run.output) {
                let dir = if p.is_dir() { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) };
                if let Some(dir) = dir
                    && ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Open folder"))).clicked()
                {
                    cx.backend.spawn_detached(async move {
                        let _ = open::that_detached(dir);
                    });
                }
            }
        });
        if let Some(ToolOutcome::Failed(e)) = &run.outcome {
            crate::widgets::error_box(ui, e);
        }
        let lines = run.log.lock().clone();
        egui::ScrollArea::both().id_salt(("backup_log", &self.conn)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for l in &lines {
                let lower = l.to_ascii_lowercase();
                let mut t = RichText::new(l).monospace();
                if lower.contains("error") && !lower.contains("errors ignored") {
                    t = t.color(if is_unknown_setting_error(l) { crate::theme::warning_color(dark) } else { crate::theme::error_color(dark) });
                } else if lower.contains("warning") {
                    t = t.color(crate::theme::warning_color(dark));
                }
                ui.add(egui::Label::new(t).extend());
            }
        });
    }

    fn dialogs(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.confirm {
            let mut close = false;
            let target = cx.conns.config(&self.conn).map(|c| c.name.clone()).unwrap_or_default();
            let db = Some(self.restore_form.database.trim().to_string()).filter(|d| !d.is_empty()).unwrap_or_else(|| tr("(connection database)").to_string());
            let modal = egui::Modal::new(egui::Id::new(("confirm_restore", &self.conn))).show(ui.ctx(), |ui| {
                ui.heading(tr("Restore the backup"));
                ui.label(trf("Restore {} into \"{}\" / {}?", &[&self.restore_form.input, &target, &db]));
                ui.label(tr("Existing objects can be dropped or overwritten."));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Restore")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        self.start(cx);
                        close = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.confirm = false;
            }
        }
        if let Some((mut name, mut folder)) = self.save_prompt.take() {
            let mut keep = true;
            let modal = egui::Modal::new(egui::Id::new(("save_backup_task", &self.conn))).show(ui.ctx(), |ui| {
                ui.heading(tr("Save as task"));
                egui::Grid::new("save_task_grid").num_columns(2).show(ui, |ui| {
                    ui.label(tr("Name"));
                    ui.add(egui::TextEdit::singleline(&mut name).desired_width(260.0));
                    ui.end_row();
                    ui.label(tr("Folder"));
                    ui.add(egui::TextEdit::singleline(&mut folder).desired_width(260.0).hint_text(tr("(top level)")));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!name.trim().is_empty(), egui::Button::new(tr("Save"))).clicked() {
                        self.save_as_task(cx, name.trim().to_string(), folder.trim().to_string());
                        keep = false;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        keep = false;
                    }
                });
            });
            if keep && !modal.should_close() {
                self.save_prompt = Some((name, folder));
            }
        }
    }

    fn save_as_task(&mut self, cx: &mut AppContext, name: String, folder: String) {
        let Some(store) = cx.services.tasks.clone() else {
            cx.toasts.error(tr("The task store is not available"), None);
            return;
        };
        let kind = if self.restore {
            dbiji_tasks::TaskKind::Restore { connection_id: self.conn.clone(), options: self.restore_form.to_options() }
        } else {
            dbiji_tasks::TaskKind::Backup { connection_id: self.conn.clone(), options: self.backup.to_options() }
        };
        let mut task = dbiji_tasks::TaskDef::new(name, kind);
        task.folder = Some(folder).filter(|f| !f.is_empty());
        self.save_job = Some(spawn_blocking(cx.backend, move || store.create(task)));
    }
}

fn warning_box(ui: &mut Ui, text: &str) {
    let color = crate::theme::warning_color(ui.visuals().dark_mode);
    egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, color)).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(icon::WARNING).color(color));
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
        });
    });
}

impl TabView for BackupTab {
    fn title(&self) -> String {
        if self.restore { tr("Restore").to_string() } else { tr("Backup").to_string() }
    }
    fn icon(&self) -> &'static str {
        if self.restore { icon::ARROW_COUNTER_CLOCKWISE } else { icon::ARCHIVE }
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.init_defaults(cx);
        self.poll(cx);
        self.header(ui, cx);
        ui.separator();
        let flavor = self.flavor(cx);
        let running = self.is_running();
        egui::Panel::bottom(egui::Id::new(("backup_run", &self.conn, self.restore)))
            .resizable(true)
            .default_size(ui.available_height() * 0.4)
            .size_range(120.0..=f32::INFINITY)
            .show(ui, |ui| {
                ui.add_space(4.0);
                self.command_ui(ui, cx);
                ui.add_space(4.0);
                self.run_ui(ui, cx);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("backup_form_scroll", &self.conn)).auto_shrink([false, false]).show(ui, |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    if self.restore {
                        self.restore_form.ui(ui, flavor, "restore_tab");
                    } else {
                        self.backup.ui(ui, flavor, "backup_tab");
                    }
                });
            });
        });
        self.dialogs(ui, cx);
        if self.save_job.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        let _ = cx;
        // a running tool is cancelled when the tab goes away
        self.cancel();
        true
    }

    crate::impl_tab_any!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_roundtrip() {
        let o = BackupOptions {
            output: "/tmp/x.dump".into(),
            format: BackupFormat::Custom,
            database: Some("db".into()),
            schemas: vec!["public".into(), "sales".into()],
            tables: vec!["public.t".into()],
            schema_only: true,
            compression: Some(5),
            tool_path: Some("/opt/pg/bin".into()),
            extra_args: vec!["--lock-wait-timeout=10s".into(), "a b".into()],
            ..Default::default()
        };
        assert_eq!(BackupForm::from_options(&o).to_options(), o);
        let r = RestoreOptions { input: "/tmp/x.dump".into(), format: BackupFormat::Custom, clean: true, exit_on_error: true, ..Default::default() };
        assert_eq!(RestoreForm::from_options(&r).to_options(), r);
        assert!(!RestoreForm::default().exit_on_error);
        assert_eq!(retarget_extension_for(ToolFlavor::Other, "/tmp/a.sql", BackupFormat::Plain, BackupFormat::Custom), "/tmp/a.dump");
        assert_eq!(retarget_extension_for(ToolFlavor::Other, "/tmp/a.dump", BackupFormat::Custom, BackupFormat::Directory), "/tmp/a");
        assert_eq!(retarget_extension_for(ToolFlavor::Other, "/tmp/a", BackupFormat::Directory, BackupFormat::Tar), "/tmp/a.tar");
        assert_eq!(retarget_extension_for(ToolFlavor::Other, "/tmp/a.bak", BackupFormat::Plain, BackupFormat::Custom), "/tmp/a.bak");
        assert_eq!(retarget_extension_for(ToolFlavor::Sqlite, "/tmp/a.sql", BackupFormat::Plain, BackupFormat::Custom), "/tmp/a.db");
        assert_eq!(retarget_extension_for(ToolFlavor::Sqlite, "/tmp/a.db", BackupFormat::Custom, BackupFormat::Plain), "/tmp/a.sql");
    }

    #[test]
    fn outcome_classification() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let failed: DbResult<()> = Err(DbError::io("pg_restore exited with exit status: 1"));
        let settings = lines(&[
            "pg_restore: error: could not execute query: ERROR:  unrecognized configuration parameter \"transaction_timeout\"",
            "Command was: SET transaction_timeout = 0;",
        ]);
        assert!(matches!(classify(&failed, &settings), ToolOutcome::Warnings { only_settings: true, .. }));
        let real = lines(&["pg_restore: error: could not execute query: ERROR:  relation \"t\" already exists"]);
        assert!(matches!(classify(&failed, &real), ToolOutcome::Failed(_)));
        assert!(matches!(classify(&Ok(()), &real), ToolOutcome::Warnings { only_settings: false, .. }));
        assert_eq!(classify(&Ok(()), &lines(&["pg_dump: dumping contents of table"])), ToolOutcome::Succeeded);
        assert_eq!(classify(&Err(DbError::cancelled()), &settings), ToolOutcome::Cancelled);
    }
}
