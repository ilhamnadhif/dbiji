//! Main window: menu bar, toolbar, navigator, dock area, status bar, dialogs.

use crate::backend::Backend;
use crate::command::AppCommand;
use crate::conn::{ConnectionManager, Credentials};
use crate::cx::AppContext;
use crate::dialogs::Dialogs;
use crate::nav::Navigator;
use crate::ops::Operations;
use crate::services::{Services, Settings};
use crate::tabs::{DockViewer, Tab, TabView};
use crate::toasts::Toasts;
use dbiji_core::tr;
use egui::{RichText, Ui};
use egui_dock::{DockArea, DockState};
use egui_phosphor::regular as icon;
use std::sync::Arc;
use std::time::Instant;

/// SQLite / DuckDB database file (by extension).
pub fn database_file_kind(path: &std::path::Path) -> Option<dbiji_core::DbKind> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "db" | "sqlite" | "sqlite3" | "db3" => Some(dbiji_core::DbKind::Sqlite),
        "duckdb" | "ddb" => Some(dbiji_core::DbKind::DuckDb),
        _ => None,
    }
}

pub fn is_database_file(path: &std::path::Path) -> bool {
    database_file_kind(path).is_some()
}

pub struct DbijiApp {
    pub backend: Arc<Backend>,
    pub conns: ConnectionManager,
    pub services: Services,
    pub settings: Settings,
    pub keymap: crate::keys::Keymap,
    pub dock: DockState<Tab>,
    next_tab_id: u64,
    pub nav: Navigator,
    project: crate::project_view::ProjectView,
    /// Left panel shows the project (scripts, bookmarks, diagrams) instead of the database navigator.
    show_project: bool,
    show_nav: bool,
    pub toasts: Toasts,
    pub ops: Operations,
    commands: Vec<AppCommand>,
    pub dialogs: Dialogs,
    started: Instant,
    first_frame: bool,
    /// Exit after the first frame (startup benchmark).
    pub exit_after_first_frame: bool,
    active_tab: Option<u64>,
    /// Benchmark mode: quit after this instant.
    exit_at: Option<Instant>,
    /// The window was asked to close while tabs had unsaved changes.
    confirm_quit: bool,
    quit_confirmed: bool,
    /// "Generate SQL" requests waiting for the table definition.
    pending_generate: Vec<(String, dbiji_core::ObjectRef, String)>,
    /// Tab last revealed in the navigator (Link with editor).
    linked_tab: Option<u64>,
    /// "Execute…" of routines waiting for their definition.
    pending_routines: Vec<(String, dbiji_core::ObjectRef, crate::backend::Job<dbiji_core::DbResult<dbiji_core::ddl::RoutineDef>>)>,
    script_counter: u32,
    /// `DBIJI_BENCH_SCENARIO=workload`: connections still to open editors for.
    bench_workload: Option<Vec<String>>,
    /// "What's new" and update checks.
    pub updates: crate::updates::Updates,
    /// The native application menu (About, Check for Updates…, Quit).
    #[cfg(target_os = "macos")]
    pub app_menu: Option<crate::macmenu::AppMenu>,
    /// Settings or connections existed before this start (not a first start).
    pub existing_install: bool,
    /// Tip of the day window.
    pub tip: Option<crate::tips::TipWindow>,
    /// Pending transactions window.
    pub pending_tx: crate::pending_tx::PendingTxDialog,
    /// Collect diagnostic info running.
    diagnostics: Option<crate::backend::Job<dbiji_core::DbResult<std::path::PathBuf>>>,
}

impl DbijiApp {
    pub fn new(backend: Arc<Backend>, conns: ConnectionManager, services: Services, settings: Settings) -> Self {
        let (keymap, keymap_warnings) = crate::keys::Keymap::from_config(&settings.keymap, crate::keys::IS_MAC);
        let mut services = services;
        services.warnings.extend(keymap_warnings);
        let mut app = DbijiApp {
            backend,
            conns,
            services,
            settings,
            keymap,
            dock: DockState::new(Vec::new()),
            next_tab_id: 1,
            nav: Navigator::default(),
            project: Default::default(),
            show_project: false,
            show_nav: true,
            toasts: Toasts::default(),
            ops: Operations::default(),
            commands: Vec::new(),
            dialogs: Dialogs::default(),
            started: Instant::now(),
            first_frame: true,
            exit_after_first_frame: false,
            active_tab: None,
            exit_at: std::env::var("DBIJI_BENCH_EXIT_AFTER_SECS").ok().and_then(|v| v.parse::<u64>().ok()).map(|s| Instant::now() + std::time::Duration::from_secs(s)),
            pending_generate: Vec::new(),
            pending_routines: Vec::new(),
            linked_tab: None,
            script_counter: 0,
            confirm_quit: false,
            quit_confirmed: false,
            bench_workload: None,
            updates: Default::default(),
            #[cfg(target_os = "macos")]
            app_menu: None,
            existing_install: false,
            tip: None,
            pending_tx: Default::default(),
            diagnostics: None,
        };
        app.existing_install = crate::updates::is_existing_install(&app.services.paths);
        app.updates.data_dir = Some(app.services.paths.data_dir().to_path_buf());
        app.open_tab(Box::new(crate::tabs::welcome::WelcomeTab));
        app
    }

    /// Queue a command for the next frame (as a menu item would).
    pub fn push_command(&mut self, cmd: AppCommand) {
        self.commands.push(cmd);
    }

    /// Open a tab, or focus an existing tab with the same dedupe key.
    pub fn open_tab(&mut self, view: Box<dyn TabView>) -> u64 {
        if let Some(key) = view.dedupe_key()
            && let Some(path) = self.dock.find_tab_from(|t| t.view.dedupe_key().as_deref() == Some(key.as_str()))
        {
            let id = self.dock.iter_all_tabs().find(|(p, _)| *p == path).map(|(_, t)| t.id).unwrap_or(0);
            self.focus_tab_path(path);
            return id;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.dock.push_to_focused_leaf(Tab::new(id, view));
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            self.focus_tab_path(path);
        }
        id
    }

    /// Make a tab the active tab of its leaf and that leaf the focused one, so
    /// shortcuts and the status bar refer to it.
    fn focus_tab_path(&mut self, path: egui_dock::TabPath) {
        let _ = self.dock.set_active_tab(path);
        self.dock.set_focused_node_and_surface(path.node_path());
    }

    /// Keep a focused leaf whenever tabs exist (no focus right after start or
    /// after the focused leaf was closed).
    fn ensure_focus(&mut self) {
        if self.dock.focused_leaf().is_some() && self.dock.find_active_focused().is_some() {
            return;
        }
        let first = self.dock.iter_all_tabs().map(|(p, _)| p).next();
        if let Some(path) = first {
            self.dock.set_focused_node_and_surface(path.node_path());
        }
    }

