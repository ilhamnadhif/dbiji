//! Table migration between connections (FR-T3): source tables, target
//! connection and schema, table / column names and type overrides, table
//! preparation, indexes and foreign keys; runs with per-table progress and
//! can be saved as a task.

use super::{
    MessageLog, Running, TaskSaver, combo, connection_combo, connection_name, current_schema, error_text, next_uid, not_connected, on_error_options, progress_ui, schema_combo,
    schema_tables, section, steps_ui, table_def_of,
};
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::transferexec::{AppSessionFactory, database_of, migration_task_config};
use dbiji_core::meta::{ObjectRef, SchemaRef};
use dbiji_core::{DbError, DbKind, DbResult, tr, trf};
use dbiji_tasks::TaskKind;
use dbiji_transfer::{MessageLevel, MigrationJob, MigrationMode, MigrationSummary, NameCase, SessionFactory, run_migration};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::collections::BTreeMap;
use std::sync::Arc;

const PAGE_SOURCE: usize = 0;
const PAGE_TARGET: usize = 1;
const PAGE_MAPPING: usize = 2;
const PAGE_OPTIONS: usize = 3;
const PAGE_RUN: usize = 4;

/// One source table with its target names.
#[derive(Clone)]
struct TableEntry {
    enabled: bool,
    source: ObjectRef,
    /// Target table name (empty: the source name with the name case applied).
    target_name: String,
    /// Target column names by source column (empty values are ignored).
    column_names: BTreeMap<String, String>,
    /// Target types by source column (empty values are ignored).
    column_types: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableState {
    Pending,
    Running,
    Done,
    Failed,
}

/// Live state of one table during a run.
struct TableRun {
    name: String,
    state: TableState,
    rows: u64,
    base: u64,
}

/// The migration tab.
pub struct MigrationTab {
    uid: u64,
    conn: String,
    page: usize,
    tables: Vec<TableEntry>,
    picker_schema: Option<SchemaRef>,
    picker_filter: String,
    target_conn: Option<String>,
    target_schema: Option<SchemaRef>,
    /// Options (tables and names are built from `tables`).
    options: MigrationJob,
    run: Option<Running<MigrationSummary>>,
    table_runs: Vec<TableRun>,
    log: MessageLog,
    result: Option<Result<MigrationSummary, String>>,
    error: Option<String>,
    confirm: bool,
    saver: TaskSaver,
}

fn apply_case(case: NameCase, s: &str) -> String {
    match case {
        NameCase::Keep => s.to_string(),
        NameCase::Lower => s.to_lowercase(),
        NameCase::Upper => s.to_uppercase(),
    }
}

impl MigrationTab {
    /// Migrate `objects` (tables) of `conn` to another connection.
    pub fn new(conn: String, objects: Vec<ObjectRef>) -> Self {
        let mut picker_schema = None;
        let mut tables = Vec::new();
        for o in objects {
            if o.kind == dbiji_core::meta::ObjectKind::Schema {
                picker_schema = Some(SchemaRef::new(o.database.clone(), Some(o.name.clone())));
            } else if o.kind.is_table_like() {
                if picker_schema.is_none() {
                    picker_schema = Some(o.schema_ref()).filter(|s| s.schema.is_some());
                }
                tables.push(TableEntry { enabled: true, source: o, target_name: String::new(), column_names: BTreeMap::new(), column_types: BTreeMap::new() });
            }
        }
        MigrationTab {
            uid: next_uid(),
            conn,
            page: PAGE_SOURCE,
            tables,
            picker_schema,
            picker_filter: String::new(),
            target_conn: None,
            target_schema: None,
            options: MigrationJob::default(),
            run: None,
            table_runs: Vec::new(),
            log: MessageLog::default(),
            result: None,
            error: None,
            confirm: false,
            saver: TaskSaver::default(),
        }
    }

    // ------------------------------------------------------------ public API

