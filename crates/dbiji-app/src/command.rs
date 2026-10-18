//! Commands that UI components ask the application to perform. Components push
//! them into `AppContext::commands`; the app executes them at the end of the frame.

use crate::tabs::TabView;
use dbiji_core::meta::{ObjectRef, SchemaRef};
use dbiji_core::DbKind;
use std::path::PathBuf;

/// Page of the object editor to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObjectPage {
    #[default]
    Properties,
    Data,
    Ddl,
    Dependencies,
    Statistics,
    Diagram,
}

pub enum AppCommand {
    /// A keymap action (palette, toolbar, menu) dispatched like its shortcut.
    Action(crate::keys::Action),
    // ------------------------------------------------------------------ tabs
    OpenTab(Box<dyn TabView>),
    CloseTab(u64),
    CloseOtherTabs(u64),
    CloseAllTabs,

    // --------------------------------------------------------------- editors
    NewSqlEditor { connection: Option<String>, schema: Option<SchemaRef>, text: Option<String> },
    OpenScript(PathBuf),
    /// Open the SQL console (terminal-style output) for a connection.
    NewSqlConsole { connection: String },
    /// Generate SELECT / INSERT / UPDATE / DELETE / UPSERT / DDL for an object
    /// and open it in a new editor.
    GenerateSql { connection: String, object: ObjectRef, kind: String },

    // ----------------------------------------------------------- connections
    Connect(String),
    Disconnect(String),
    Reconnect(String),
    DisconnectAll,
    NewConnection { kind: Option<DbKind>, folder: Option<String> },
    EditConnection(String),
    DuplicateConnection(String),
    DeleteConnection(String),
    ImportConnections,
    SaveConnections,
    /// Create the SQLite sample database and a connection to it.
    CreateSampleDatabase,

    // --------------------------------------------------------------- objects
    OpenObject { connection: String, object: ObjectRef, page: ObjectPage },
    RefreshMetadata { connection: String, object: Option<ObjectRef>, schema: Option<SchemaRef> },
    CreateTable { connection: String, schema: SchemaRef },
    EditTable { connection: String, table: ObjectRef },
    DropObjects { connection: String, objects: Vec<ObjectRef> },
    AddBookmark { connection: String, object: ObjectRef },

    // ----------------------------------------------------------------- tools
    OpenSettings,
    /// Preferences were edited: apply and save them.
    SettingsChanged,
    OpenHistory,
    OpenTasks,
    OpenErDiagram { connection: String, schema: SchemaRef, tables: Vec<String> },
    OpenSavedDiagram(PathBuf),
    OpenQueryBuilder { connection: String, schema: Option<SchemaRef> },
    OpenSessionManager { connection: String },
    OpenLockManager { connection: String },
    OpenDashboard { connection: String },
    OpenSchemaCompare { connection: Option<String>, schema: Option<SchemaRef> },
    OpenDataCompare { connection: Option<String>, table: Option<ObjectRef> },
    OpenAiAssistant { connection: Option<String> },
    OpenExport { connection: String, objects: Vec<ObjectRef>, sql: Option<String> },
    OpenImport { connection: String, table: Option<ObjectRef>, schema: Option<SchemaRef> },
    OpenMigration { connection: String, objects: Vec<ObjectRef> },
    OpenBackup { connection: String, schema: Option<SchemaRef>, restore: bool },
    OpenMaintenance { connection: String, objects: Vec<ObjectRef> },
    OpenMockData { connection: String, table: ObjectRef },
    OpenDebugger { connection: String, routine: ObjectRef },
    ChangePassword { connection: String },
    OpenSearch { connection: Option<String>, data: bool },

    // -------------------------------------------------------------------- ui
    ShowPalette,
    ToggleNavigator,
    ToggleOperations,
    ShowShortcuts,
    ShowAbout,
    Quit,
}
