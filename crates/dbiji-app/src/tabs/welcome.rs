//! Start page shown when no other tab is open.

use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::tr;
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

#[derive(Default)]
pub struct WelcomeTab;

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
            });
        });
    }

    crate::impl_tab_any!();
}
