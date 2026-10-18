//! Keyboard shortcuts: keymap lookup for key events and the shortcuts
//! overview (PRD §10). Bindings are customisable in the settings and stored
//! in `keymap.json`.

use crate::cx::AppContext;
pub use dbiji_config::{Action, ActionCategory, ActionContext, Keymap, Shortcut};
use dbiji_core::tr;

pub const IS_MAC: bool = cfg!(target_os = "macos");

/// Shortcut produced by a key press.
pub fn shortcut_of(key: egui::Key, m: egui::Modifiers) -> Option<Shortcut> {
    Shortcut::from_event(key.name(), m.command, m.ctrl, m.alt, m.shift, IS_MAC)
}

/// Actions triggered by this frame's key presses in `context`; the matching
/// key events are consumed so widgets do not also react to them.
pub fn take_actions(ctx: &egui::Context, keymap: &Keymap, context: ActionContext) -> Vec<Action> {
    let mut hits: Vec<(Action, egui::Key, egui::Modifiers)> = Vec::new();
    ctx.input(|i| {
        for ev in &i.events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                let Some(sc) = shortcut_of(*key, *modifiers) else { continue };
                if let Some(a) = keymap.action_for(&sc, context) {
                    hits.push((a, *key, *modifiers));
                }
            }
        }
    });
    if hits.is_empty() {
        return Vec::new();
    }
    ctx.input_mut(|i| {
        for (_, key, m) in &hits {
            i.consume_key(*m, *key);
            // Option+letter on macOS also types a character; drop it.
            if m.alt && !m.command {
                i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
            }
        }
    });
    hits.into_iter().map(|(a, _, _)| a).collect()
}

/// Label of an action's primary shortcut (`⇧⌘F`), if bound.
pub fn label(keymap: &Keymap, action: Action) -> String {
    keymap.label(action).unwrap_or_default()
}

/// Translated action label.
pub fn action_label(action: Action) -> &'static str {
    tr(action.label())
}

const CATEGORIES: &[ActionCategory] = &[
    ActionCategory::Sql,
    ActionCategory::Editor,
    ActionCategory::Data,
    ActionCategory::Transactions,
    ActionCategory::Connections,
    ActionCategory::Navigation,
    ActionCategory::View,
    ActionCategory::Tools,
    ActionCategory::Application,
    ActionCategory::Help,
];

/// Read-only overview of all bound shortcuts, grouped by category.
pub fn shortcuts_table(ui: &mut egui::Ui, cx: &mut AppContext) {
    egui::ScrollArea::vertical().max_height(520.0).show(ui, |ui| {
        for cat in CATEGORIES {
            let actions: Vec<Action> = Action::ALL.iter().copied().filter(|a| a.category() == *cat && !cx.keymap.shortcuts(*a).is_empty()).collect();
            if actions.is_empty() {
                continue;
            }
            ui.add_space(4.0);
            ui.strong(tr(cat.label()));
            egui::Grid::new(("shortcuts", cat.label())).num_columns(2).striped(true).spacing([24.0, 4.0]).min_col_width(240.0).show(ui, |ui| {
                for a in actions {
                    ui.label(action_label(a));
                    let keys: Vec<String> = cx.keymap.shortcuts(a).iter().map(|s| s.format(IS_MAC)).collect();
                    ui.monospace(keys.join("   "));
                    ui.end_row();
                }
            });
        }
    });
}
