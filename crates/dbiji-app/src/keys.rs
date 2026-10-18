//! Keyboard shortcuts overview.

use crate::cx::AppContext;
use dbiji_core::tr;

/// Default shortcuts (PRD §10). Customisable keymaps are configured in settings.
pub const DEFAULT_SHORTCUTS: &[(&str, &str)] = &[
    ("Execute statement at cursor", "⌘↩"),
    ("Execute script", "⌥X"),
    ("Execute in new result tab", "⇧⌘↩"),
    ("Explain execution plan", "⇧⌘E"),
    ("Cancel running query", "⌘."),
    ("New SQL editor for the selected connection", "⌘]"),
    ("Select active connection of the editor", "⌘9"),
    ("Select active schema of the editor", "⌘0"),
    ("Show or hide the results panel", "⌘6"),
    ("Format SQL", "⇧⌘F"),
    ("Command palette", "⇧⌘P"),
    ("Save data changes / save script", "⌘S"),
    ("Autocomplete", "⌃Space"),
    ("Toggle comment", "⌘/"),
    ("Find / replace", "⌘F / ⌥⌘F"),
    ("Go to line", "⌘L"),
    ("Toggle navigator", "⇧⌘B"),
];

pub fn shortcuts_table(ui: &mut egui::Ui, _cx: &mut AppContext) {
    egui::Grid::new("shortcuts").num_columns(2).striped(true).spacing([24.0, 6.0]).show(ui, |ui| {
        for (action, keys) in DEFAULT_SHORTCUTS {
            ui.label(tr(action));
            ui.monospace(*keys);
            ui.end_row();
        }
    });
}