    fn close_tab(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id)
            && let Some(mut tab) = self.dock.remove_tab(path)
        {
            let mut cx = AppContext {
                backend: &self.backend,
                conns: &mut self.conns,
                services: &self.services,
                settings: &mut self.settings,
                commands: &mut self.commands,
                toasts: &mut self.toasts,
                ops: &mut self.ops,
                keymap: &self.keymap,
                tab_active: false,
                current_tab: None,
            };
            if !tab.view.on_close(&mut cx) {
                // put it back
                self.dock.push_to_focused_leaf(tab);
            }
        }
    }

    /// Request connecting, prompting for credentials when needed.
    pub(crate) fn request_connect(&mut self, id: &str) {
        if self.conns.secrets.is_locked() {
            // Encrypted vault locked (auto-lock): ask for the master password first.
            self.dialogs.vault_open = true;
            self.toasts.info(tr("Unlock the saved passwords, then connect again."));
            return;
        }
        let missing = self.conns.missing_credentials(id);
        if missing.is_empty() {
            if let Err(e) = self.conns.connect(&self.backend, id, Credentials::default()) {
                self.toasts.error(tr("Cannot connect"), Some(e.to_string()));
            }
        } else {
            self.dialogs.open_credentials(id, missing, &self.conns);
        }
    }

    fn process_commands(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        if let Some(menu) = &self.app_menu {
            self.commands.extend(menu.commands());
        }
        let commands = std::mem::take(&mut self.commands);
        for cmd in commands {
            self.handle_command(ctx, cmd);
        }
    }

    fn handle_command(&mut self, ctx: &egui::Context, cmd: AppCommand) {
        match cmd {
            AppCommand::OpenTab(view) => {
                self.open_tab(view);
            }
            AppCommand::CloseTab(id) => self.close_tab(id),
            AppCommand::CloseOtherTabs(keep) => {
                let ids: Vec<u64> = self.dock.iter_all_tabs().map(|(_, t)| t.id).filter(|i| *i != keep).collect();
                for id in ids {
                    self.close_tab(id);
                }
            }
            AppCommand::CloseAllTabs => {
                let ids: Vec<u64> = self.dock.iter_all_tabs().map(|(_, t)| t.id).collect();
                for id in ids {
                    self.close_tab(id);
                }
            }
            AppCommand::Connect(id) => self.request_connect(&id),
            AppCommand::Disconnect(id) => self.conns.disconnect(&self.backend, &id),
            AppCommand::Reconnect(id) => {
                self.conns.disconnect(&self.backend, &id);
                self.request_connect(&id);
            }
            AppCommand::DisconnectAll => self.conns.disconnect_all(&self.backend),
            AppCommand::NewConnection { kind, folder } => self.dialogs.new_connection(kind, folder, &self.conns, self.settings.navigator.default_view),
            AppCommand::EditConnection(id) => self.dialogs.edit_connection(&id, &self.conns),
            AppCommand::DuplicateConnection(id) => match self.conns.store.duplicate(&id) {
                Ok(dup) => {
                    if let Err(e) = self.services.secrets.read().copy_connection_secrets(&id, &dup.id) {
                        self.toasts.error(tr("Cannot copy the saved passwords"), Some(e.to_string()));
                    }
                    self.conns.dirty = true;
                    self.nav.select(crate::nav::NavNode::Connection(dup.id.clone()));
                }
                Err(e) => self.toasts.error(tr("Cannot duplicate the connection"), Some(e.to_string())),
            },
            AppCommand::SaveConnections => self.conns.dirty = true,
            AppCommand::CreateSampleDatabase => {
                let dir = self.services.paths.data_dir().join("samples");
                let _ = std::fs::create_dir_all(&dir);
                let mut path = dir.join("dbiji-sample.db");
                let mut n = 2;
                while path.exists() {
                    path = dir.join(format!("dbiji-sample-{n}.db"));
                    n += 1;
                }
                match crate::sample::create(&path) {
                    Ok(()) => {
                        self.open_path(&path, false);
                        self.toasts.success(tr("Sample database created"));
                    }
                    Err(e) => self.toasts.error(tr("Cannot create the sample database"), Some(e.full_text())),
                }
            }
            AppCommand::ImportConnections => {
                if self.dialogs.import.is_none() {
                    self.dialogs.import = Some(crate::dialogs::import::ImportDialog::default());
                }
            }
            AppCommand::DeleteConnection(id) => self.dialogs.confirm_delete_connection(&id, &self.conns),
            AppCommand::ShowPalette => {
                crate::palette::reset(ctx);
                self.dialogs.palette_open = true;
            }
            AppCommand::Action(a) => self.dispatch_action(ctx, a),
            AppCommand::NewSqlEditor { connection, schema, text } => {
                self.script_counter += 1;
                let title = format!("Script-{}", self.script_counter);
                let tab = crate::tabs::sql_editor::SqlEditorTab::new(connection.clone(), schema, text.unwrap_or_default(), None, false, title);
                if let Some(c) = &connection
                    && !self.conns.is_connected(c)
                {
                    self.request_connect(c);
                }
                self.open_tab(Box::new(tab));
            }
            AppCommand::NewSqlConsole { connection } => {
                let name = self.conns.config(&connection).map(|c| c.name.clone()).unwrap_or_default();
                let title = format!("{} {}", tr("Console"), name);
                if !self.conns.is_connected(&connection) {
                    self.request_connect(&connection);
                }
                self.open_tab(Box::new(crate::tabs::sql_editor::SqlEditorTab::new(Some(connection), None, String::new(), None, true, title)));
            }
            AppCommand::OpenScript(path) => {
                self.open_script(path);
            }
            AppCommand::ExecuteRoutine { connection, routine } => {
                let Some(live) = self.conns.live_mut(&connection) else { return };
                let Some(session) = live.ensure_session(&self.backend, routine.database.as_deref()) else { return };
                let r = routine.clone();
                let job = self.backend.spawn(async move { session.lock().await.routine_definition(&r).await });
                self.pending_routines.push((connection, routine, job));
            }
            AppCommand::GenerateSql { connection, object, kind } => {
                if object.kind.is_table_like() {
                    if let Some(live) = self.conns.live_mut(&connection) {
                        let main = live.main.clone();
                        live.meta.request_table_def(&self.backend, &main, &object);
                    }
                    self.pending_generate.push((connection, object, kind));
                } else {
                    self.commands.push(AppCommand::OpenObject { connection, object, page: crate::command::ObjectPage::Ddl });
                }
            }
            AppCommand::OpenSettings => {
                self.open_tab(Box::new(crate::tabs::settings::SettingsTab::default()));
            }
            AppCommand::SettingsChanged => self.apply_settings(ctx),
            AppCommand::OpenErDiagram { connection, schema, tables } => {
                self.open_tab(Box::new(crate::tabs::er::ErTab::new(connection, schema, tables)));
            }
            AppCommand::OpenSessionManager { connection } => {
                self.open_tab(Box::new(crate::tabs::admin::SessionManagerTab::new(connection)));
            }
            AppCommand::OpenLockManager { connection } => {
                self.open_tab(Box::new(crate::tabs::admin::LockManagerTab::new(connection)));
            }
            AppCommand::OpenDashboard { connection } => {
                self.open_tab(Box::new(crate::tabs::admin::DashboardTab::new(connection)));
            }
            AppCommand::OpenQueryBuilder { connection, schema } => {
                self.open_tab(Box::new(crate::tabs::query_builder::QueryBuilderTab::new(connection, schema)));
            }
            AppCommand::OpenSchemaCompare { connection, schema } => {
                self.open_tab(Box::new(crate::tabs::compare::SchemaCompareTab::new(connection, schema)));
            }
            AppCommand::DropObjects { connection, objects } => {
                if !objects.is_empty() && self.conns.is_connected(&connection) {
                    self.dialogs.drop = Some(crate::dialogs::drop::DropDialog::new(connection, objects));
                }
            }
            AppCommand::AddBookmark { connection, object } => {
                let label = object.display_path();
                let r = self
                    .services
                    .projects
                    .as_ref()
                    .ok_or_else(|| dbiji_core::DbError::config("no project"))
                    .and_then(|p| p.default_project())
                    .and_then(|p| p.add_bookmark(dbiji_config::Bookmark::new(connection, object, label)));
                match r {
                    Ok(_) => self.toasts.success(tr("Bookmark added")),
                    Err(e) => self.toasts.error(tr("Cannot add the bookmark"), Some(e.to_string())),
                }
            }
            AppCommand::OpenSavedDiagram(path) => self.open_saved_diagram(&path),
            AppCommand::OpenExport { connection, objects, sql } => {
                self.open_tab(Box::new(crate::tabs::transfer::ExportTab::new(connection, objects, sql)));
            }
            AppCommand::OpenImport { connection, table, schema } => {
                self.open_tab(Box::new(crate::tabs::transfer::ImportTab::new(connection, table, schema)));
            }
            AppCommand::OpenMigration { connection, objects } => {
                self.open_tab(Box::new(crate::tabs::transfer::MigrationTab::new(connection, objects)));
            }
            AppCommand::OpenMockData { connection, table } => {
                self.open_tab(Box::new(crate::tabs::transfer::MockDataTab::new(connection, table)));
            }
            AppCommand::OpenTasks => {
                self.open_tab(Box::new(crate::tabs::tasks::TasksTab::new()));
            }
            AppCommand::OpenBackup { connection, schema, restore } => {
                self.open_tab(Box::new(crate::tabs::dbtools::BackupTab::new(connection, schema, restore)));
            }
            AppCommand::OpenMaintenance { connection, objects } => {
                self.open_tab(Box::new(crate::tabs::dbtools::MaintenanceTab::new(connection, objects)));
            }
            AppCommand::ChangePassword { connection } => {
                self.open_tab(Box::new(crate::tabs::dbtools::ChangePasswordTab::new(connection)));
            }
            AppCommand::NewDatabase { connection } => {
                if self.conns.is_connected(&connection) {
                    self.dialogs.create_db = Some(crate::dialogs::create_db::CreateDatabaseDialog::database(connection));
                } else {
                    self.toasts.info(tr("Connect first, then create the database."));
                }
            }
            AppCommand::NewSchema { connection, database } => {
                if self.conns.is_connected(&connection) {
                    self.dialogs.create_db = Some(crate::dialogs::create_db::CreateDatabaseDialog::schema(connection, database));
                } else {
                    self.toasts.info(tr("Connect first, then create the database."));
                }
            }
            AppCommand::OpenFdwConfigurator { connection } => {
                self.open_tab(Box::new(crate::tabs::dbtools::FdwConfigTab::new(connection)));
            }
            AppCommand::OpenDebugger { connection, routine } => {
                self.open_tab(Box::new(crate::tabs::dbtools::DebuggerTab::new(connection, routine)));
            }
            AppCommand::OpenAiAssistant { connection } => {
                self.open_tab(Box::new(crate::tabs::ai::AiAssistantTab::new(connection)));
            }
            AppCommand::OpenDataCompare { connection, table } => {
                self.open_tab(Box::new(crate::tabs::data_compare::DataCompareTab::new(connection, table)));
            }
            AppCommand::CreateTable { connection, schema } => {
                self.open_tab(Box::new(crate::tabs::table_designer::TableDesignerTab::create(connection, schema)));
            }
            AppCommand::EditTable { connection, table } => {
                self.open_tab(Box::new(crate::tabs::table_designer::TableDesignerTab::edit(connection, table)));
            }
            AppCommand::OpenSearch { connection, data } => {
                self.open_tab(Box::new(crate::tabs::search::SearchTab::new(connection, data)));
            }
            AppCommand::OpenHistory => {
                self.open_tab(Box::new(crate::tabs::history::HistoryTab::default()));
            }
            AppCommand::ToggleNavigator => self.show_nav = !self.show_nav,
            AppCommand::ToggleOperations => self.ops.panel_open = !self.ops.panel_open,
            AppCommand::ShowAbout => self.dialogs.about_open = true,
            AppCommand::CheckForUpdates => self.updates.check_now(&self.backend, true),
            AppCommand::ShowShortcuts => self.dialogs.shortcuts_open = true,
            AppCommand::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            AppCommand::RefreshMetadata { connection, object, schema } => {
                if let Some(live) = self.conns.live_mut(&connection) {
                    match (object, schema) {
                        (Some(o), _) => live.meta.clear_object(&o),
                        (None, Some(s)) => live.meta.clear_schema(&s),
                        (None, None) => live.meta.clear(),
                    }
                }
            }
            other => crate::router::route(self, ctx, other),
        }
    }

    /// Keyboard shortcuts of the focused context (active tab) and global ones.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialogs.captures_keyboard() {
            return;
        }
        let context = self.dock.find_active_focused().map(|(_, t)| t.view.focus_context()).unwrap_or(crate::keys::ActionContext::Global);
        for action in crate::keys::take_actions(ctx, &self.keymap, context) {
            self.dispatch_action(ctx, action);
        }
    }

    fn selected_connection(&self) -> Option<String> {
        self.nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string).or_else(|| {
            let active = self.active_tab?;
            self.dock.iter_all_tabs().find(|(_, t)| t.id == active).and_then(|(_, t)| t.view.connection_id().map(str::to_string))
        })
    }

    /// Run an action: the active tab gets the first chance, then the global handler.
    pub fn dispatch_action(&mut self, ctx: &egui::Context, action: crate::keys::Action) {
        use crate::keys::Action as A;
        let handled = match self.dock.find_active_focused() {
            Some((_, tab)) => {
                let mut cx = AppContext {
                    backend: &self.backend,
                    conns: &mut self.conns,
                    services: &self.services,
                    settings: &mut self.settings,
                    commands: &mut self.commands,
                    toasts: &mut self.toasts,
                    ops: &mut self.ops,
                    keymap: &self.keymap,
                    tab_active: true,
                    current_tab: self.active_tab,
                };
                tab.view.on_action(action, &mut cx)
            }
            None => false,
        };
        if handled {
            return;
        }
        let conn = self.selected_connection();
        let cmd = match action {
            A::NewSqlEditor => Some(AppCommand::NewSqlEditor { connection: conn, schema: None, text: None }),
            A::OpenSqlConsole => conn.map(|c| AppCommand::NewSqlConsole { connection: c }),
            A::NewConnection => Some(AppCommand::NewConnection { kind: None, folder: None }),
            A::NewDatabase => match conn {
                Some(c) => Some(AppCommand::NewDatabase { connection: c }),
                None => {
                    self.toasts.info(tr("Select a connection in the navigator first."));
                    None
                }
            },
            A::EditConnection => conn.map(AppCommand::EditConnection),
            A::Connect => conn.map(AppCommand::Connect),
            A::Disconnect => conn.map(AppCommand::Disconnect),
            A::Reconnect => conn.map(AppCommand::Reconnect),
            A::ToggleNavigator => Some(AppCommand::ToggleNavigator),
            A::ToggleResultsLayout => {
                use dbiji_config::settings::ResultsLayout;
                let r = &mut self.settings.results.layout;
                *r = if *r == ResultsLayout::Right { ResultsLayout::Bottom } else { ResultsLayout::Right };
                self.save_settings();
                None
            }
            A::ToggleTheme => {
                self.toggle_theme(ctx);
                None
            }
            A::ToggleProperties => {
                self.settings.navigator.show_properties = !self.settings.navigator.show_properties;
                self.save_settings();
                None
            }
            A::PendingTransactions => {
                self.pending_tx.open = true;
                None
            }
            A::ShowErrorLog => {
                self.open_tab(Box::new(crate::tabs::errorlog::ErrorLogTab::new()));
                None
            }
            A::TipOfTheDay => {
                self.show_tip();
                None
            }
            A::CollectDiagnostics => {
                self.collect_diagnostics(None);
                None
            }
            A::CommandPalette | A::OpenDatabaseObject => Some(AppCommand::ShowPalette),
            A::OpenHistory => Some(AppCommand::OpenHistory),
            A::OpenErDiagram => match (conn, self.nav.selected_schema()) {
                (Some(c), Some(s)) => Some(AppCommand::OpenErDiagram { connection: c, schema: s, tables: Vec::new() }),
                _ => {
                    self.toasts.info(tr("Select a schema in the navigator first."));
                    None
                }
            },
            A::OpenTasks => Some(AppCommand::OpenTasks),
            A::OpenSessionManager => conn.map(|c| AppCommand::OpenSessionManager { connection: c }),
            A::AiAssistant => Some(AppCommand::OpenAiAssistant { connection: conn }),
            A::OpenSettings => Some(AppCommand::OpenSettings),
            A::Quit => Some(AppCommand::Quit),
            A::ShowShortcuts | A::Help => Some(AppCommand::ShowShortcuts),
            A::Export => conn.map(|c| AppCommand::OpenExport { connection: c, objects: Vec::new(), sql: None }),
            A::Import => conn.map(|c| AppCommand::OpenImport { connection: c, table: None, schema: None }),
            A::CloseTab => self.active_tab.map(AppCommand::CloseTab),
            A::NextTab | A::PrevTab => {
                self.cycle_tab(action == A::NextTab);
                None
            }
            A::ZoomIn | A::ZoomOut | A::ResetZoom => {
                let z = ctx.zoom_factor();
                let nz = match action {
                    A::ZoomIn => (z + 0.1).min(3.0),
                    A::ZoomOut => (z - 0.1).max(0.5),
                    _ => 1.0,
                };
                ctx.set_zoom_factor(nz);
                self.settings.general.ui_scale = nz;
                self.save_settings();
                None
            }
            A::Refresh => {
                if let Some(node) = self.nav.selected.clone() {
                    self.nav.refresh_node(&node, &mut self.commands);
                }
                None
            }
            _ => None,
        };
        if let Some(c) = cmd {
            self.commands.push(c);
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        let ids: Vec<u64> = self.dock.iter_all_tabs().map(|(_, t)| t.id).collect();
        if ids.is_empty() {
            return;
        }
        let pos = self.active_tab.and_then(|a| ids.iter().position(|i| *i == a)).unwrap_or(0);
        let next = if forward { (pos + 1) % ids.len() } else { (pos + ids.len() - 1) % ids.len() };
        let id = ids[next];
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            let _ = self.dock.set_active_tab(path);
        }
    }

    /// Benchmark workload (PRD §2): 3 connections and 5 result tabs with all
    /// rows fetched. Servers come from `DBIJI_BENCH_URLS` (comma separated) or
    /// the `DBIJI_TEST_*` variables; passwords are never stored.
    pub fn start_bench_workload(&mut self) {
        let mut urls: Vec<String> = std::env::var("DBIJI_BENCH_URLS").map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        if urls.is_empty() {
            for v in ["DBIJI_TEST_PG", "DBIJI_TEST_MYSQL", "DBIJI_TEST_MARIADB"] {
                if let Ok(u) = std::env::var(v) {
                    urls.push(u);
                }
            }
        }
        let mut ids = Vec::new();
        for (i, url) in urls.iter().take(3).enumerate() {
            let Ok((mut cfg, password)) = dbiji_config::parse_connection_url(url) else { continue };
            cfg.name = format!("bench {}", i + 1);
            cfg.save_password = false;
            let id = cfg.id.clone();
            self.conns.upsert(cfg);
            self.conns.dirty = false;
            let creds = Credentials { password, ..Default::default() };
            if self.conns.connect(&self.backend, &id, creds).is_ok() {
                ids.push(id);
            }
        }
        self.bench_workload = Some(ids);
    }

    fn poll_bench_workload(&mut self) {
        let Some(ids) = self.bench_workload.clone() else { return };
        if ids.iter().any(|id| !self.conns.is_connected(id) && self.conns.status(id) == crate::conn::ConnStatus::Connecting) {
            return;
        }
        self.bench_workload = None;
        let ready: Vec<String> = ids.into_iter().filter(|id| self.conns.is_connected(id)).collect();
        if ready.is_empty() {
            return;
        }
        for n in 0..5 {
            let id = ready[n % ready.len()].clone();
            let kind = self.conns.config(&id).map(|c| c.kind).unwrap_or(dbiji_core::DbKind::Postgres);
            let sql = match kind {
                dbiji_core::DbKind::Postgres | dbiji_core::DbKind::CockroachDb => {
                    "SELECT g AS id, md5(g::text) AS name, g * 1.5 AS amount, now() AS created FROM generate_series(1, 20000) g".to_string()
                }
                _ => "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 200)                       SELECT a.n * 1000 + b.n AS id, MD5(a.n * 1000 + b.n) AS name, (a.n + b.n) * 1.5 AS amount, NOW() AS created                       FROM seq a CROSS JOIN seq b LIMIT 20000"
                    .to_string(),
            };
            let mut tab = crate::tabs::sql_editor::SqlEditorTab::new(Some(id), None, sql, None, false, format!("Bench {}", n + 1));
            tab.auto_run = true;
            tab.fetch_all_results = true;
            self.open_tab(Box::new(tab));
        }
    }

    /// Open a file given on the command line: SQL scripts open in an editor,
    /// SQLite / DuckDB database files become (or reuse) a connection.
    pub fn open_path(&mut self, path: &std::path::Path, execute: bool) {
        let kind = database_file_kind(path);
        let Some(kind) = kind.filter(|k| self.conns.registry.get(*k).is_some()) else {
            let id = self.open_script(path.to_path_buf());
            if let (true, Some(id)) = (execute, id)
                && let Some((_, tab)) = self.dock.iter_all_tabs_mut().find(|(_, t)| t.id == id)
                && let Some(ed) = tab.view.as_any_mut().downcast_mut::<crate::tabs::sql_editor::SqlEditorTab>()
            {
                ed.auto_run = true;
            }
            return;
        };
        let full = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let file = full.display().to_string();
        let existing = self.conns.configs().iter().find(|c| c.kind == kind && c.file_path.as_deref() == Some(file.as_str())).map(|c| c.id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                let name = full.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| file.clone());
                let mut cfg = dbiji_core::config::ConnectionConfig::new(kind, self.conns.store.unique_name(&name, None));
                cfg.file_path = Some(file);
                cfg.auth = dbiji_core::config::AuthMethod::None;
                let id = cfg.id.clone();
                self.conns.upsert(cfg);
                id
            }
        };
        self.request_connect(&id);
        self.nav.select(crate::nav::NavNode::Connection(id));
    }

    /// Reopen the editors of the previous session (FR-S1).
    pub fn restore_session(&mut self) {
        let Some(projects) = self.services.projects.as_ref() else { return };
        let state = projects.load_session();
        let mut opened = 0;
        let mut to_connect: Vec<String> = Vec::new();
        for t in &state.tabs {
            if matches!(t.kind, dbiji_config::project::TabKind::ObjectEditor | dbiji_config::project::TabKind::DataEditor | dbiji_config::project::TabKind::ErDiagram)
                && let Some(c) = t.connection_id.clone().filter(|c| self.conns.config(c).is_some() && !to_connect.contains(c))
            {
                to_connect.push(c);
            }
        }
        for c in to_connect {
            if !self.conns.is_connected(&c) {
                self.request_connect(&c);
            }
        }
        for t in state.tabs {
            match t.kind {
                dbiji_config::project::TabKind::SqlScript if t.path.as_ref().is_some_and(|p| p.exists()) => {
                    if let Some(p) = t.path.clone() {
                        let _ = self.open_script(p);
                        opened += 1;
                    }
                }
                dbiji_config::project::TabKind::SqlScript | dbiji_config::project::TabKind::SqlConsole => {
                    let conn = t.connection_id.clone().filter(|c| self.conns.config(c).is_some());
                    let schema = (t.schema.is_some() || t.database.is_some()).then(|| dbiji_core::meta::SchemaRef::new(t.database.clone(), t.schema.clone()));
                    let console = t.kind == dbiji_config::project::TabKind::SqlConsole;
                    self.script_counter += 1;
                    let title = t.title.clone().unwrap_or_else(|| format!("Script-{}", self.script_counter));
                    self.open_tab(Box::new(crate::tabs::sql_editor::SqlEditorTab::new(conn, schema, t.content.clone().unwrap_or_default(), None, console, title)));
                    opened += 1;
                }
                dbiji_config::project::TabKind::ObjectEditor | dbiji_config::project::TabKind::DataEditor => {
                    let (Some(conn), Some(obj)) = (t.connection_id.clone().filter(|c| self.conns.config(c).is_some()), t.object.clone()) else { continue };
                    let page = if t.kind == dbiji_config::project::TabKind::DataEditor { crate::command::ObjectPage::Data } else { crate::command::ObjectPage::Properties };
                    self.open_tab(Box::new(crate::tabs::object::ObjectEditorTab::new(conn, obj, page)));
                    opened += 1;
                }
                dbiji_config::project::TabKind::ErDiagram => {
                    let Some(conn) = t.connection_id.clone().filter(|c| self.conns.config(c).is_some()) else { continue };
                    match &t.path {
                        Some(p) => {
                            let dir = self.services.projects.as_ref().and_then(|m| m.default_project().ok()).map(|pr| pr.diagrams_dir());
                            if let Some(dir) = dir {
                                self.open_saved_diagram(&dir.join(p));
                            }
                        }
                        None => {
                            let schema = dbiji_core::meta::SchemaRef::new(t.database.clone(), t.schema.clone());
                            let tables = t.content.clone().map(|c| c.split('\n').map(str::to_string).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
                            self.open_tab(Box::new(crate::tabs::er::ErTab::new(conn, schema, tables)));
                        }
                    }
                    opened += 1;
                }
                _ => {}
            }
        }
        if opened > 0 {
            // The welcome tab is not needed when editors were restored.
            let welcome: Vec<u64> = self.dock.iter_all_tabs().filter(|(_, t)| t.view.as_any().is::<crate::tabs::welcome::WelcomeTab>()).map(|(_, t)| t.id).collect();
            for id in welcome {
                if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                    self.dock.remove_tab(path);
                }
            }
        }
    }

    /// Remember the open editors for the next start.
    pub fn save_session(&mut self) {
        let Some(projects) = self.services.projects.as_ref() else { return };
        let mut state = dbiji_config::SessionState::default();
        for (_, t) in self.dock.iter_all_tabs() {
            if let Some(v) = t.view.session_state()
                && let Ok(tab) = serde_json::from_value::<dbiji_config::SessionTab>(v)
            {
                state.tabs.push(tab);
            }
        }
        if let Err(e) = projects.save_session(&state) {
            tracing::warn!("cannot save the session: {e}");
        }
    }

    /// Open a script file in an editor (its saved connection and schema are restored).
    pub fn open_script(&mut self, path: std::path::PathBuf) -> Option<u64> {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.toasts.error(tr("Cannot open the script"), Some(format!("{}: {e}", path.display())));
                return None;
            }
        };
        let meta = self.services.projects.as_ref().and_then(|p| p.default_project().ok()).and_then(|p| p.script_meta(&path).ok().flatten());
        // Scripts without a saved connection use the selected one.
        let conn = meta
            .as_ref()
            .and_then(|m| m.connection_id.clone())
            .filter(|c| self.conns.config(c).is_some())
            .or_else(|| self.nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string));
        let schema = meta.as_ref().and_then(|m| (m.schema.is_some() || m.database.is_some()).then(|| dbiji_core::meta::SchemaRef::new(m.database.clone(), m.schema.clone())));
        let title = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(c) = &conn
            && !self.conns.is_connected(c)
        {
            self.request_connect(c);
        }
        Some(self.open_tab(Box::new(crate::tabs::sql_editor::SqlEditorTab::new(conn, schema, text, Some(path), false, title))))
    }

    /// Open a diagram saved in the project (JSON with connection, schema and diagram).
    pub fn open_saved_diagram(&mut self, path: &std::path::Path) {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let doc = match self.services.projects.as_ref().and_then(|p| p.default_project().ok()).map(|p| p.load_diagram(&name)) {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                self.toasts.error(tr("Cannot open the diagram"), Some(e.to_string()));
                return;
            }
            None => return,
        };
        let conn = doc.get("connection").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_default();
        let schema =
            dbiji_core::meta::SchemaRef::new(doc.get("database").and_then(|v| v.as_str()).map(str::to_string), doc.get("schema").and_then(|v| v.as_str()).map(str::to_string));
        let diagram = doc.get("diagram").map(|d| d.to_string()).and_then(|j| dbiji_visual::er::ErDiagram::from_json(&j).ok());
        match diagram {
            Some(d) => {
                if !conn.is_empty() && !self.conns.is_connected(&conn) {
                    self.request_connect(&conn);
                }
                self.open_tab(Box::new(crate::tabs::er::ErTab::from_saved(conn, schema, name, d)));
            }
            None => self.toasts.error(tr("Cannot open the diagram"), Some(path.display().to_string())),
        }
    }

    /// Finish "Generate SQL" requests whose table definitions arrived.
    fn poll_routines(&mut self) {
        let mut still = Vec::new();
        for (conn, obj, mut job) in std::mem::take(&mut self.pending_routines) {
            match job.poll() {
                Some(Ok(def)) => {
                    let Some(live) = self.conns.live(&conn) else { continue };
                    let mut def = def;
                    def.kind = def.kind.or(Some(obj.kind));
                    let header = format!("-- {} {}\n", tr("Call of"), live.dialect.qualified(def.schema.as_deref(), &def.name));
                    let text = format!("{header}{}\n", dbiji_sql::routine_call_sql(live.dialect.as_ref(), &def));
                    self.commands.push(AppCommand::NewSqlEditor { connection: Some(conn), schema: Some(obj.schema_ref()), text: Some(text) });
                }
                Some(Err(e)) => self.toasts.error(tr("Cannot read the routine"), Some(e.full_text())),
                None => still.push((conn, obj, job)),
            }
        }
        self.pending_routines = still;
    }

    fn poll_generate(&mut self) {
        let mut still = Vec::new();
        for (conn, obj, kind) in std::mem::take(&mut self.pending_generate) {
            let Some(live) = self.conns.live(&conn) else { continue };
            match live.meta.table_defs.get(&obj) {
                Some(crate::backend::Loadable::Loaded(def)) => {
                    let d = live.dialect.as_ref();
                    let o = dbiji_sql::GenerateOptions::default();
                    let text = match kind.as_str() {
                        "select" => dbiji_sql::select_sql(d, def, &o),
                        "insert" => dbiji_sql::insert_sql(d, def, &o),
                        "update" => dbiji_sql::update_sql(d, def, &o),
                        "delete" => dbiji_sql::delete_sql(d, def, &o),
                        "upsert" => dbiji_sql::upsert_sql(d, def, &o).unwrap_or_else(|| format!("-- {}", tr("This database has no UPSERT statement."))),
                        _ => dbiji_sql::ddl_sql(d, def),
                    };
                    self.commands.push(AppCommand::NewSqlEditor { connection: Some(conn), schema: Some(obj.schema_ref()), text: Some(text) });
                }
                Some(crate::backend::Loadable::Failed(e)) => self.toasts.error(tr("Cannot read the table"), Some(e.full_text())),
                _ => still.push((conn, obj, kind)),
            }
        }
        self.pending_generate = still;
    }

    /// Apply edited preferences (theme, language, limits, keymap) and save them.
    fn apply_settings(&mut self, ctx: &egui::Context) {
        crate::theme::apply_settings(ctx, &self.settings);
        dbiji_core::i18n::set_locale(self.settings.general.effective_locale());
        self.settings.results.apply_memory_limits();
        let (keymap, _) = crate::keys::Keymap::from_config(&self.settings.keymap, crate::keys::IS_MAC);
        self.keymap = keymap;
        if let Some(h) = &self.services.history {
            let mut h = h.lock();
            h.set_redaction(self.settings.history.redact_secrets);
            let _ = h.set_retention(Some(self.settings.history.retention()));
        }
        self.save_settings();
    }

    /// Use `theme` from now on (and save it).
    pub fn set_theme(&mut self, ctx: &egui::Context, theme: dbiji_config::settings::Theme) {
        self.settings.general.theme = theme;
        crate::theme::apply_settings(ctx, &self.settings);
        self.save_settings();
    }

    /// Switch between the light and the dark theme.
    pub fn toggle_theme(&mut self, ctx: &egui::Context) {
        use dbiji_config::settings::Theme;
        let next = if ctx.theme() == egui::Theme::Dark { Theme::Light } else { Theme::Dark };
        self.set_theme(ctx, next);
    }

    /// Show the next tip.
    pub fn show_tip(&mut self) {
        let n = crate::tips::TIPS.len();
        let index = self.settings.general.next_tip % n;
        self.settings.general.next_tip = (index + 1) % n;
        self.save_settings();
        self.tip = Some(crate::tips::TipWindow { index });
    }

    /// Write the diagnostic zip (to `out`, by default into Downloads).
    pub fn collect_diagnostics(&mut self, out: Option<std::path::PathBuf>) {
        if self.diagnostics.is_some() {
            return;
        }
        let out = out.unwrap_or_else(crate::diagnostics::default_output);
        let paths = self.services.paths.clone();
        let settings = self.settings.clone();
        let conns = self.conns.configs().to_vec();
        self.diagnostics = Some(crate::tabs::dbtools::spawn_blocking(&self.backend, move || crate::diagnostics::collect(&paths, &settings, &conns, &out)));
    }

    /// The diagnostic zip is being written.
    pub fn collecting_diagnostics(&self) -> bool {
        self.diagnostics.is_some()
    }

    /// Write `settings.json` / `keymap.json`.
    pub fn save_settings(&mut self) {
        if let Err(e) = dbiji_config::settings::save(&self.services.paths, &self.settings) {
            self.toasts.error(tr("Cannot save the settings"), Some(e.to_string()));
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(tr("File"), |ui| {
                if ui.button(format!("{} {}", icon::FILE_SQL, tr("New SQL editor"))).clicked() {
                    self.commands.push(AppCommand::NewSqlEditor { connection: None, schema: None, text: None });
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Open script…"))).clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).pick_file() {
                        self.commands.push(AppCommand::OpenScript(path));
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} {}", icon::GEAR, tr("Settings…"))).clicked() {
                    self.commands.push(AppCommand::OpenSettings);
                    ui.close();
                }
                ui.separator();
                if ui.button(tr("Quit")).clicked() {
                    self.commands.push(AppCommand::Quit);
                    ui.close();
                }
            });
            ui.menu_button(tr("Database"), |ui| {
                if ui.button(format!("{} {}", icon::PLUS, tr("New connection…"))).clicked() {
                    self.commands.push(AppCommand::NewConnection { kind: None, folder: None });
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::DOWNLOAD_SIMPLE, tr("Import connections…"))).clicked() {
                    self.commands.push(AppCommand::ImportConnections);
                    ui.close();
                }
                ui.separator();
                let conn = self.selected_connection().filter(|c| self.conns.is_connected(c));
                // What the selected server can create.
                let (db_ok, schema_ok, file_based) = conn.as_deref().and_then(|c| self.conns.live(c)).map_or((false, false, false), |l| {
                    let probe = dbiji_core::NewDatabase { name: "x".into(), ..Default::default() };
                    (l.dialect.create_database_sql(&probe).is_some(), l.dialect.create_schema_sql("x", None).is_some(), l.config.kind.is_file_based())
                });
                let why = if conn.is_none() {
                    tr("Select a connected connection in the navigator first.")
                } else if file_based {
                    tr("A database of this type is a file: use New connection and choose a new file.")
                } else {
                    tr("Not supported by this database type.")
                };
                let resp = ui.add_enabled(db_ok, egui::Button::new(format!("{} {}", icon::DATABASE, tr("New database…")))).on_disabled_hover_text(why);
                if resp.clicked()
                    && let Some(c) = conn.clone()
                {
                    self.commands.push(AppCommand::NewDatabase { connection: c });
                    ui.close();
                }
                if ui.add_enabled(schema_ok, egui::Button::new(format!("{} {}", icon::STACK, tr("New schema…")))).on_disabled_hover_text(why).clicked()
                    && let Some(c) = conn
                {
                    let database = self.nav.selected_schema().and_then(|s| s.database);
                    self.commands.push(AppCommand::NewSchema { connection: c, database });
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} {}", icon::PLUGS, tr("Disconnect all"))).clicked() {
                    self.commands.push(AppCommand::DisconnectAll);
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::QUEUE, tr("Pending transactions…"))).clicked() {
                    self.pending_tx.open = true;
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} {}", icon::GIT_DIFF, tr("Compare schemas…"))).clicked() {
                    self.commands.push(AppCommand::OpenSchemaCompare { connection: None, schema: None });
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::ARROWS_LEFT_RIGHT, tr("Compare data…"))).clicked() {
                    self.commands.push(AppCommand::OpenDataCompare { connection: None, table: None });
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Search…"))).clicked() {
                    self.commands.push(AppCommand::OpenSearch { connection: None, data: false });
                    ui.close();
                }
            });
            ui.menu_button(tr("Tools"), |ui| {
                if ui.button(format!("{} {}", icon::CLOCK_COUNTER_CLOCKWISE, tr("Query history"))).clicked() {
                    self.commands.push(AppCommand::OpenHistory);
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::LIST_CHECKS, tr("Tasks"))).clicked() {
                    self.commands.push(AppCommand::OpenTasks);
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::SPARKLE, tr("AI assistant"))).clicked() {
                    let conn = self.nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string);
                    self.commands.push(AppCommand::OpenAiAssistant { connection: conn });
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::HOURGLASS, tr("Background operations"))).clicked() {
                    self.commands.push(AppCommand::ToggleOperations);
                    ui.close();
                }
            });
            ui.menu_button(tr("View"), |ui| {
                if ui.button(tr("Toggle navigator")).clicked() {
                    self.commands.push(AppCommand::ToggleNavigator);
                    ui.close();
                }
                let mut props = self.settings.navigator.show_properties;
                if ui.checkbox(&mut props, tr("Properties panel")).clicked() {
                    self.settings.navigator.show_properties = props;
                    self.save_settings();
                    ui.close();
                }
                ui.menu_button(format!("{} {}", icon::CIRCLE_HALF, tr("Theme")), |ui| {
                    use dbiji_config::settings::Theme;
                    for (t, label) in [(Theme::System, tr("System")), (Theme::Light, tr("Light")), (Theme::Dark, tr("Dark"))] {
                        if ui.radio(self.settings.general.theme == t, label).clicked() {
                            self.set_theme(ui.ctx(), t);
                            ui.close();
                        }
                    }
                });
                if ui.button(tr("Command palette")).clicked() {
                    self.commands.push(AppCommand::ShowPalette);
                    ui.close();
                }
            });
            ui.menu_button(tr("Help"), |ui| {
                if ui.button(tr("Keyboard shortcuts")).clicked() {
                    self.commands.push(AppCommand::ShowShortcuts);
                    ui.close();
                }
                if ui.button(tr("Database drivers")).clicked() {
                    self.dialogs.drivers_open = true;
                    ui.close();
                }
                if ui.button(tr("Create sample database")).clicked() {
                    self.commands.push(AppCommand::CreateSampleDatabase);
                    ui.close();
                }
                if ui.button(format!("{} {}", icon::LIGHTBULB, tr("Tip of the day"))).clicked() {
                    self.show_tip();
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} {}", icon::WARNING_OCTAGON, tr("Error log"))).clicked() {
                    self.open_tab(Box::new(crate::tabs::errorlog::ErrorLogTab::new()));
                    ui.close();
                }
                if ui.add_enabled(self.diagnostics.is_none(), egui::Button::new(format!("{} {}", icon::FILE_ZIP, tr("Collect diagnostic info")))).clicked() {
                    self.collect_diagnostics(None);
                    ui.close();
                }
                ui.separator();
                if ui.button(format!("{} {}", icon::SPARKLE, tr("What's new"))).clicked() {
                    self.updates.show_whats_new();
                    ui.close();
                }
                if ui.add_enabled(!self.updates.checking(), egui::Button::new(format!("{} {}", icon::ARROW_CIRCLE_UP, tr("Check for updates…")))).clicked() {
                    self.commands.push(AppCommand::CheckForUpdates);
                    ui.close();
                }
                if ui.button(tr("About DBiji")).clicked() {
                    self.commands.push(AppCommand::ShowAbout);
                    ui.close();
                }
            });
        });
    }

    /// Whether a toolbar action can run now, with the reason when not.
    fn toolbar_action_state(&self, action: crate::keys::Action) -> Result<(), &'static str> {
        use crate::keys::Action as A;
        let tab = self.active_tab.and_then(|id| self.dock.iter_all_tabs().find(|(_, t)| t.id == id)).map(|(_, t)| t);
        if let Some(enabled) = tab.and_then(|t| t.view.action_enabled(action)) {
            return if enabled {
                Ok(())
            } else {
                match action {
                    A::CancelQuery => Err(tr("No query is running.")),
                    A::Commit | A::Rollback => Err(tr("Auto-commit is on: every statement is committed already.")),
                    _ => Err(tr("Not available right now.")),
                }
            };
        }
        let editor_only = matches!(action, A::CancelQuery | A::Commit | A::Rollback | A::ToggleAutoCommit | A::SelectConnection | A::SelectSchema)
            || action.context() == crate::keys::ActionContext::SqlEditor;
        if editor_only {
            return Err(tr("Available in a SQL editor: open one with New SQL editor."));
        }
        let needs_connection =
            matches!(action, A::EditConnection | A::Connect | A::Disconnect | A::Reconnect | A::OpenSqlConsole | A::OpenSessionManager | A::NewDatabase | A::Export | A::Import);
        if needs_connection && self.selected_connection().is_none() {
            return Err(tr("Select a connection in the navigator first."));
        }
        Ok(())
    }

    /// Main toolbar (customisable in the settings).
    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let hidden = &self.settings.toolbar.hidden;
            for id in &self.settings.toolbar.items {
                if hidden.contains(id) {
                    continue;
                }
                if id == "|" {
                    ui.separator();
                    continue;
                }
                let Some(action) = crate::keys::Action::from_id(id) else { continue };
                let keys = crate::keys::label(&self.keymap, action);
                let tip = if keys.is_empty() { crate::keys::action_label(action).to_string() } else { format!("{}  ({keys})", crate::keys::action_label(action)) };
                let state = self.toolbar_action_state(action);
                let mut text = RichText::new(crate::keys::action_icon(action)).size(16.0);
                if let Some(c) = crate::keys::action_color(action, ui.visuals().dark_mode).filter(|_| state.is_ok()) {
                    text = text.color(c);
                }
                let b = egui::Button::new(text).frame(false).min_size(egui::vec2(26.0, 22.0));
                let resp = ui.add_enabled(state.is_ok(), b).on_hover_text(&tip);
                let resp = match state {
                    Err(why) => resp.on_disabled_hover_text(format!("{tip}\n{why}")),
                    Ok(()) => resp,
                };
                if resp.clicked() {
                    self.commands.push(AppCommand::Action(action));
                }
            }
            // An update to install.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.updates.toolbar_button(ui));
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let active = self.dock.find_active_focused().map(|(_, t)| (t.view.connection_id().map(str::to_string), t.view.title()));
            match active.as_ref().and_then(|(c, _)| c.clone()) {
                Some(conn) => {
                    let name = self.conns.config(&conn).map(|c| c.name.clone()).unwrap_or_default();
                    let ty = self.conns.type_of(&conn);
                    if let Some(color) = crate::theme::connection_marker(ty.color) {
                        ui.label(RichText::new("■").color(color)).on_hover_text(&ty.name);
                    }
                    ui.label(format!("{} {}", icon::DATABASE, name));
                    if let Some(live) = self.conns.live(&conn) {
                        if let Some(db) = &live.current_database {
                            ui.separator();
                            ui.label(db);
                        }
                        if let Some(s) = &live.current_schema {
                            ui.label(format!("/ {s}"));
                        }
                        ui.separator();
                        // `version()` texts repeat the product ("PostgreSQL 16.4 on …"): show the number.
                        let number = live.server.version.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())).unwrap_or("");
                        ui.weak(format!("{} {number}", live.server.product));
                    } else {
                        ui.weak(tr("not connected"));
                    }
                }
                None => {
                    ui.weak(tr("No active connection"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = self.ops.running();
                let label = if running > 0 { format!("{} {running}", icon::HOURGLASS) } else { icon::HOURGLASS.to_string() };
                if ui.small_button(label).on_hover_text(tr("Background operations")).clicked() {
                    self.ops.panel_open = !self.ops.panel_open;
                }
                let dark = ui.visuals().dark_mode;
                let (theme_icon, tip) = if dark { (icon::SUN, tr("Switch to the light theme")) } else { (icon::MOON, tr("Switch to the dark theme")) };
                if ui.small_button(theme_icon).on_hover_text(tip).clicked() {
                    self.toggle_theme(ui.ctx());
                }
                let mem = dbiji_core::rowstore::global_resident_bytes();
                ui.weak(format!("{} {:.0} MB", icon::MEMORY, mem as f64 / 1_048_576.0)).on_hover_text(tr("Memory used by result sets"));
            });
        });
    }
}

