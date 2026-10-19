//! Error log view: the application log (errors and warnings of every
//! component, connection failures, info messages) with a level filter,
//! search, copy and the log file location.

use crate::applog::{self, LogEntry};
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::tr;
use egui::{Color32, RichText, Ui};
use egui_phosphor::regular as icon;
use tracing::Level;

pub struct ErrorLogTab {
    /// Least severe level shown.
    level: Level,
    search: String,
    entries: Vec<LogEntry>,
    version: Option<u64>,
}

impl Default for ErrorLogTab {
    fn default() -> Self {
        ErrorLogTab { level: Level::WARN, search: String::new(), entries: Vec::new(), version: None }
    }
}

impl ErrorLogTab {
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries shown with the current filter, newest first.
    pub fn visible(&self) -> Vec<&LogEntry> {
        let needle = self.search.to_lowercase();
        self.entries.iter().rev().filter(|e| e.level <= self.level && (needle.is_empty() || e.message.to_lowercase().contains(&needle) || e.target.contains(&needle))).collect()
    }

    pub fn set_level(&mut self, level: Level) {
        self.level = level;
    }
}

fn level_color(level: Level, dark: bool) -> Color32 {
    match level {
        Level::ERROR => crate::theme::error_color(dark),
        Level::WARN => Color32::from_rgb(214, 152, 32),
        _ => Color32::GRAY,
    }
}

fn level_label(level: Level) -> &'static str {
    match level {
        Level::ERROR => "Errors",
        Level::WARN => "Errors and warnings",
        _ => "Everything",
    }
}

impl TabView for ErrorLogTab {
    fn title(&self) -> String {
        tr("Error log").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::WARNING_OCTAGON
    }

    fn dedupe_key(&self) -> Option<String> {
        Some("error-log".into())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let _ = cx;
        let v = applog::version();
        if self.version != Some(v) {
            self.entries = applog::entries();
            self.version = Some(v);
        }
        // new entries appear while the tab is open
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("error_log_level").selected_text(tr(level_label(self.level))).show_ui(ui, |ui| {
                for l in [Level::ERROR, Level::WARN, Level::INFO] {
                    ui.selectable_value(&mut self.level, l, tr(level_label(l)));
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Search"))).desired_width(220.0));
            if ui.button(format!("{} {}", icon::COPY, tr("Copy"))).on_hover_text(tr("Copy the shown entries")).clicked() {
                let text: Vec<String> = self.visible().iter().rev().map(|e| e.line()).collect();
                ui.ctx().copy_text(text.join("\n"));
            }
            if ui.button(format!("{} {}", icon::TRASH, tr("Clear"))).clicked() {
                applog::clear();
            }
            if let Some(file) = applog::log_file()
                && ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Show log file"))).on_hover_text(file.display().to_string()).clicked()
            {
                let _ = open::that_detached(file.parent().unwrap_or(&file));
            }
        });
        ui.separator();
        let dark = ui.visuals().dark_mode;
        let shown = self.visible();
        if shown.is_empty() {
            ui.weak(tr("No entries."));
            return;
        }
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, shown.len(), |ui, range| {
            for e in &shown[range] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(e.time.format("%H:%M:%S").to_string()).monospace().weak()).on_hover_text(e.time.format("%Y-%m-%d %H:%M:%S%.3f").to_string());
                    ui.label(RichText::new(format!("{:<5}", e.level)).monospace().color(level_color(e.level, dark)));
                    let first = e.message.lines().next().unwrap_or("");
                    let resp = ui.add(egui::Label::new(first).truncate());
                    if e.message.contains('\n') || first.len() > 120 {
                        resp.on_hover_text(format!("{}\n\n{}", e.target, e.message));
                    }
                });
            }
        });
    }

    crate::impl_tab_any!();
}
