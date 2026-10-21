//! The main menu's items: what they do (`DbijiApp::run_menu_action`) and their
//! state, for the in-window menu bar and macOS's native menu bar alike.

use dbiji_config::settings::Theme;

/// An item of the main menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuAction {
    NewSqlEditor,
    OpenScript,
    Settings,
    Quit,
    NewConnection,
    ImportConnections,
    NewDatabase,
    NewSchema,
    DisconnectAll,
    PendingTransactions,
    CompareSchemas,
    CompareData,
    Search,
    QueryHistory,
    Tasks,
    AiAssistant,
    BackgroundOperations,
    ToggleNavigator,
    PropertiesPanel,
    Theme(Theme),
    CommandPalette,
    KeyboardShortcuts,
    DatabaseDrivers,
    CreateSampleDatabase,
    TipOfTheDay,
    ErrorLog,
    CollectDiagnostics,
    WhatsNew,
    CheckForUpdates,
    About,
}

/// What changes in the menu: items that can be off, check marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    /// The selected (connected) server can create a database / a schema.
    pub new_database: bool,
    pub new_schema: bool,
    /// The navigator's properties panel is shown.
    pub properties: bool,
    pub theme: Theme,
    /// No diagnostic collection is running.
    pub diagnostics: bool,
    /// No update check is running.
    pub updates: bool,
}
