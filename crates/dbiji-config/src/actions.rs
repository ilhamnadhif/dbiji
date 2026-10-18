//! Canonical list of user-invokable actions (menus, toolbar, command palette,
//! keyboard shortcuts). Every action has a stable string id used in
//! `keymap.json` and the toolbar configuration, an English label (routed
//! through `dbiji_core::tr` by the UI), a category and a focus context.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Grouping used by the command palette and the keymap editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionCategory {
    /// Running SQL and execution plans.
    Sql,
    /// Text editing in the SQL editor.
    Editor,
    /// Result set / data editor.
    Data,
    /// Commit, rollback, auto-commit.
    Transactions,
    /// Connection management.
    Connections,
    /// Tabs and window navigation.
    Navigation,
    /// Panels and zoom.
    View,
    /// Tool windows (history, diagrams, tasks ...).
    Tools,
    /// Application level actions.
    Application,
    /// Help.
    Help,
}

impl ActionCategory {
    /// English label.
    pub fn label(self) -> &'static str {
        match self {
            ActionCategory::Sql => "SQL",
            ActionCategory::Editor => "Editor",
            ActionCategory::Data => "Data",
            ActionCategory::Transactions => "Transactions",
            ActionCategory::Connections => "Connections",
            ActionCategory::Navigation => "Navigation",
            ActionCategory::View => "View",
            ActionCategory::Tools => "Tools",
            ActionCategory::Application => "Application",
            ActionCategory::Help => "Help",
        }
    }
}

/// Where keyboard focus must be for a shortcut to trigger the action.
/// Shortcuts of different non-global contexts never conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionContext {
    /// Anywhere in the main window.
    Global,
    /// The SQL editor text has focus.
    SqlEditor,
    /// A result / data grid has focus.
    ResultsGrid,
    /// The database navigator has focus.
    Navigator,
}

impl ActionContext {
    /// True when shortcuts in `self` and `other` can be active at the same time.
    pub fn overlaps(self, other: ActionContext) -> bool {
        self == other || self == ActionContext::Global || other == ActionContext::Global
    }
}

