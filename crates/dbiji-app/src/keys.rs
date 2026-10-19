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

/// Colour cue of an action's icon: green to run, red to stop.
pub fn action_color(action: Action, dark: bool) -> Option<egui::Color32> {
    match action {
        Action::ExecuteStatement | Action::ExecuteScript | Action::ExecuteInNewTab => Some(crate::theme::success_color(dark)),
        Action::CancelQuery => Some(crate::theme::error_color(dark)),
        _ => None,
    }
}

/// Toolbar / palette icon of an action.
pub fn action_icon(action: Action) -> &'static str {
    use egui_phosphor::regular as i;
    match action {
        Action::ExecuteStatement => i::PLAY,
        Action::ExecuteScript => i::FAST_FORWARD,
        Action::ExecuteSelection => i::TEXT_AA,
        Action::ExecuteInNewTab => i::PLUS_SQUARE,
        Action::ExplainPlan | Action::ExplainAnalyze | Action::ShowPlanDiagram => i::TREE_VIEW,
        Action::CancelQuery => i::STOP,
        Action::NewSqlEditor => i::FILE_SQL,
        Action::OpenSqlConsole => i::TERMINAL_WINDOW,
        Action::SelectConnection => i::PLUGS,
        Action::SelectSchema => i::DATABASE,
        Action::ToggleResultsPanel | Action::MaximizeResults => i::ROWS,
        Action::FormatSql => i::TEXT_INDENT,
        Action::Save | Action::SaveData => i::FLOPPY_DISK,
        Action::Find | Action::FindNext | Action::FindPrevious => i::MAGNIFYING_GLASS,
        Action::Replace => i::SWAP,
        Action::GoToLine => i::ARROW_LINE_DOWN,
        Action::ToggleComment | Action::ToggleBlockComment => i::CHAT_TEXT,
        Action::Autocomplete => i::LIST_MAGNIFYING_GLASS,
        Action::UpperCase | Action::LowerCase => i::TEXT_AA,
        Action::CancelDataChanges => i::ARROW_COUNTER_CLOCKWISE,
        Action::Export => i::EXPORT,
        Action::Import => i::DOWNLOAD_SIMPLE,
        Action::Commit => i::CHECK_CIRCLE,
        Action::Rollback => i::ARROW_U_UP_LEFT,
        Action::ToggleAutoCommit => i::TOGGLE_RIGHT,
        Action::NewConnection => i::PLUS_CIRCLE,
        Action::EditConnection => i::PENCIL_SIMPLE,
        Action::Connect => i::PLUGS_CONNECTED,
        Action::Disconnect => i::PLUGS,
        Action::Reconnect => i::ARROWS_CLOCKWISE,
        Action::Refresh => i::ARROW_CLOCKWISE,
        Action::ToggleNavigator => i::SIDEBAR,
        Action::ToggleTheme => i::CIRCLE_HALF,
        Action::ToggleProperties => i::INFO,
        Action::NewDatabase => i::DATABASE,
        Action::PendingTransactions => i::QUEUE,
        Action::ShowErrorLog => i::WARNING_OCTAGON,
        Action::TipOfTheDay => i::LIGHTBULB,
        Action::CollectDiagnostics => i::FILE_ZIP,
        Action::ToggleResultsLayout => i::SQUARE_SPLIT_HORIZONTAL,
        Action::NextStatement => i::ARROW_LINE_DOWN,
        Action::PreviousStatement => i::ARROW_LINE_UP,
        Action::CopyStatement => i::COPY,
        Action::SelectRowCount => i::HASH,
        Action::EvaluateExpression => i::CALCULATOR,
        Action::OpenDeclaration => i::ARROW_SQUARE_OUT,
        Action::MorphToDelimited => i::LIST_DASHES,
        Action::TrimTrailingSpaces => i::SCISSORS,
        Action::SearchWeb => i::GLOBE,
        Action::ToggleFold => i::CARET_UP_DOWN,
        Action::LoadExecutionPlan => i::FOLDER_OPEN,
        Action::OpenDatabaseObject => i::MAGNIFYING_GLASS,
        Action::CollapseAllFolds => i::ARROWS_IN_LINE_VERTICAL,
        Action::ExpandAllFolds => i::ARROWS_OUT_LINE_VERTICAL,
        Action::ZoomIn => i::MAGNIFYING_GLASS_PLUS,
        Action::ZoomOut => i::MAGNIFYING_GLASS_MINUS,
        Action::ResetZoom => i::MAGNIFYING_GLASS,
        Action::CloseTab => i::X,
        Action::NextTab => i::ARROW_RIGHT,
        Action::PrevTab => i::ARROW_LEFT,
        Action::CommandPalette => i::COMMAND,
        Action::OpenHistory => i::CLOCK_COUNTER_CLOCKWISE,
        Action::OpenErDiagram => i::GRAPH,
        Action::OpenTasks => i::LIST_CHECKS,
        Action::OpenSessionManager => i::USERS,
        Action::AiAssistant => i::SPARKLE,
        Action::OpenSettings => i::GEAR,
        Action::Quit => i::SIGN_OUT,
        Action::Help => i::QUESTION,
        Action::ShowShortcuts => i::KEYBOARD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(ctx: &egui::Context, key: egui::Key, modifiers: egui::Modifiers, keymap: &Keymap, context: ActionContext) -> Vec<Action> {
        let input = egui::RawInput {
            events: vec![egui::Event::ModifiersChanged(modifiers), egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }],
            ..Default::default()
        };
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(input, |ui| {
            actions = take_actions(ui.ctx(), keymap, context);
        });
        out.textures_delta.clear();
        actions
    }

    #[test]
    fn default_shortcuts_dispatch_to_actions() {
        let (keymap, _) = Keymap::from_config(&Default::default(), IS_MAC);
        let ctx = egui::Context::default();
        let cmd = if IS_MAC { egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND } else { egui::Modifiers::CTRL | egui::Modifiers::COMMAND };
        assert_eq!(press(&ctx, egui::Key::Enter, cmd, &keymap, ActionContext::SqlEditor), vec![Action::ExecuteStatement]);
        // Editor shortcuts do not fire outside the editor.
        assert!(press(&ctx, egui::Key::Enter, cmd, &keymap, ActionContext::Global).is_empty());
        assert_eq!(press(&ctx, egui::Key::P, cmd | egui::Modifiers::SHIFT, &keymap, ActionContext::Global), vec![Action::CommandPalette]);
        assert_eq!(press(&ctx, egui::Key::F, cmd | egui::Modifiers::SHIFT, &keymap, ActionContext::SqlEditor), vec![Action::FormatSql]);
    }
}
