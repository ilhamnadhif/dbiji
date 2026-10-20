//! The task editor dialog: general settings, the form of the task type, the
//! schedule with validation and a preview of the next runs, time limit,
//! notifications and the system scheduler option.

use super::draft::{IntervalUnit, KindDraft, ScheduleDraft, ScheduleMode, TaskDraft, kind_icon, kind_label, weekday_names};
use super::system;
use crate::cx::AppContext;
use crate::tabs::dbtools::{ToolFlavor, connection_combo, dialog_near, schema_field};
use chrono::{Datelike, Local};
use dbiji_core::config::AuthMethod;
use dbiji_core::{tr, trf};
use dbiji_tasks::{Schedule, TaskDef, TaskKind};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::path::PathBuf;

/// What the user did in the editor this frame.
pub(super) enum EditorAction {
    None,
    Cancel,
    Save(Box<TaskDef>),
}

pub(super) struct TaskEditor {
    pub draft: TaskDraft,
    /// Error of the last save attempt.
    pub error: Option<String>,
    /// A save is in progress.
    pub saving: bool,
    add_step: Option<String>,
    exe: Option<PathBuf>,
}

const CRON_PRESETS: &[&str] = &["*/15 * * * *", "0 * * * *", "0 2 * * *", "0 9 * * 1-5", "0 6 * * 1", "0 0 1 * *"];

fn preset_label(expr: &str) -> &'static str {
    match expr {
        "*/15 * * * *" => tr("Every 15 minutes"),
        "0 * * * *" => tr("Every hour"),
        "0 2 * * *" => tr("Every day at 02:00"),
        "0 9 * * 1-5" => tr("Weekdays at 09:00"),
        "0 6 * * 1" => tr("Every Monday at 06:00"),
        _ => tr("First day of the month at 00:00"),
    }
}

/// Warning when a connection cannot be opened without the user.
pub(super) fn credential_warning(cx: &AppContext, conn: &str) -> Option<&'static str> {
    let cfg = cx.conns.config(conn)?;
    match cfg.auth {
        AuthMethod::Prompt => Some(tr("This connection asks for its password at every connect; tasks can only use saved passwords.")),
        AuthMethod::Password if !cfg.save_password && !cfg.kind.is_file_based() => Some(tr("This connection does not save its password; the task cannot connect without it.")),
        _ => None,
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong());
    ui.separator();
}

fn warn(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(format!("{} {text}", icon::WARNING)).color(crate::theme::warning_color(ui.visuals().dark_mode)));
}

fn path_row(ui: &mut Ui, value: &mut String, hint: &str, save: bool) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(300.0).hint_text(hint));
        if ui.button((dbiji_icons::ic(icon::FOLDER_OPEN), tr("Browse…"))).clicked() {
            let d = dialog_near(value);
            let picked = if save { d.save_file() } else { d.pick_file() };
            if let Some(p) = picked {
                *value = p.display().to_string();
            }
        }
    });
}

