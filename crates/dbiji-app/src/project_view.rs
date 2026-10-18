//! Project panel next to the database navigator: scripts (with folders),
//! bookmarks and saved ER diagrams of the default project.

use crate::command::{AppCommand, ObjectPage};
use crate::cx::AppContext;
use dbiji_config::project::ScriptFolder;
use dbiji_config::Bookmark;
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
        ui.horizontal(|ui| {
            if ui.button(icon::FILE_PLUS).on_hover_text(tr("New SQL editor")).clicked() {
                cx.push(AppCommand::NewSqlEditor { connection: None, schema: None, text: None });
            }
            if ui.button(icon::ARROW_CLOCKWISE).on_hover_text(tr("Refresh")).clicked() {
                self.reload(cx);
            }
            if let Some(p) = cx.services.projects.as_ref().and_then(|m| m.default_project().ok()) {
                if ui.button(icon::FOLDER_OPEN).on_hover_text(tr("Show in Finder")).clicked() {
                    let _ = open::that(p.path());
                }
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
                    if ui.selectable_label(false, format!("{} {d}", icon::GRAPH)).double_clicked() {
                        if let Some(dir) = &dir {
                            cx.push(AppCommand::OpenSavedDiagram(dir.join(format!("{d}.json"))));
                        }
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
        let r = ui.selectable_label(false, format!("{} {}", icon::FILE_SQL, s.name)).on_hover_text(s.path.display().to_string());
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