macro_rules! actions {
    ($( $(#[$doc:meta])* $variant:ident => ($id:literal, $label:literal, $cat:ident, $ctx:ident) ),* $(,)?) => {
        /// A user-invokable action.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Action {
            $( $(#[$doc])* $variant, )*
        }

        impl Action {
            /// Every action, in menu order.
            pub const ALL: &'static [Action] = &[ $( Action::$variant, )* ];

            /// Stable identifier used in configuration files.
            pub fn id(self) -> &'static str {
                match self { $( Action::$variant => $id, )* }
            }

            /// English label (translate with `dbiji_core::tr`).
            pub fn label(self) -> &'static str {
                match self { $( Action::$variant => $label, )* }
            }

            /// Category for menus and the command palette.
            pub fn category(self) -> ActionCategory {
                match self { $( Action::$variant => ActionCategory::$cat, )* }
            }

            /// Focus context of the action's shortcuts.
            pub fn context(self) -> ActionContext {
                match self { $( Action::$variant => ActionContext::$ctx, )* }
            }

            /// Looks an action up by its [`Action::id`].
            pub fn from_id(id: &str) -> Option<Action> {
                match id { $( $id => Some(Action::$variant), )* _ => None }
            }
        }
    };
}

actions! {
    /// Run the statement under the cursor (or the selection).
    ExecuteStatement => ("sql.execute_statement", "Execute Statement", Sql, SqlEditor),
    /// Run the whole script.
    ExecuteScript => ("sql.execute_script", "Execute Script", Sql, SqlEditor),
    /// Run only the selected text.
    ExecuteSelection => ("sql.execute_selection", "Execute Selection", Sql, SqlEditor),
    /// Run the current statement and show its results in a new tab.
    ExecuteInNewTab => ("sql.execute_in_new_tab", "Execute in New Tab", Sql, SqlEditor),
    /// EXPLAIN the current statement.
    ExplainPlan => ("sql.explain_plan", "Explain Execution Plan", Sql, SqlEditor),
    /// EXPLAIN ANALYZE the current statement.
    ExplainAnalyze => ("sql.explain_analyze", "Explain Analyze", Sql, SqlEditor),
    /// Show the plan of the current statement as a diagram.
    ShowPlanDiagram => ("sql.show_plan_diagram", "Show Plan Diagram", Sql, SqlEditor),
    /// Cancel the running query of the active editor.
    CancelQuery => ("sql.cancel_query", "Cancel Query", Sql, Global),
    /// New SQL script for the selected connection.
    NewSqlEditor => ("sql.new_editor", "New SQL Editor", Sql, Global),
    /// Open a SQL console (not saved as a file).
    OpenSqlConsole => ("sql.open_console", "Open SQL Console", Sql, Global),
    /// Choose the active connection of the editor.
    SelectConnection => ("sql.select_connection", "Select Active Connection", Sql, Global),
    /// Choose the active schema / database of the editor.
    SelectSchema => ("sql.select_schema", "Select Active Schema", Sql, Global),
    /// Show or hide the results panel.
    ToggleResultsPanel => ("sql.toggle_results_panel", "Toggle Results Panel", View, Global),
    /// Maximize or restore the results panel.
    MaximizeResults => ("sql.maximize_results", "Maximize Results Panel", View, Global),
    /// Format the SQL text.
    FormatSql => ("editor.format_sql", "Format SQL", Editor, SqlEditor),
    /// Save the script.
    Save => ("editor.save", "Save", Editor, SqlEditor),
    /// Find text.
    Find => ("editor.find", "Find", Editor, SqlEditor),
    /// Find the next match.
    FindNext => ("editor.find_next", "Find Next", Editor, SqlEditor),
    /// Find the previous match.
    FindPrevious => ("editor.find_previous", "Find Previous", Editor, SqlEditor),
    /// Find and replace.
    Replace => ("editor.replace", "Replace", Editor, SqlEditor),
    /// Jump to a line number.
    GoToLine => ("editor.go_to_line", "Go to Line", Editor, SqlEditor),
    /// Comment or uncomment the selected lines (`--`).
    ToggleComment => ("editor.toggle_comment", "Toggle Line Comment", Editor, SqlEditor),
    /// Wrap the selection in a block comment (`/* */`).
    ToggleBlockComment => ("editor.toggle_block_comment", "Toggle Block Comment", Editor, SqlEditor),
    /// Open the completion popup.
    Autocomplete => ("editor.autocomplete", "Autocomplete", Editor, SqlEditor),
    /// Upper-case the selection.
    UpperCase => ("editor.upper_case", "To Upper Case", Editor, SqlEditor),
    /// Lower-case the selection.
    LowerCase => ("editor.lower_case", "To Lower Case", Editor, SqlEditor),
    /// Save pending data changes of the grid.
    SaveData => ("data.save", "Save Data Changes", Data, ResultsGrid),
    /// Discard pending data changes of the grid.
    CancelDataChanges => ("data.cancel_changes", "Cancel Data Changes", Data, ResultsGrid),
    /// Export data.
    Export => ("data.export", "Export Data", Data, Global),
    /// Import data.
    Import => ("data.import", "Import Data", Data, Global),
    /// Commit the open transaction.
    Commit => ("tx.commit", "Commit", Transactions, Global),
    /// Roll back the open transaction.
    Rollback => ("tx.rollback", "Rollback", Transactions, Global),
    /// Switch between auto-commit and manual commit.
    ToggleAutoCommit => ("tx.toggle_auto_commit", "Toggle Auto-Commit", Transactions, Global),
    /// Create a connection.
    NewConnection => ("conn.new", "New Connection", Connections, Global),
    /// Edit the selected connection.
    EditConnection => ("conn.edit", "Edit Connection", Connections, Global),
    /// Connect the selected connection.
    Connect => ("conn.connect", "Connect", Connections, Global),
    /// Disconnect the selected connection.
    Disconnect => ("conn.disconnect", "Disconnect", Connections, Global),
    /// Reconnect the selected connection.
    Reconnect => ("conn.reconnect", "Reconnect", Connections, Global),
    /// Refresh the selected navigator node or result set.
    Refresh => ("view.refresh", "Refresh", View, Global),
    /// Show or hide the navigator.
    ToggleNavigator => ("view.toggle_navigator", "Toggle Navigator", View, Global),
    /// Increase the font size.
    ZoomIn => ("view.zoom_in", "Zoom In", View, Global),
    /// Decrease the font size.
    ZoomOut => ("view.zoom_out", "Zoom Out", View, Global),
    /// Reset the font size.
    ResetZoom => ("view.reset_zoom", "Reset Zoom", View, Global),
    /// Close the active tab.
    CloseTab => ("tab.close", "Close Tab", Navigation, Global),
    /// Activate the next tab.
    NextTab => ("tab.next", "Next Tab", Navigation, Global),
    /// Activate the previous tab.
    PrevTab => ("tab.previous", "Previous Tab", Navigation, Global),
    /// Open the command palette.
    CommandPalette => ("app.command_palette", "Command Palette", Navigation, Global),
    /// Open the query manager (history).
    OpenHistory => ("tools.query_history", "Query History", Tools, Global),
    /// Open an ER diagram.
    OpenErDiagram => ("tools.er_diagram", "ER Diagram", Tools, Global),
    /// Open the task manager.
    OpenTasks => ("tools.tasks", "Tasks", Tools, Global),
    /// Open the session manager.
    OpenSessionManager => ("tools.session_manager", "Session Manager", Tools, Global),
    /// Open the AI assistant.
    AiAssistant => ("tools.ai_assistant", "AI Assistant", Tools, Global),
    /// Open the settings.
    OpenSettings => ("app.settings", "Settings", Application, Global),
    /// Quit DBiji.
    Quit => ("app.quit", "Quit", Application, Global),
    /// Open the documentation.
    Help => ("help.contents", "Help", Help, Global),
    /// Show all keyboard shortcuts.
    ShowShortcuts => ("help.shortcuts", "Keyboard Shortcuts", Help, Global),
}

impl Action {
    /// Default toolbar items (action ids, in order).
    pub fn default_toolbar() -> Vec<Action> {
        vec![
            Action::NewConnection,
            Action::NewSqlEditor,
            Action::SelectConnection,
            Action::SelectSchema,
            Action::ExecuteStatement,
            Action::ExecuteScript,
            Action::ExplainPlan,
            Action::CancelQuery,
            Action::Commit,
            Action::Rollback,
            Action::ToggleAutoCommit,
            Action::FormatSql,
            Action::OpenHistory,
        ]
    }

    /// Actions of one category.
    pub fn in_category(category: ActionCategory) -> impl Iterator<Item = Action> {
        Action::ALL
            .iter()
            .copied()
            .filter(move |a| a.category() == category)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl Serialize for Action {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = String::deserialize(d)?;
        Action::from_id(&id)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown action '{id}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_roundtrip() {
        let mut seen = HashSet::new();
        for a in Action::ALL {
            assert!(seen.insert(a.id()), "duplicate id {}", a.id());
            assert_eq!(Action::from_id(a.id()), Some(*a));
            assert!(!a.label().is_empty());
            assert!(a.id().contains('.'));
        }
        assert!(Action::ALL.len() >= 55);
        assert_eq!(Action::from_id("nope"), None);
    }

    #[test]
    fn serde_uses_ids() {
        let s = serde_json::to_string(&Action::FormatSql).unwrap();
        assert_eq!(s, "\"editor.format_sql\"");
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, Action::FormatSql);
        assert!(serde_json::from_str::<Action>("\"x.y\"").is_err());
    }

    #[test]
    fn contexts() {
        assert!(ActionContext::Global.overlaps(ActionContext::ResultsGrid));
        assert!(!ActionContext::SqlEditor.overlaps(ActionContext::ResultsGrid));
        assert_eq!(Action::SaveData.context(), ActionContext::ResultsGrid);
        assert_eq!(Action::Save.context(), ActionContext::SqlEditor);
        assert!(Action::in_category(ActionCategory::Transactions).count() == 3);
        assert!(!Action::default_toolbar().is_empty());
    }
}