/// Schedule form with validation, description and the next five runs.
pub(super) fn schedule_ui(ui: &mut Ui, s: &mut ScheduleDraft) {
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut s.mode, ScheduleMode::None, tr("Manual only"));
        ui.radio_value(&mut s.mode, ScheduleMode::Cron, tr("Cron expression"));
        ui.radio_value(&mut s.mode, ScheduleMode::Interval, tr("Interval"));
        ui.radio_value(&mut s.mode, ScheduleMode::Daily, tr("Daily"));
        ui.radio_value(&mut s.mode, ScheduleMode::Once, tr("Once"));
    });
    match s.mode {
        ScheduleMode::None => {
            ui.weak(tr("The task runs only when started manually."));
        }
        ScheduleMode::Cron => {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.cron).font(egui::TextStyle::Monospace).desired_width(220.0).hint_text("m h dom mon dow"));
                ui.menu_button(tr("Examples"), |ui| {
                    for expr in CRON_PRESETS {
                        if ui.button(format!("{}   {expr}", preset_label(expr))).clicked() {
                            s.cron = expr.to_string();
                            ui.close();
                        }
                    }
                });
            });
            ui.weak(tr("minute hour day-of-month month day-of-week, e.g. 30 9 * * 1-5 (also @hourly, @daily, @weekly, @monthly)"));
        }
        ScheduleMode::Interval => {
            ui.horizontal(|ui| {
                ui.label(tr("Every"));
                ui.add(egui::DragValue::new(&mut s.every).range(1..=100_000).speed(1));
                egui::ComboBox::from_id_salt("sched_unit").selected_text(s.unit.label()).show_ui(ui, |ui| {
                    for u in IntervalUnit::ALL {
                        ui.selectable_value(&mut s.unit, u, u.label());
                    }
                });
            });
        }
        ScheduleMode::Daily => {
            ui.horizontal(|ui| {
                ui.label(tr("At"));
                ui.add(egui::TextEdit::singleline(&mut s.daily_at).desired_width(60.0).hint_text("HH:MM"));
                ui.weak(tr("(local time)"));
            });
            ui.horizontal(|ui| {
                for (i, name) in weekday_names().iter().enumerate() {
                    ui.toggle_value(&mut s.weekdays[i], *name);
                }
                if !s.weekdays.iter().any(|d| *d) {
                    ui.weak(tr("every day"));
                }
            });
        }
        ScheduleMode::Once => {
            ui.horizontal(|ui| {
                ui.label(tr("At"));
                ui.add(egui::TextEdit::singleline(&mut s.once_at).desired_width(140.0).hint_text("YYYY-MM-DD HH:MM"));
                ui.weak(tr("(local time)"));
            });
        }
    }
    if s.mode == ScheduleMode::None {
        return;
    }
    let dark = ui.visuals().dark_mode;
    match s.preview(Local::now(), 5) {
        Err(e) => {
            ui.label(RichText::new(format!("{} {e}", icon::X_CIRCLE)).color(crate::theme::error_color(dark)));
        }
        Ok(times) => {
            if let Ok(Some(sched)) = s.to_schedule() {
                ui.label(RichText::new(sched.describe()).color(crate::theme::success_color(dark)));
            }
            if times.is_empty() {
                ui.weak(tr("No upcoming runs."));
            } else {
                ui.weak(tr("Next runs:"));
                let names = weekday_names();
                for t in times {
                    ui.monospace(format!("  {} {}", names[t.weekday().num_days_from_monday() as usize], t.format("%Y-%m-%d %H:%M:%S")));
                }
            }
        }
    }
}

impl TaskEditor {
    pub fn new(draft: TaskDraft) -> Self {
        TaskEditor { draft, error: None, saving: false, add_step: None, exe: system::current_exe().ok() }
    }

    fn kind_title(&self) -> (&'static str, &'static str) {
        match self.draft.kind.to_kind() {
            Ok(k) => (kind_icon(&k), kind_label(&k)),
            Err(_) => match &self.draft.kind {
                KindDraft::SqlScript(_) => (icon::FILE_SQL, tr("SQL script")),
                KindDraft::Shell { .. } => (icon::TERMINAL, tr("Shell command")),
                KindDraft::Backup { .. } => (icon::ARCHIVE, tr("Backup")),
                KindDraft::Restore { .. } => (icon::ARROW_COUNTER_CLOCKWISE, tr("Restore")),
                KindDraft::SchemaCompare(_) => (icon::GIT_DIFF, tr("Schema compare")),
                KindDraft::Composite { .. } => (icon::STACK, tr("Composite task")),
                KindDraft::External(k) => (kind_icon(k), kind_label(k)),
            },
        }
    }

    /// Problem with the system scheduler entry (blocks saving).
    fn os_problem(&self, task: &TaskDef) -> Option<String> {
        if !task.os_scheduler {
            return None;
        }
        let exe = self.exe.as_ref()?;
        system::entry_preview(task, exe).err().map(|e| e.message)
    }

