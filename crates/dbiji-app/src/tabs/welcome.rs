//! Start page shown when no other tab is open.

use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::tr;
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

#[derive(Default)]
pub struct WelcomeTab;

/// Most recently modified scripts of the default project (cached for a few seconds).
fn recent_scripts(ui: &Ui, cx: &AppContext) -> Vec<dbiji_config::project::ScriptInfo> {
    let id = egui::Id::new("welcome_recent_scripts");
    let now = ui.input(|i| i.time);
    if let Some((t, list)) = ui.data(|d| d.get_temp::<(f64, std::sync::Arc<Vec<dbiji_config::project::ScriptInfo>>)>(id)) {
        if now - t < 5.0 {
            return (*list).clone();
        }
    }
    let mut list = cx.services.projects.as_ref().and_then(|p| p.default_project().ok()).and_then(|p| p.list_scripts().ok()).unwrap_or_default();
    list.sort_by_key(|s| std::cmp::Reverse(s.modified));
    list.truncate(8);
    ui.data_mut(|d| d.insert_temp(id, (now, std::sync::Arc::new(list.clone()))));
    list
}

impl TabView for WelcomeTab {
    fn title(&self) -> String {
        tr("Welcome").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::HOUSE
    }

    fn dedupe_key(&self) -> Option<String> {
        Some("welcome".into())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("DBiji").size(32.0).strong());
                ui.label(RichText::new(tr("Lightweight SQL client written in Rust")).weak());
                ui.add_space(16.0);
                let w = 260.0;
                if ui.add_sized([w, 28.0], egui::Button::new(format!("{} {}", icon::PLUS, tr("New connection")))).clicked() {
                    cx.push(AppCommand::NewConnection { kind: None, folder: None });
                }
                if ui.add_sized([w, 28.0], egui::Button::new(format!("{} {}", icon::FILE_SQL, tr("New SQL editor")))).clicked() {
                    cx.push(AppCommand::NewSqlEditor { connection: None, schema: None, text: None });
                }
                if ui.add_sized([w, 28.0], egui::Button::new(format!("{} {}", icon::DOWNLOAD_SIMPLE, tr("Import connections from DBeaver")))).clicked() {
                    cx.push(AppCommand::ImportConnections);
                }
                if ui.add_sized([w, 28.0], egui::Button::new(format!("{} {}", icon::DATABASE, tr("Create sample database")))).on_hover_text(tr("A small SQLite shop database to try every feature")).clicked() {
                    cx.push(AppCommand::CreateSampleDatabase);
                }
                if ui.add_sized([w, 28.0], egui::Button::new(format!("{} {}", icon::COMMAND, tr("Command palette")))).clicked() {
                    cx.push(AppCommand::ShowPalette);
                }
                ui.add_space(20.0);
                ui.label(RichText::new(tr("Recent connections")).strong());
                let mut recent: Vec<_> = cx.conns.configs().iter().filter(|c| c.last_connected.is_some()).cloned().collect();
                recent.sort_by_key(|c| std::cmp::Reverse(c.last_connected));
                if recent.is_empty() {
                    ui.weak(tr("None yet"));
                }
                for c in recent.into_iter().take(8) {
                    if ui.link(format!("{} {}  —  {}", crate::nav::db_icon(c.kind), c.name, c.summary())).clicked() {
                        cx.push(AppCommand::Connect(c.id.clone()));
                        cx.push(AppCommand::NewSqlEditor { connection: Some(c.id.clone()), schema: None, text: None });
                    }
                }
                let scripts = recent_scripts(ui, cx);
                if !scripts.is_empty() {
                    ui.add_space(16.0);
                    ui.label(RichText::new(tr("Recent scripts")).strong());
                    for s in scripts {
                        if ui.link(format!("{} {}", icon::FILE_SQL, s.relative)).on_hover_text(s.path.display().to_string()).clicked() {
                            cx.push(AppCommand::OpenScript(s.path.clone()));
                        }
                    }
                }
            });
        });
    }

    crate::impl_tab_any!();
}
