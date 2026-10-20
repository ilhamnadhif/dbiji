//! Project panel next to the database navigator: scripts (with folders),
//! bookmarks and saved ER diagrams of the default project.

use crate::command::{AppCommand, ObjectPage};
use crate::cx::AppContext;
use dbiji_config::Bookmark;
use dbiji_config::project::ScriptFolder;
use dbiji_core::tr;
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ProjectView {
    scripts: Option<ScriptFolder>,
    bookmarks: Vec<Bookmark>,
    diagrams: Vec<String>,
    loaded: Option<Instant>,
    filter: String,
    error: Option<String>,
    /// Name typed for a new project.
    new_project: Option<String>,
}

impl ProjectView {
    fn reload(&mut self, cx: &AppContext) {
        self.loaded = Some(Instant::now());
        let Some(p) = cx.services.projects.as_ref().and_then(|m| m.default_project().ok()) else {
            self.error = Some(tr("The project folder is not available.").to_string());
            return;
        };
        self.error = None;
        self.scripts = p.script_tree().ok();
        self.bookmarks = p.bookmarks().unwrap_or_default();
        self.diagrams = p.list_diagrams().unwrap_or_default();
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.loaded.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.reload(cx);
        }
        if let Some(pm) = cx.services.projects.clone() {
            let active = pm.active_name();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("project_switch").selected_text(format!("{} {}", icon::FOLDER_SIMPLE, active)).width(ui.available_width() - 4.0).show_ui(ui, |ui| {
                    for p in pm.list().unwrap_or_default() {
                        if ui.selectable_label(p.name() == active, p.name()).clicked() && p.name() != active {
                            cx.push(AppCommand::SetActiveProject(p.name().to_string()));
                            self.loaded = None;
                        }
                    }
                    ui.separator();
                    if ui.button((dbiji_icons::ic(icon::PLUS), tr("New project…"))).clicked() {
                        self.new_project = Some(String::new());
                    }
                });
            });
            if let Some(name) = &mut self.new_project {
                let mut done = false;
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(name).hint_text(tr("Project name")).desired_width(ui.available_width() - 60.0));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button(tr("Create")).clicked() || enter) && !name.trim().is_empty() {
                        match pm.create(name.trim()) {
                            Ok(p) => {
                                cx.push(AppCommand::SetActiveProject(p.name().to_string()));
                                self.loaded = None;
                                done = true;
                            }
                            Err(e) => cx.toasts.error(tr("Cannot create the project"), Some(e.to_string())),
                        }
                    }
                    if ui.button(dbiji_icons::ic(icon::X)).clicked() {
                        done = true;
                    }
                });
                if done {
                    self.new_project = None;
                }
            }
        }
        ui.horizontal(|ui| {
            if ui.button(dbiji_icons::ic(icon::FILE_PLUS)).on_hover_text(tr("New SQL editor")).clicked() {
                cx.push(AppCommand::NewSqlEditor { connection: None, schema: None, text: None });
            }
            if ui.button(dbiji_icons::ic(icon::ARROW_CLOCKWISE)).on_hover_text(tr("Refresh")).clicked() {
                self.reload(cx);
            }
            if let Some(p) = cx.services.projects.as_ref().and_then(|m| m.default_project().ok())
                && ui.button(dbiji_icons::ic(icon::FOLDER_OPEN)).on_hover_text(tr("Show in Finder")).clicked()
            {
                let _ = open::that(p.path());
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Filter"))).desired_width(f32::INFINITY));
        if let Some(e) = &self.error {
            ui.weak(e);
        }
        let filter = self.filter.to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::CollapsingHeader::new(RichText::new(format!("{} {}", icon::FILE_SQL, tr("Scripts"))).strong()).default_open(true).show(ui, |ui| {
                if let Some(root) = self.scripts.clone() {
                    folder(ui, cx, &root, &filter);
                }
            });
            egui::CollapsingHeader::new(RichText::new(format!("{} {}", icon::BOOKMARK_SIMPLE, tr("Bookmarks"))).strong()).default_open(true).show(ui, |ui| {
                if self.bookmarks.is_empty() {
                    ui.weak(tr("Right-click an object in the navigator to bookmark it."));
                }
                let mut remove: Option<String> = None;
                for b in &self.bookmarks {
                    if !filter.is_empty() && !b.label.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let conn_name = cx.conns.config(&b.connection_id).map(|c| c.name.clone()).unwrap_or_default();
                    let r = ui.selectable_label(false, format!("{} {}", crate::nav::kind_icon(b.object.kind), b.label)).on_hover_text(&conn_name);
                    if r.double_clicked() {
                        if !cx.conns.is_connected(&b.connection_id) {
                            cx.push(AppCommand::Connect(b.connection_id.clone()));
                        }
                        let page = if b.object.kind.has_data() { ObjectPage::Data } else { ObjectPage::Properties };
                        cx.push(AppCommand::OpenObject { connection: b.connection_id.clone(), object: b.object.clone(), page });
                    }
                    r.context_menu(|ui| {
                        if ui.button(tr("Remove bookmark")).clicked() {
                            remove = Some(b.id.clone());
                            ui.close();
                        }
                    });
                }
                if let Some(id) = remove {
                    if let Some(p) = cx.services.projects.as_ref().and_then(|m| m.default_project().ok()) {
                        let _ = p.remove_bookmark(&id);
                    }
                    self.loaded = None;
                }
            });
            egui::CollapsingHeader::new(RichText::new(format!("{} {}", icon::GRAPH, tr("Diagrams"))).strong()).default_open(true).show(ui, |ui| {
                if self.diagrams.is_empty() {
                    ui.weak(tr("Saved ER diagrams appear here."));
                }
                let dir = cx.services.projects.as_ref().and_then(|m| m.default_project().ok()).map(|p| p.diagrams_dir());
                for d in &self.diagrams {
                    if !filter.is_empty() && !d.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if ui.selectable_label(false, format!("{} {d}", icon::GRAPH)).double_clicked()
                        && let Some(dir) = &dir
                    {
                        cx.push(AppCommand::OpenSavedDiagram(dir.join(format!("{d}.json"))));
                    }
                }
            });
        });
    }
}

fn folder(ui: &mut Ui, cx: &mut AppContext, f: &ScriptFolder, filter: &str) {
    for sub in &f.folders {
        egui::CollapsingHeader::new(format!("{} {}", icon::FOLDER, sub.name)).id_salt(&sub.relative).show(ui, |ui| folder(ui, cx, sub, filter));
    }
    for s in &f.scripts {
        if !filter.is_empty() && !s.name.to_lowercase().contains(filter) {
            continue;
        }
        let r = ui.selectable_label(false, (dbiji_icons::ic(icon::FILE_SQL), s.name.as_str())).on_hover_text(s.path.display().to_string());
        if r.double_clicked() {
            cx.push(AppCommand::OpenScript(s.path.clone()));
        }
        r.context_menu(|ui| {
            if ui.button(tr("Open")).clicked() {
                cx.push(AppCommand::OpenScript(s.path.clone()));
                ui.close();
            }
            if ui.button(tr("Show in Finder")).clicked() {
                let _ = open::that(s.path.parent().unwrap_or(&s.path));
                ui.close();
            }
        });
    }
}