    /// Draw the dialog.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext, tasks: &[TaskDef], folders: &[String]) -> EditorAction {
        let mut action = EditorAction::None;
        let screen = ctx.content_rect();
        let width = (screen.width() - 80.0).clamp(420.0, 760.0);
        let max_h = (screen.height() - 170.0).max(220.0);
        let (glyph, title) = self.kind_title();
        egui::Modal::new(egui::Id::new("task_editor_modal")).show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.heading(if self.draft.is_new() { tr("New task") } else { tr("Edit task") });
                ui.label(RichText::new(format!("{glyph} {title}")).weak());
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("task_editor_scroll").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                self.general_ui(ui, folders);
                section(ui, title);
                self.kind_ui(ui, cx, tasks);
                section(ui, tr("Schedule"));
                schedule_ui(ui, &mut self.draft.schedule);
                section(ui, tr("Options"));
                self.options_ui(ui);
            });
            ui.separator();
            let built = self.draft.to_task();
            let problem = match &built {
                Ok(t) => self.os_problem(t),
                Err(e) => Some(e.clone()),
            };
            if let Some(e) = &self.error {
                crate::widgets::error_box(ui, e);
            }
            ui.horizontal(|ui| {
                let can_save = problem.is_none() && !self.saving;
                if ui.add_enabled(can_save, egui::Button::new((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save")))).clicked()
                    && let Ok(t) = &built
                {
                    action = EditorAction::Save(Box::new(t.clone()));
                }
                if ui.button(tr("Cancel")).clicked() {
                    action = EditorAction::Cancel;
                }
                if self.saving {
                    ui.spinner();
                } else if let Some(p) = &problem {
                    ui.label(RichText::new(p).color(crate::theme::warning_color(ui.visuals().dark_mode)));
                }
            });
        });
        action
    }

    fn general_ui(&mut self, ui: &mut Ui, folders: &[String]) {
        let d = &mut self.draft;
        egui::Grid::new("task_general").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Name"));
            ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(320.0));
            ui.end_row();
            ui.label(tr("Folder"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut d.folder).desired_width(280.0).hint_text(tr("(top level)")));
                if !folders.is_empty() {
                    ui.menu_button(icon::CARET_DOWN, |ui| {
                        if ui.button(tr("(top level)")).clicked() {
                            d.folder.clear();
                            ui.close();
                        }
                        for f in folders {
                            if ui.button(format!("{} {f}", icon::FOLDER)).clicked() {
                                d.folder = f.clone();
                                ui.close();
                            }
                        }
                    });
                }
            });
            ui.end_row();
            ui.label(tr("Description"));
            ui.add(egui::TextEdit::multiline(&mut d.description).desired_rows(2).desired_width(320.0));
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut d.enabled, tr("Enabled (disabled tasks are not run by the scheduler)"));
            ui.end_row();
        });
    }

    fn kind_ui(&mut self, ui: &mut Ui, cx: &mut AppContext, tasks: &[TaskDef]) {
        let own_id = self.draft.original.as_ref().map(|t| t.id.clone());
        match &mut self.draft.kind {
            KindDraft::SqlScript(d) => {
                egui::Grid::new("task_sql").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label(tr("Connection"));
                    connection_combo(ui, cx, "task_sql_conn", &mut d.connection_id, 260.0);
                    ui.end_row();
                    if let Some(w) = credential_warning(cx, &d.connection_id) {
                        ui.label("");
                        warn(ui, w);
                        ui.end_row();
                    }
                    ui.label(tr("Database"));
                    ui.add(egui::TextEdit::singleline(&mut d.database).desired_width(200.0).hint_text(tr("(connection database)")));
                    ui.end_row();
                    ui.label(tr("Schema"));
                    ui.horizontal(|ui| {
                        let conn = d.connection_id.clone();
                        schema_field(ui, cx, "task_sql_schema", &conn, &mut d.schema);
                    });
                    ui.end_row();
                    ui.label(tr("Source"));
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.from_file, false, tr("SQL text"));
                        ui.radio_value(&mut d.from_file, true, tr("Script file"));
                    });
                    ui.end_row();
                });
                if d.from_file {
                    path_row(ui, &mut d.script_path, tr("Script file to run"), false);
                } else {
                    ui.add(egui::TextEdit::multiline(&mut d.sql).code_editor().desired_rows(8).desired_width(f32::INFINITY).hint_text("SELECT …;"));
                }
                ui.checkbox(&mut d.stop_on_error, tr("Stop at the first failing statement"));
                ui.checkbox(&mut d.commit_at_end, tr("Commit at the end (connections in manual commit mode)"));
                ui.weak(tr("Client commands such as @set or @include are not run by tasks."));
            }
            KindDraft::Shell { command } => {
                ui.add(egui::TextEdit::multiline(command).code_editor().desired_rows(3).desired_width(f32::INFINITY).hint_text(tr("Command line")));
                ui.weak(tr("Runs with the system shell (sh -c, cmd /C on Windows); the output goes to the run log."));
            }
            KindDraft::Backup { connection_id, form } => {
                ui.horizontal(|ui| {
                    ui.label(tr("Connection"));
                    connection_combo(ui, cx, "task_backup_conn", connection_id, 260.0);
                });
                if let Some(w) = credential_warning(cx, connection_id) {
                    warn(ui, w);
                }
                let flavor = cx.conns.config(connection_id).map(|c| ToolFlavor::of(c.kind)).unwrap_or(ToolFlavor::Other);
                form.ui(ui, flavor, "task_backup");
            }
            KindDraft::Restore { connection_id, form } => {
                ui.horizontal(|ui| {
                    ui.label(tr("Connection"));
                    connection_combo(ui, cx, "task_restore_conn", connection_id, 260.0);
                });
                if let Some(w) = credential_warning(cx, connection_id) {
                    warn(ui, w);
                }
                let flavor = cx.conns.config(connection_id).map(|c| ToolFlavor::of(c.kind)).unwrap_or(ToolFlavor::Other);
                form.ui(ui, flavor, "task_restore");
            }
            KindDraft::SchemaCompare(d) => {
                egui::Grid::new("task_compare").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label(tr("Source"));
                    ui.horizontal(|ui| {
                        connection_combo(ui, cx, "task_cmp_src", &mut d.source_connection, 200.0);
                        let conn = d.source_connection.clone();
                        schema_field(ui, cx, "task_cmp_src_schema", &conn, &mut d.source_schema);
                    });
                    ui.end_row();
                    ui.label(tr("Target"));
                    ui.horizontal(|ui| {
                        connection_combo(ui, cx, "task_cmp_dst", &mut d.target_connection, 200.0);
                        let conn = d.target_connection.clone();
                        schema_field(ui, cx, "task_cmp_dst_schema", &conn, &mut d.target_schema);
                    });
                    ui.end_row();
                    ui.label(tr("Migration script"));
                    path_row(ui, &mut d.script_path, tr("optional: write the script to this file"), true);
                    ui.end_row();
                    ui.label(tr("HTML report"));
                    path_row(ui, &mut d.report_path, tr("optional: write a report to this file"), true);
                    ui.end_row();
                });
                for conn in [d.source_connection.clone(), d.target_connection.clone()] {
                    if let Some(w) = credential_warning(cx, &conn) {
                        warn(ui, w);
                        break;
                    }
                }
                ui.checkbox(&mut d.drop_removed, tr("Drop objects missing in the source"));
                ui.checkbox(&mut d.apply, tr("Apply the migration script to the target"));
                if d.apply {
                    warn(ui, tr("Every run changes the target schema without asking."));
                }
            }
            KindDraft::Composite { steps, stop_on_error, parallel } => {
                let name_of = |id: &str| tasks.iter().find(|t| t.id == id).map(|t| (kind_icon(&t.kind), t.path()));
                let mut action: Option<(usize, i32)> = None;
                if steps.is_empty() {
                    ui.weak(tr("Add the tasks to run."));
                }
                for (i, id) in steps.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.weak(format!("{}.", i + 1));
                        match name_of(id) {
                            Some((g, n)) => ui.label(format!("{g} {n}")),
                            None => ui.label(RichText::new(tr("(missing task)")).color(crate::theme::error_color(ui.visuals().dark_mode))),
                        };
                        if ui.add_enabled(i > 0, egui::Button::new(dbiji_icons::ic(icon::ARROW_UP)).small()).on_hover_text(tr("Move up")).clicked() {
                            action = Some((i, -1));
                        }
                        if ui.add_enabled(i + 1 < steps.len(), egui::Button::new(dbiji_icons::ic(icon::ARROW_DOWN)).small()).on_hover_text(tr("Move down")).clicked() {
                            action = Some((i, 1));
                        }
                        if ui.small_button(dbiji_icons::ic(icon::X)).on_hover_text(tr("Remove")).clicked() {
                            action = Some((i, 0));
                        }
                    });
                }
                match action {
                    Some((i, 0)) => {
                        steps.remove(i);
                    }
                    Some((i, d)) => {
                        let j = (i as i32 + d) as usize;
                        steps.swap(i, j);
                    }
                    None => {}
                }
                ui.horizontal(|ui| {
                    let candidates: Vec<&TaskDef> = tasks.iter().filter(|t| Some(&t.id) != own_id.as_ref()).collect();
                    let label = self.add_step.as_ref().and_then(|id| candidates.iter().find(|t| &t.id == id)).map(|t| t.path()).unwrap_or_else(|| tr("Choose a task…").to_string());
                    egui::ComboBox::from_id_salt("task_add_step").selected_text(label).width(280.0).show_ui(ui, |ui| {
                        for t in &candidates {
                            ui.selectable_value(&mut self.add_step, Some(t.id.clone()), format!("{} {}", kind_icon(&t.kind), t.path()));
                        }
                    });
                    if ui.add_enabled(self.add_step.is_some(), egui::Button::new((dbiji_icons::ic(icon::PLUS), tr("Add step")))).clicked()
                        && let Some(id) = self.add_step.take()
                    {
                        steps.push(id);
                    }
                });
                ui.checkbox(parallel, tr("Run the steps in parallel"));
                ui.checkbox(stop_on_error, if *parallel { tr("Cancel the other steps when a step fails") } else { tr("Stop at the first failing step") });
            }
            KindDraft::External(kind) => {
                ui.label(tr("This task was created by a transfer wizard; its settings are shown read-only. Create it again from the wizard to change them."));
                let conns: Vec<String> = kind.connection_ids().iter().map(|c| cx.conns.config(c).map(|c| c.name.clone()).unwrap_or_else(|| c.to_string())).collect();
                if !conns.is_empty() {
                    ui.label(trf("Connections: {}", &[&conns.join(", ")]));
                }
                let json = match kind {
                    TaskKind::Export { config, .. } | TaskKind::Import { config, .. } | TaskKind::Migration { config, .. } | TaskKind::SchemaCompare { config } => {
                        serde_json::to_string_pretty(config).unwrap_or_default()
                    }
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                egui::ScrollArea::vertical().id_salt("task_external_json").max_height(220.0).show(ui, |ui| {
                    let mut t = json.as_str();
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
            }
        }
    }

    fn options_ui(&mut self, ui: &mut Ui) {
        let d = &mut self.draft;
        ui.horizontal(|ui| {
            ui.checkbox(&mut d.timeout, tr("Time limit"));
            ui.add_enabled(d.timeout, egui::DragValue::new(&mut d.timeout_secs).range(1..=7 * 86_400).speed(10).suffix(" s"));
            if d.timeout {
                ui.weak(crate::widgets::format_duration(std::time::Duration::from_secs(d.timeout_secs)));
            }
        });
        ui.horizontal(|ui| {
            ui.label(tr("Notify"));
            ui.checkbox(&mut d.notify_success, tr("when a run succeeds"));
            ui.checkbox(&mut d.notify_failure, tr("when a run fails"));
        });
        let schedule = d.schedule.to_schedule().ok().flatten();
        let r = ui.add_enabled_ui(schedule.is_some(), |ui| ui.checkbox(&mut d.os_scheduler, tr("Also run when DBiji is closed (system scheduler)")));
        r.response.on_disabled_hover_text(tr("Needs a schedule"));
        if !(d.os_scheduler && schedule.is_some()) {
            return;
        }
        ui.indent("task_os", |ui| {
            ui.weak(trf(
                "The schedule is registered with {} when you save. It starts DBiji without a window (--run-task) and uses saved passwords only.",
                &[&system::scheduler_name()],
            ));
            if matches!(schedule, Some(Schedule::Once { .. })) {
                warn(ui, tr("The system scheduler repeats a one-time schedule every year on that date; remove the option after the run."));
            }
            let task = self.draft.to_task();
            match (&task, &self.exe) {
                (Ok(t), Some(exe)) => match system::entry_preview(t, exe) {
                    Ok(text) => {
                        egui::CollapsingHeader::new(tr("System scheduler entry")).id_salt("task_os_entry").show(ui, |ui| {
                            egui::ScrollArea::vertical().id_salt("task_os_entry_scroll").max_height(180.0).show(ui, |ui| {
                                let mut s = text.as_str();
                                ui.add(egui::TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                            });
                        });
                    }
                    Err(e) => {
                        ui.label(RichText::new(e.message).color(crate::theme::error_color(ui.visuals().dark_mode)));
                    }
                },
                (_, None) => warn(ui, tr("The DBiji program could not be located.")),
                _ => {}
            }
        });
    }
}
