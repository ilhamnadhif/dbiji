//! Tip of the day: short hints about features, shown on start (can be
//! turned off) and from Help › Tip of the day.

use dbiji_core::tr;
use egui::{Context, RichText};
use egui_phosphor::regular as icon;

pub const TIPS: &[&str] = &[
    "Press Ctrl+Enter (⌘↩ on macOS) to run the statement under the cursor; select text to run only the selection.",
    "The command palette (⌘⇧P / Ctrl+Shift+P) runs any command and opens tables, views and routines by name.",
    "Open Database Object (⌘⇧D / Ctrl+Shift+D) finds a table or routine by name in every connected database.",
    "Use :name or ${name} in a script: DBiji asks for the values when it runs.",
    "Double-click a cell in the results to edit it; changes are saved with ⌘S / Ctrl+S or the Save button.",
    "Right-click a column header to filter, hide, pin or highlight rows with a value.",
    "Click a column header to sort: ascending, descending, then back to the original order.",
    "Connection types (development, test, production) colour the connection and can require confirmation before data changes.",
    "In manual commit mode, Database › Pending transactions lists every connection with uncommitted changes.",
    "Navigator › Link with editor selects the table of the active tab in the tree.",
    "Fold long statements and block comments with the markers in the gutter; the SQL editor menu collapses or expands them all.",
    "Drag a .sql file onto the window to open it; drop a SQLite or DuckDB file to add it as a connection.",
    "Tools › Backup and Restore use pg_dump, mysqldump or sqlite3; Scope › All databases runs pg_dumpall.",
    "Export results to CSV, JSON, Excel, SQL INSERT statements, Markdown and more from the results toolbar.",
    "The ER diagram of a schema opens from the navigator context menu; drag tables to arrange it.",
    "Compare schemas or the data of two tables from the Database menu and generate the synchronisation script.",
    "Settings › Keyboard lets you choose DBeaver-compatible shortcuts or define your own.",
    "View › Theme switches between the light and the dark theme, or follows the system.",
    "Help › Error log shows the errors and warnings of this session; Help › Collect diagnostic info saves them without any secret.",
    "Hold ⌘ / Ctrl and click a table name in the editor to open it.",
];

/// The tip window; `index` is the tip shown. Returns false when it is closed.
pub struct TipWindow {
    pub index: usize,
}

impl TipWindow {
    /// Show the tip; `show_on_start` is the setting. Returns false when closed.
    pub fn show(&mut self, ctx: &Context, show_on_start: &mut bool) -> bool {
        let mut open = true;
        let mut close = false;
        let n = TIPS.len();
        egui::Window::new(format!("{} {}", icon::LIGHTBULB, tr("Tip of the day")))
            .id(egui::Id::new("tip_of_the_day"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.add(egui::Label::new(RichText::new(tr(TIPS[self.index % n])).size(14.0)).wrap());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.checkbox(show_on_start, tr("Show tips on start"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tr("Close")).clicked() {
                            close = true;
                        }
                        if ui.button(format!("{} {}", tr("Next tip"), icon::CARET_RIGHT)).clicked() {
                            self.index = (self.index + 1) % n;
                        }
                        if ui.button(icon::CARET_LEFT).on_hover_text(tr("Previous tip")).clicked() {
                            self.index = (self.index + n - 1) % n;
                        }
                        ui.weak(format!("{}/{n}", self.index % n + 1));
                    });
                });
            });
        open && !close
    }
}