    /// Choose the target connection and schema (`None`: its default schema).
    pub fn set_target(&mut self, conn: String, schema: Option<String>) {
        self.target_conn = Some(conn);
        self.target_schema = schema.map(|s| SchemaRef::new(None, Some(s)));
    }

    /// Rename the target table of `source` (by source table name).
    pub fn set_target_name(&mut self, source: &str, target: &str) {
        if let Some(t) = self.tables.iter_mut().find(|t| t.source.name == source) {
            t.target_name = target.to_string();
        }
    }

    /// Override the target type of a column (`""` removes the override).
    pub fn set_column_type(&mut self, table: &str, column: &str, data_type: &str) {
        if let Some(t) = self.tables.iter_mut().find(|t| t.source.name == table) {
            if data_type.trim().is_empty() {
                t.column_types.remove(column);
            } else {
                t.column_types.insert(column.to_string(), data_type.trim().to_string());
            }
        }
    }

    /// Add a source table.
    pub fn add_table(&mut self, table: ObjectRef) {
        if !self.tables.iter().any(|t| t.source == table) {
            self.tables.push(TableEntry { enabled: true, source: table, target_name: String::new(), column_names: BTreeMap::new(), column_types: BTreeMap::new() });
        }
    }

    /// Options (mode, batch size, indexes, foreign keys, transactions, errors).
    pub fn options_mut(&mut self) -> &mut MigrationJob {
        &mut self.options
    }

    /// The job as it would run now.
    pub fn job(&self) -> MigrationJob {
        let mut job = self.options.clone();
        let enabled: Vec<&TableEntry> = self.tables.iter().filter(|t| t.enabled).collect();
        job.tables = enabled.iter().map(|t| t.source.clone()).collect();
        job.target_schema = self.target_schema.as_ref().and_then(|s| s.schema.clone());
        job.table_names = enabled.iter().filter(|t| !t.target_name.trim().is_empty()).map(|t| (t.source.name.clone(), t.target_name.trim().to_string())).collect();
        job.column_names = enabled
            .iter()
            .map(|t| {
                let names: BTreeMap<String, String> = t.column_names.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| (k.clone(), v.trim().to_string())).collect();
                (t.source.name.clone(), names)
            })
            .filter(|(_, m)| !m.is_empty())
            .collect();
        job.column_types = enabled
            .iter()
            .flat_map(|t| t.column_types.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(c, v)| (format!("{}.{c}", t.source.name), v.trim().to_string())))
            .collect();
        job
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn set_page(&mut self, page: usize) {
        self.page = page.min(PAGE_RUN);
    }