impl eframe::App for DbijiApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            self.first_frame = false;
            crate::bench::report_first_frame(self.started);
            for w in std::mem::take(&mut self.services.warnings) {
                self.toasts.warning(tr("Configuration problem"), Some(w));
            }
            if self.services.secrets.read().is_locked() {
                self.dialogs.vault_open = true;
            }
            if !self.exit_after_first_frame && self.exit_at.is_none() && self.updates.start(&self.backend, &mut self.settings, self.existing_install) {
                self.save_settings();
            }
            // A tip on start, not on the first start and not over update notes.
            let popup = self.updates.whats_new.is_some() || self.updates.ask || self.updates.available.is_some();
            if self.existing_install && self.settings.general.show_tips && !popup && !self.exit_after_first_frame && self.exit_at.is_none() {
                self.show_tip();
            }
            if self.exit_after_first_frame {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if let Some(t) = self.exit_at {
            if Instant::now() >= t {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(t - Instant::now());
            }
        }
        self.conns.poll(&self.backend);
        if self.bench_workload.is_some() {
            self.poll_bench_workload();
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if !self.pending_generate.is_empty() {
            self.poll_generate();
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if !self.pending_routines.is_empty() {
            self.poll_routines();
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.settings.navigator.expand_on_connect {
            for id in self.conns.just_connected.clone() {
                self.nav.expand(crate::nav::NavNode::Connection(id.clone()));
            }
        }
        for (id, err) in self.conns.errors() {
            let name = self.conns.config(&id).map(|c| c.name.clone()).unwrap_or_default();
            self.toasts.error(dbiji_core::trf("Cannot connect to {}", &[&name]), Some(err));
            self.conns.take_error(&id);
        }
        self.shortcuts(&ctx);
        // Unsaved changes: ask before quitting.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_confirmed && self.exit_at.is_none() && !self.exit_after_first_frame {
            let dirty = self.dock.iter_all_tabs().any(|(_, t)| t.view.is_dirty()) || !crate::pending_tx::collect(&self.dock, &self.conns).is_empty();
            if dirty && self.settings.general.confirm_exit_with_unsaved {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_quit = true;
            }
        }
        if self.confirm_quit {
            let dirty: Vec<String> = self.dock.iter_all_tabs().filter(|(_, t)| t.view.is_dirty()).map(|(_, t)| t.view.title()).collect();
            let pending = crate::pending_tx::collect(&self.dock, &self.conns);
            let mut decided: Option<bool> = None;
            let mut review = false;
            let modal = egui::Modal::new(egui::Id::new("confirm_quit")).show(&ctx, |ui| {
                ui.heading(tr("Unsaved changes"));
                if !dirty.is_empty() {
                    ui.label(tr("These tabs have unsaved changes:"));
                    for t in dirty.iter().take(10) {
                        ui.label(format!("• {t}"));
                    }
                }
                if !pending.is_empty() {
                    ui.label(tr("These connections have uncommitted transactions; quitting rolls them back:"));
                    for p in pending.iter().take(10) {
                        ui.label(format!("• {} — {}", p.conn_name, p.owner));
                    }
                }
                ui.horizontal(|ui| {
                    if !pending.is_empty() && ui.button(tr("Review transactions")).clicked() {
                        review = true;
                    }
                    if ui.button(RichText::new(tr("Quit without saving")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        decided = Some(false);
                    }
                });
            });
            if review {
                self.pending_tx.open = true;
                decided = Some(false);
            }
            match decided {
                Some(true) => {
                    self.confirm_quit = false;
                    self.quit_confirmed = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(false) => self.confirm_quit = false,
                None if modal.should_close() => self.confirm_quit = false,
                None => {}
            }
        }
        // Files dropped on the window: scripts open in editors, database files as connections.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            self.open_path(&path, false);
        }

        egui::Panel::top("dbiji_menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("dbiji_toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("dbiji_status").exact_size(24.0).show(ui, |ui| self.status_bar(ui));

        let mut cx = AppContext {
            backend: &self.backend,
            conns: &mut self.conns,
            services: &self.services,
            settings: &mut self.settings,
            commands: &mut self.commands,
            toasts: &mut self.toasts,
            ops: &mut self.ops,
            keymap: &self.keymap,
            tab_active: false,
            current_tab: None,
        };
        if self.show_nav {
            egui::Panel::left("dbiji_navigator").resizable(true).default_size(280.0).size_range(160.0..=600.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.show_project, format!("{} {}", icon::DATABASE, tr("Database"))).clicked() {
                        self.show_project = false;
                    }
                    if ui.selectable_label(self.show_project, format!("{} {}", icon::FOLDER_SIMPLE, tr("Project"))).clicked() {
                        self.show_project = true;
                    }
                });
                ui.separator();
                if self.show_project {
                    self.project.ui(ui, &mut cx);
                } else {
                    self.nav.ui(ui, &mut cx);
                }
            });
        }
        self.active_tab = self.dock.find_active_focused().map(|(_, t)| t.id);
        if cx.settings.navigator.link_with_editor && self.active_tab != self.linked_tab {
            self.linked_tab = self.active_tab;
            if let Some((conn, obj)) = self.dock.find_active_focused().and_then(|(_, t)| t.view.navigator_object()) {
                self.nav.reveal(&conn, &obj);
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| {
            let mut viewer = DockViewer { cx: &mut cx, close_vetoed: Vec::new(), active_tab: self.active_tab };
            DockArea::new(&mut self.dock).style(egui_dock::Style::from_egui(ui.style().as_ref())).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
        });

        self.dialogs.show(&ctx, &mut cx);
        if self.ops.panel_open {
            let mut open = true;
            egui::Window::new(format!("{} {}", icon::HOURGLASS, tr("Background operations"))).open(&mut open).default_width(360.0).show(&ctx, |ui| self.ops.ui(ui));
            self.ops.panel_open = open;
        }
        if self.updates.ui(&ctx, &self.backend, &mut self.settings, &mut self.toasts) {
            self.save_settings();
        }
        // The update was handed to the installer: quit (unsaved scripts are asked about first).
        if std::mem::take(&mut self.updates.restart) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(tip) = &mut self.tip {
            let mut show = self.settings.general.show_tips;
            let open = tip.show(&ctx, &mut show);
            if !open {
                self.tip = None;
            }
            if show != self.settings.general.show_tips {
                self.settings.general.show_tips = show;
                self.save_settings();
            }
        }
        self.pending_tx.poll(&mut self.toasts);
        if self.pending_tx.open {
            let items = crate::pending_tx::collect(&self.dock, &self.conns);
            self.pending_tx.show(&ctx, &self.backend, &items);
        }
        if let Some(job) = &mut self.diagnostics {
            match job.poll() {
                Some(Ok(path)) => {
                    self.diagnostics = None;
                    self.toasts.success(dbiji_core::trf("Diagnostic info saved to {}", &[&path.display()]));
                }
                Some(Err(e)) => {
                    self.diagnostics = None;
                    self.toasts.error(tr("Cannot collect the diagnostic info"), Some(e.to_string()));
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        self.toasts.show(&ctx);

        // Credentials entered → connect.
        for (id, creds) in self.dialogs.take_credentials() {
            if let Err(e) = self.conns.connect(&self.backend, &id, creds) {
                self.toasts.error(tr("Cannot connect"), Some(e.to_string()));
            }
        }
        if self.dock.iter_all_tabs().next().is_none() {
            self.open_tab(Box::new(crate::tabs::welcome::WelcomeTab));
        }
        self.ensure_focus();
        self.process_commands(&ctx);
        if self.conns.dirty {
            self.conns.dirty = false;
            if let Err(e) = self.conns.store.save(&self.services.paths) {
                self.toasts.error(tr("Cannot save the connections"), Some(e.to_string()));
            }
        }
        if self.conns.is_connecting() || self.ops.running() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_session();
        if let Some(s) = self.services.scheduler.clone() {
            self.backend.block_on(async move { s.shutdown(std::time::Duration::from_secs(3)).await });
        }
        self.ops.cancel_all();
        self.conns.disconnect_all(&self.backend);
    }
}
