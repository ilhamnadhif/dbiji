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
    if let Some((t, list)) = ui.data(|d| d.get_temp::<(f64, std::sync::Arc<Vec<dbiji_config::project::ScriptInfo>>)>(id))
        && now - t < 5.0
    {
        return (*list).clone();
    }
    let mut list = cx.services.projects.as_ref().and_then(|p| p.default_project().ok()).and_then(|p| p.list_scripts().ok()).unwrap_or_default();
    list.sort_by_key(|s| std::cmp::Reverse(s.modified));
    list.truncate(8);
    ui.data_mut(|d| d.insert_temp(id, (now, std::sync::Arc::new(list.clone()))));
    list
}

/// A clickable two-line row: icon, name, and a detail line (cut with "…").
fn recent_row(ui: &mut Ui, icon: &str, name: &str, detail: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 42.0), egui::Sense::click());
    let v = ui.visuals();
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, v.widgets.hovered.weak_bg_fill);
    }
    let text_color = v.text_color();
    let weak = v.weak_text_color();
    let painter = ui.painter_at(rect);
    painter.text(egui::pos2(rect.left() + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, icon, egui::TextStyle::Body.resolve(ui.style()), weak);
    let left = rect.left() + 34.0;
    let max_w = (rect.right() - left - 8.0).max(20.0);
    let line = |text: &str, style: egui::TextStyle, color: egui::Color32| {
        egui::WidgetText::from(RichText::new(text).color(color)).into_galley(ui, Some(egui::TextWrapMode::Truncate), max_w, style)
    };
    let top = line(name, egui::TextStyle::Body, text_color);
    let bottom = line(detail, egui::TextStyle::Small, weak);
    let y = rect.center().y - (top.size().y + bottom.size().y + 2.0) / 2.0;
    let top_h = top.size().y;
    painter.galley(egui::pos2(left, y), top, text_color);
    painter.galley(egui::pos2(left, y + top_h + 2.0), bottom, weak);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
    resp
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
        use crate::keys::Action;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(32.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("DBiji").size(34.0).strong());
                ui.label(RichText::new(tr("Lightweight SQL client written in Rust")).weak());
                ui.label(RichText::new(format!("{} {}", tr("Version"), env!("CARGO_PKG_VERSION"))).small().weak());
            });
            ui.add_space(24.0);
            // Two cards side by side when there is room, else stacked.
            let card_w = 340.0;
            let wide = ui.available_width() >= card_w * 2.0 + 48.0;
            let total = if wide { card_w * 2.0 + 24.0 } else { card_w };
            let margin = ((ui.available_width() - total) / 2.0).max(8.0);
            let card = |ui: &mut Ui, title: &str, add: &mut dyn FnMut(&mut Ui)| {
                egui::Frame::group(ui.style()).fill(ui.visuals().window_fill).corner_radius(8).inner_margin(16).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(card_w - 34.0);
                        ui.label(RichText::new(title).strong().size(15.0));
                        ui.add_space(8.0);
                        add(ui);
                    });
                });
            };
            let keymap = cx.keymap;
            let shortcut = |a: Action| crate::keys::label(keymap, a);
            // The cards only read the context; their commands run afterwards.
            let cmds = std::cell::RefCell::new(Vec::new());
            let push = |c: AppCommand| cmds.borrow_mut().push(c);
            let ro: &AppContext = cx;
            let mut start = |ui: &mut Ui| {
                ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
                let full = egui::vec2(ui.available_width(), 32.0);
                let accent = ui.visuals().selection.stroke.color;
                let on_accent = if ui.visuals().dark_mode { egui::Color32::BLACK } else { egui::Color32::WHITE };
                let primary = egui::Button::new(RichText::new(format!("{}  {}", icon::PLUS, tr("New connection"))).color(on_accent).strong()).fill(accent).min_size(full);
                if ui.add(primary).clicked() {
                    push(AppCommand::NewConnection { kind: None, folder: None });
                }
                ui.add_space(4.0);
                let items: [(&str, &str, Option<Action>, Option<&str>, AppCommand); 4] = [
                    (icon::FILE_SQL, tr("New SQL editor"), Some(Action::NewSqlEditor), None, AppCommand::NewSqlEditor { connection: None, schema: None, text: None }),
                    (icon::COMMAND, tr("Command palette"), Some(Action::CommandPalette), None, AppCommand::ShowPalette),
                    (icon::DOWNLOAD_SIMPLE, tr("Import connections from DBeaver"), None, None, AppCommand::ImportConnections),
                    (icon::DATABASE, tr("Create sample database"), None, Some(tr("A small SQLite shop database to try every feature")), AppCommand::CreateSampleDatabase),
                ];
                for (ic, label, action, tip, cmd) in items {
                    let mut b = egui::Button::new(format!("{ic}  {label}")).min_size(full);
                    if let Some(a) = action {
                        let keys = shortcut(a);
                        if !keys.is_empty() {
                            b = b.shortcut_text(keys);
                        }
                    }
                    let mut r = ui.add(b);
                    if let Some(t) = tip {
                        r = r.on_hover_text(t);
                    }
                    if r.clicked() {
                        push(cmd);
                    }
                }
            };
            let mut recent_ui = |ui: &mut Ui| {
                let mut recent: Vec<_> = ro.conns.configs().iter().filter(|c| c.last_connected.is_some()).cloned().collect();
                recent.sort_by_key(|c| std::cmp::Reverse(c.last_connected));
                ui.label(RichText::new(tr("Recent connections")).weak().small());
                if recent.is_empty() {
                    ui.weak(tr("None yet"));
                }
                for c in recent.into_iter().take(6) {
                    let detail = crate::nav::connection_detail(&c);
                    if recent_row(ui, crate::nav::db_icon(c.kind), &c.name, &detail).on_hover_text(format!("{detail}\n{}", tr("Connect and open a SQL editor"))).clicked() {
                        push(AppCommand::Connect(c.id.clone()));
                        push(AppCommand::NewSqlEditor { connection: Some(c.id.clone()), schema: None, text: None });
                    }
                }
                let scripts = recent_scripts(ui, ro);
                if !scripts.is_empty() {
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr("Recent scripts")).weak().small());
                    for s in scripts.into_iter().take(6) {
                        let dir = s.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                        let r = recent_row(ui, icon::FILE_SQL, &s.relative, &dir);
                        if r.on_hover_text(s.path.display().to_string()).clicked() {
                            push(AppCommand::OpenScript(s.path.clone()));
                        }
                    }
                }
            };
            ui.horizontal_top(|ui| {
                ui.add_space(margin);
                if wide {
                    card(ui, tr("Start"), &mut start);
                    ui.add_space(24.0);
                    card(ui, tr("Recent"), &mut recent_ui);
                } else {
                    ui.vertical(|ui| {
                        card(ui, tr("Start"), &mut start);
                        ui.add_space(16.0);
                        card(ui, tr("Recent"), &mut recent_ui);
                    });
                }
            });
            for c in cmds.take() {
                cx.push(c);
            }
        });
    }

    crate::impl_tab_any!();
}