    /// Run the migration (the "Run" button without the confirmation).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.run.is_some() {
            return;
        }
        if let Err(e) = self.validate(cx) {
            self.error = Some(e);
            return;
        }
        let Some(target) = self.target_conn.clone() else { return };
        if let Some(cfg) = cx.conns.config(&target)
            && cfg.read_only
        {
            self.error = Some(trf("The connection \"{}\" is read-only.", &[&cfg.name]));
            return;
        }
        for c in [self.conn.clone(), target.clone()] {
            if cx.conns.live(&c).is_none() {
                self.error = Some(not_connected(cx, &c));
                return;
            }
        }
        let job = self.job();
        let (Some(src_live), Some(dst_live)) = (cx.conns.live(&self.conn), cx.conns.live(&target)) else { return };
        let src_db = database_of(job.tables.iter());
        let dst_db = self.target_schema.as_ref().and_then(|s| s.database.clone());
        let src: Arc<dyn SessionFactory> = Arc::new(AppSessionFactory::from_live(src_live, src_db.as_deref()));
        let dst: Arc<dyn SessionFactory> = Arc::new(AppSessionFactory::from_live(dst_live, dst_db.as_deref()));
        self.table_runs = job.tables.iter().map(|t| TableRun { name: t.display_path(), state: TableState::Pending, rows: 0, base: 0 }).collect();
        self.error = None;
        self.result = None;
        self.log.clear();
        self.page = PAGE_RUN;
        let title = trf("Migrate {} table(s) to {}", &[&job.tables.len(), &connection_name(cx, &target)]);
        self.run = Some(Running::spawn(cx, title, move |progress, cancel| async move { run_migration(job, src, dst, &progress, &cancel).await }));
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub fn cancel(&self) {
        if let Some(r) = &self.run {
            r.cancel();
        }
    }

    pub fn last_summary(&self) -> Option<&MigrationSummary> {
        self.result.as_ref().and_then(|r| r.as_ref().ok())
    }

    pub fn last_error(&self) -> Option<&str> {
        match &self.result {
            Some(Err(e)) => Some(e),
            _ => self.error.as_deref(),
        }
    }

    /// Save the current configuration as a task (in the background).
    pub fn save_task(&mut self, cx: &mut AppContext, name: &str, folder: Option<String>) {
        match self.task_kind(cx) {
            Ok(kind) => self.saver.save(cx, name.to_string(), folder, kind),
            Err(e) => self.saver.fail(e.full_text()),
        }
    }

    pub fn saved_task_id(&self) -> Option<&str> {
        self.saver.saved.as_deref()
    }

    // --------------------------------------------------------------- helpers

    fn task_kind(&self, cx: &AppContext) -> DbResult<TaskKind> {
        self.validate(cx).map_err(DbError::config)?;
        let target = self.target_conn.clone().ok_or_else(|| DbError::config(tr("Choose the target connection.")))?;
        Ok(TaskKind::Migration { source_connection_id: self.conn.clone(), target_connection_id: target, config: migration_task_config(&self.job())? })
    }

    fn tables_error(&self) -> Option<String> {
        if !self.tables.iter().any(|t| t.enabled) {
            return Some(tr("Choose at least one table to migrate.").to_string());
        }
        None
    }

    fn target_error(&self, cx: &AppContext) -> Option<String> {
        let Some(target) = &self.target_conn else { return Some(tr("Choose the target connection.").to_string()) };
        if *target == self.conn {
            // Same connection: the target must not be the source table itself.
            let default = current_schema(cx, &self.conn).and_then(|s| s.schema);
            let target_schema = self.target_schema.as_ref().and_then(|s| s.schema.clone()).or(default.clone());
            for t in self.tables.iter().filter(|t| t.enabled) {
                let source_schema = t.source.schema.clone().or(default.clone());
                let name = if t.target_name.trim().is_empty() { apply_case(self.options.name_case, &t.source.name) } else { t.target_name.trim().to_string() };
                if source_schema == target_schema && name == t.source.name {
                    return Some(trf("The target of {} is the table itself; choose another schema or name.", &[&t.source.display_path()]));
                }
            }
        }
        None
    }

    fn validate(&self, cx: &AppContext) -> Result<(), String> {
        match self.tables_error().or_else(|| self.target_error(cx)) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn reachable(&self, cx: &AppContext) -> usize {
        if self.tables_error().is_some() {
            PAGE_SOURCE
        } else if self.target_error(cx).is_some() {
            PAGE_TARGET
        } else {
            PAGE_RUN
        }
    }

    fn destructive(&self) -> bool {
        matches!(self.options.mode, MigrationMode::Truncate | MigrationMode::DropAndCreate)
    }

    fn poll(&mut self, cx: &mut AppContext) {
        let mut finished = None;
        if let Some(run) = &mut self.run {
            let messages = run.progress.take_messages();
            let active = run.progress.active_items();
            let rows = run.progress.rows();
            for t in &mut self.table_runs {
                let is_active = active.contains(&t.name);
                match t.state {
                    TableState::Pending if is_active => {
                        t.state = TableState::Running;
                        t.base = rows;
                    }
                    TableState::Running if is_active => t.rows = rows.saturating_sub(t.base),
                    TableState::Running => {
                        t.state = TableState::Done;
                        t.rows = rows.saturating_sub(t.base);
                    }
                    _ => {}
                }
            }
            for m in &messages {
                if m.level == MessageLevel::Error
                    && let Some(item) = &m.item
                    && let Some(t) = self.table_runs.iter_mut().find(|t| &t.name == item)
                {
                    t.state = TableState::Failed;
                }
            }
            self.log.push(messages);
            if let Some(result) = run.poll(cx, describe) {
                self.log.drain(&run.progress);
                finished = Some(result);
            }
        }
        if let Some(result) = finished {
            self.run = None;
            match result {
                Ok(summary) => {
                    let failed = summary.tables.iter().filter(|t| t.error.is_some()).count();
                    if failed > 0 {
                        cx.toasts.warning(trf("Migration finished, {} table(s) failed", &[&failed]), None);
                    } else {
                        cx.toasts.success(describe(&summary));
                    }
                    if let Some(target) = &self.target_conn {
                        let schema = self.target_schema.clone().or_else(|| current_schema(cx, target));
                        cx.push(crate::command::AppCommand::RefreshMetadata { connection: target.clone(), object: None, schema });
                    }
                    self.result = Some(Ok(summary));
                }
                Err(e) => {
                    if e.kind != dbiji_core::ErrorKind::Cancelled {
                        cx.toasts.error(tr("Migration failed"), Some(e.full_text()));
                    }
                    self.result = Some(Err(error_text(&e)));
                }
            }
        }
        self.saver.poll(cx);
    }

    // ------------------------------------------------------------------ pages

    fn source_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Tables to migrate"));
        if self.tables.is_empty() {
            ui.weak(tr("No table selected yet: add tables below."));
        }
        let mut remove = None;
        egui::Grid::new(("mig_tables", self.uid)).num_columns(3).spacing([8.0, 4.0]).striped(true).show(ui, |ui| {
            for (i, t) in self.tables.iter_mut().enumerate() {
                ui.checkbox(&mut t.enabled, "");
                ui.label(format!("{} {}", icon::TABLE, t.source.display_path()));
                if ui.small_button(dbiji_icons::ic(icon::TRASH)).on_hover_text(tr("Remove")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.tables.remove(i);
        }
        section(ui, tr("Add tables"));
        let conn = self.conn.clone();
        if cx.conns.live(&conn).is_none() {
            ui.horizontal(|ui| {
                ui.weak(tr("Connect to list the tables."));
                if ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked() {
                    cx.push(crate::command::AppCommand::Connect(conn.clone()));
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label(tr("Schema"));
            schema_combo(ui, cx, &conn, ("mig_schema", self.uid), &mut self.picker_schema);
            ui.add(egui::TextEdit::singleline(&mut self.picker_filter).hint_text(tr("Filter…")).desired_width(160.0));
        });
        let Some(schema) = self.picker_schema.clone().or_else(|| current_schema(cx, &conn)) else { return };
        match schema_tables(cx, &conn, &schema, false) {
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
                ui.horizontal(|ui| {
                    ui.weak(trf("{} tables", &[&shown.len()]));
                    if !shown.is_empty() && ui.button(tr("Add all shown")).clicked() {
                        for o in &shown {
                            self.add_table(o.clone());
                        }
                    }
                });
                egui::ScrollArea::vertical().id_salt(("mig_pick", self.uid)).max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
                    for o in &shown {
                        let already = self.tables.iter().any(|t| t.source == *o);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!already, egui::Button::new(dbiji_icons::ic(icon::PLUS)).small()).clicked() {
                                self.add_table(o.clone());
                            }
                            let text = RichText::new(format!("{} {}", icon::TABLE, o.name));
                            ui.label(if already { text.weak() } else { text });
                        });
                    }
                });
            }
        }
    }

    fn target_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Target"));
        let id = self.uid;
        egui::Grid::new(("mig_target", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Connection"));
            ui.horizontal(|ui| {
                if connection_combo(ui, cx, ("mig_conn", id), &mut self.target_conn) {
                    self.target_schema = None;
                }
                if let Some(t) = self.target_conn.clone()
                    && !cx.conns.is_connected(&t)
                    && ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked()
                {
                    cx.push(crate::command::AppCommand::Connect(t));
                }
            });
            ui.end_row();
            if let Some(t) = self.target_conn.clone()
                && cx.conns.is_connected(&t)
            {
                ui.label(tr("Schema"));
                schema_combo(ui, cx, &t, ("mig_tschema", id), &mut self.target_schema);
                ui.end_row();
            }
            ui.label(tr("Names"));
            combo(
                ui,
                ("mig_case", id),
                &mut self.options.name_case,
                &[(NameCase::Keep, tr("Keep them as they are").to_string()), (NameCase::Lower, tr("lower case").to_string()), (NameCase::Upper, tr("UPPER CASE").to_string())],
            );
            ui.end_row();
        });
        if let (Some(t), Some(src)) = (&self.target_conn, cx.conns.config(&self.conn).map(|c| c.kind)) {
            let dst = cx.conns.config(t).map(|c| c.kind);
            if let Some(dst) = dst
                && dst != src
            {
                ui.weak(trf("Column types are converted from {} to {}; check them on the next page.", &[&src.display_name(), &dst.display_name()]));
            }
        }
        if let Some(e) = self.target_error(cx) {
            ui.add_space(4.0);
            crate::widgets::error_box(ui, &e);
        }
    }

    fn mapping_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Tables and columns"));
        let id = self.uid;
        let src_kind = cx.conns.config(&self.conn).map(|c| c.kind).unwrap_or(DbKind::Generic);
        let dst_kind = self.target_conn.as_ref().and_then(|t| cx.conns.config(t)).map(|c| c.kind).unwrap_or(DbKind::Generic);
        let dst = cx.conns.registry.dialect(dst_kind);
        let same = src_kind == dst.kind() && src_kind != DbKind::Generic;
        let case = self.options.name_case;
        let conn = self.conn.clone();
        ui.weak(tr("Leave a field empty to keep the default shown in grey."));
        for (i, t) in self.tables.iter_mut().enumerate().filter(|(_, t)| t.enabled) {
            let default_name = apply_case(case, &t.source.name);
            let title = if t.target_name.trim().is_empty() {
                format!("{} → {default_name}", t.source.display_path())
            } else {
                format!("{} → {}", t.source.display_path(), t.target_name.trim())
            };
            egui::CollapsingHeader::new(title).id_salt(("mig_map", id, i)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr("Target table"));
                    ui.add(egui::TextEdit::singleline(&mut t.target_name).hint_text(default_name.clone()).desired_width(220.0));
                });
                match table_def_of(cx, &conn, &t.source) {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(tr("Loading…"));
                        });
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    }
                    Some(Err(e)) => crate::widgets::error_box(ui, &e.full_text()),
                    Some(Ok(def)) => {
                        egui::Grid::new(("mig_cols", id, i)).num_columns(4).spacing([10.0, 4.0]).striped(true).show(ui, |ui| {
                            ui.strong(tr("Column"));
                            ui.strong(tr("Source type"));
                            ui.strong(tr("Target column"));
                            ui.strong(tr("Target type"));
                            ui.end_row();
                            for c in &def.columns {
                                ui.label(&c.name);
                                ui.weak(&c.data_type);
                                let name = t.column_names.entry(c.name.clone()).or_default();
                                ui.add(egui::TextEdit::singleline(name).hint_text(apply_case(case, &c.name)).desired_width(160.0));
                                let mapped = if same { c.data_type.clone() } else { dst.ddl().map_type(src_kind, &c.data_type, c.kind) };
                                let ty = t.column_types.entry(c.name.clone()).or_default();
                                ui.add(egui::TextEdit::singleline(ty).hint_text(mapped).desired_width(180.0));
                                ui.end_row();
                            }
                        });
                    }
                }
            });
        }
    }

    fn options_page(&mut self, ui: &mut Ui) {
        section(ui, tr("Options"));
        let id = self.uid;
        let o = &mut self.options;
        egui::Grid::new(("mig_opts", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Target tables"));
            ui.checkbox(&mut o.create_tables, tr("Create the tables that do not exist"));
            ui.end_row();
            ui.label(tr("Existing rows"));
            combo(
                ui,
                ("mig_mode", id),
                &mut o.mode,
                &[
                    (MigrationMode::Append, tr("Keep them and add the new rows").to_string()),
                    (MigrationMode::Truncate, tr("Delete them first").to_string()),
                    (MigrationMode::Replace, tr("Insert or replace rows by primary key").to_string()),
                    (MigrationMode::DropAndCreate, tr("Drop and create the tables again").to_string()),
                ],
            );
            ui.end_row();
            ui.label(tr("Rows per batch"));
            ui.add(egui::DragValue::new(&mut o.batch_size).range(1..=100_000));
            ui.end_row();
            ui.label(tr("Copy"));
            ui.horizontal(|ui| {
                ui.checkbox(&mut o.copy_indexes, tr("Indexes"));
                ui.checkbox(&mut o.copy_foreign_keys, tr("Foreign keys"));
            });
            ui.end_row();
            ui.label(tr("Transactions"));
            ui.checkbox(&mut o.transaction_per_table, tr("Load every table in one transaction"));
            ui.end_row();
            ui.label(tr("On error"));
            combo(ui, ("mig_err", id), &mut o.on_error, &on_error_options());
            ui.end_row();
            ui.label(tr("Failed tables"));
            ui.checkbox(&mut o.continue_on_table_error, tr("Continue with the next table"));
            ui.end_row();
        });
    }

    fn run_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let dark = ui.visuals().dark_mode;
        if let Some(run) = &self.run {
            section(ui, tr("Migrating…"));
            if progress_ui(ui, &run.progress, run.is_cancelling()) {
                run.cancel();
            }
            egui::Grid::new(("mig_live", self.uid)).num_columns(3).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
                ui.strong(tr("Table"));
                ui.strong(tr("Status"));
                ui.strong(tr("Rows"));
                ui.end_row();
                for t in &self.table_runs {
                    ui.label(&t.name);
                    match t.state {
                        TableState::Pending => ui.weak(tr("waiting")),
                        TableState::Running => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(tr("copying"))
                            })
                            .inner
                        }
                        TableState::Done => ui.label(RichText::new(tr("done")).color(crate::theme::success_color(dark))),
                        TableState::Failed => ui.label(RichText::new(tr("failed")).color(crate::theme::error_color(dark))),
                    };
                    ui.label(t.rows.to_string());
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            self.log.ui(ui, ("mig_log", self.uid), 220.0);
            return;
        }
        match &self.result {
            Some(Ok(summary)) => {
                let summary = summary.clone();
                self.result_ui(ui, &summary);
            }
            Some(Err(e)) => {
                section(ui, tr("The migration failed"));
                crate::widgets::error_box(ui, e);
                if ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Change settings"))).clicked() {
                    self.result = None;
                    self.page = PAGE_SOURCE;
                }
            }
            None => self.summary_ui(ui, cx),
        }
        if self.result.is_some() {
            ui.add_space(6.0);
            self.log.ui(ui, ("mig_log", self.uid), 220.0);
        }
    }

    fn summary_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        section(ui, tr("Summary"));
        let job = self.job();
        egui::Grid::new(("mig_sum", self.uid)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.weak(tr("Source"));
            ui.label(connection_name(cx, &self.conn));
            ui.end_row();
            ui.weak(tr("Target"));
            let target = self.target_conn.as_ref().map(|t| connection_name(cx, t)).unwrap_or_else(|| "—".into());
            let schema = job.target_schema.clone().unwrap_or_else(|| tr("(default)").to_string());
            ui.label(format!("{target} / {schema}"));
            ui.end_row();
            ui.weak(tr("Tables"));
            ui.label(job.tables.iter().map(|t| t.display_path()).collect::<Vec<_>>().join(", "));
            ui.end_row();
        });
        if let Err(e) = self.validate(cx) {
            ui.add_space(4.0);
            crate::widgets::error_box(ui, &e);
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, s: &MigrationSummary) {
        let dark = ui.visuals().dark_mode;
        section(ui, &describe(s));
        egui::Grid::new(("mig_res", self.uid)).num_columns(6).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
            ui.strong(tr("Table"));
            ui.strong(tr("Target"));
            ui.strong(tr("Rows"));
            ui.strong(tr("Failed"));
            ui.strong(tr("Time"));
            ui.strong(tr("Status"));
            ui.end_row();
            for t in &s.tables {
                ui.label(t.source.as_ref().map(ObjectRef::display_path).unwrap_or_default());
                let mut target = t.target.as_ref().map(ObjectRef::display_path).unwrap_or_default();
                if t.created {
                    target = trf("{} (created)", &[&target]);
                }
                ui.label(target);
                ui.label(t.rows_written.to_string());
                ui.label(t.rows_failed.to_string());
                ui.label(crate::widgets::format_duration(std::time::Duration::from_millis(t.duration_ms)));
                match &t.error {
                    Some(e) => {
                        ui.add(egui::Label::new(RichText::new(e).color(crate::theme::error_color(dark))).wrap());
                    }
                    None => {
                        let text = if t.indexes_created > 0 { trf("OK, {} index(es)", &[&t.indexes_created]) } else { tr("OK").to_string() };
                        ui.label(RichText::new(text).color(crate::theme::success_color(dark)));
                    }
                }
                ui.end_row();
            }
        });
        if s.foreign_keys_created > 0 {
            ui.label(trf("{} foreign key(s) created", &[&s.foreign_keys_created]));
        }
        for w in &s.warnings {
            ui.label(RichText::new(format!("{} {w}", icon::WARNING)).color(crate::theme::warning_color(dark)));
        }
        let failed_rows: Vec<String> = s
            .tables
            .iter()
            .flat_map(|t| {
                let name = t.source.as_ref().map(ObjectRef::display_path).unwrap_or_default();
                t.errors.iter().take(20).map(move |e| trf("{}, row {}: {}", &[&name, &e.row, &e.message]))
            })
            .collect();
        if !failed_rows.is_empty() {
            section(ui, tr("Failed rows"));
            for f in failed_rows {
                ui.label(RichText::new(f).small());
            }
        }
        if ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Change settings"))).clicked() {
            self.result = None;
            self.page = PAGE_SOURCE;
        }
    }

    fn footer(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let running = self.run.is_some();
            if ui.add_enabled(self.page > PAGE_SOURCE && !running, egui::Button::new((dbiji_icons::ic(icon::CARET_LEFT), tr("Back")))).clicked() {
                self.page -= 1;
            }
            let reachable = self.reachable(cx);
            if self.page < PAGE_RUN && ui.add_enabled(self.page < reachable && !running, egui::Button::new(format!("{} {}", tr("Next"), icon::CARET_RIGHT))).clicked() {
                self.page += 1;
            }
            ui.separator();
            let ok = reachable == PAGE_RUN && !running;
            if ui.add_enabled(ok, crate::widgets::primary_button(ui, icon::PLAY, tr("Run migration"))).clicked() {
                if self.destructive() {
                    self.confirm = true;
                } else {
                    self.start(cx);
                }
            }
            if ui.add_enabled(ok && !self.saver.is_saving(), egui::Button::new((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save as task…")))).clicked() {
                let target = self.target_conn.as_ref().map(|t| connection_name(cx, t)).unwrap_or_default();
                self.saver.open(trf("Migrate to {}", &[&target]));
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(crate::theme::error_color(ui.visuals().dark_mode)));
            }
        });
        ui.add_space(2.0);
    }

    fn confirm_ui(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        if !self.confirm {
            return;
        }
        let mut close = false;
        let target = self.target_conn.as_ref().map(|t| connection_name(cx, t)).unwrap_or_default();
        let modal = egui::Modal::new(egui::Id::new(("mig_confirm", self.uid))).show(ctx, |ui| {
            ui.heading(tr("Replace the target data?"));
            let text = if self.options.mode == MigrationMode::DropAndCreate {
                trf("Existing target tables on \"{}\" are dropped and created again.", &[&target])
            } else {
                trf("All rows of the existing target tables on \"{}\" are deleted first.", &[&target])
            };
            ui.label(text);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Run")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
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
}

fn describe(s: &MigrationSummary) -> String {
    let failed = s.tables.iter().filter(|t| t.error.is_some()).count();
    if failed > 0 {
        trf("{} rows copied, {} of {} table(s) failed", &[&s.rows, &failed, &s.tables.len()])
    } else {
        trf("{} rows copied into {} table(s)", &[&s.rows, &s.tables.len()])
    }
}

impl TabView for MigrationTab {
    fn popup_size(&self) -> Option<egui::Vec2> {
        Some(egui::vec2(820.0, 600.0))
    }

    fn title(&self) -> String {
        let n = self.tables.iter().filter(|t| t.enabled).count();
        match self.tables.iter().filter(|t| t.enabled).collect::<Vec<_>>().as_slice() {
            [one] => trf("Migrate {}", &[&one.source.name]),
            _ if n == 0 => tr("Migrate tables").to_string(),
            _ => trf("Migrate {} tables", &[&n]),
        }
    }

    fn icon(&self) -> &'static str {
        icon::ARROWS_LEFT_RIGHT
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll(cx);
        let uid = self.uid;
        egui::Panel::top(egui::Id::new(("mig_top", uid))).show(ui, |ui| {
            ui.add_space(4.0);
            // In a pop-up its title bar names the wizard and the connection.
            if !cx.popup {
                ui.horizontal(|ui| {
                    crate::widgets::icon_heading(ui, icon::ARROWS_LEFT_RIGHT, tr("Migrate tables"));
                    ui.weak(connection_name(cx, &self.conn));
                });
            }
            let steps = [tr("Source"), tr("Target"), tr("Mapping"), tr("Options"), tr("Run")];
            let reachable = if self.run.is_some() { self.page } else { self.reachable(cx) };
            if let Some(p) = steps_ui(ui, &steps, self.page, reachable.max(self.page))
                && self.run.is_none()
            {
                self.page = p;
            }
            ui.add_space(2.0);
        });
        egui::Panel::bottom(egui::Id::new(("mig_bottom", uid))).show(ui, |ui| self.footer(ui, cx));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("mig_body", uid)).auto_shrink([false, false]).show(ui, |ui| match self.page {
                PAGE_SOURCE => self.source_page(ui, cx),
                PAGE_TARGET => self.target_page(ui, cx),
                PAGE_MAPPING => self.mapping_page(ui, cx),
                PAGE_OPTIONS => self.options_page(ui),
                _ => self.run_page(ui, cx),
            });
        });
        let ctx = ui.ctx().clone();
        self.confirm_ui(&ctx, cx);
        if let Some((name, folder)) = self.saver.ui(&ctx, egui::Id::new(("mig_save", uid))) {
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
    fn job_from_entries() {
        let mut tab = MigrationTab::new("src".into(), vec![ObjectRef::table(Some("public".into()), "users"), ObjectRef::table(Some("public".into()), "orders")]);
        tab.set_target("dst".into(), Some("archive".into()));
        tab.set_target_name("users", "app_users");
        tab.set_column_type("users", "id", "BIGINT");
        tab.set_column_type("orders", "total", " ");
        let job = tab.job();
        assert_eq!(job.tables.len(), 2);
        assert_eq!(job.target_schema.as_deref(), Some("archive"));
        assert_eq!(job.table_names.get("users").map(String::as_str), Some("app_users"));
        assert!(!job.table_names.contains_key("orders"));
        assert_eq!(job.column_types.get("users.id").map(String::as_str), Some("BIGINT"));
        assert_eq!(job.column_types.len(), 1);
        assert_eq!(apply_case(NameCase::Upper, "ab"), "AB");
    }
}
