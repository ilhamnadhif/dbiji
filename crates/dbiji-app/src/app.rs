//! Main window: menu bar, toolbar, navigator, dock area, status bar, dialogs.

use crate::backend::Backend;
use crate::command::AppCommand;
use crate::conn::{ConnectionManager, Credentials};
use crate::cx::AppContext;
use crate::dialogs::{Dialogs, FolderAction};
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

/// SQLite database file (by extension).
pub fn database_file_kind(path: &std::path::Path) -> Option<dbiji_core::DbKind> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "db" | "sqlite" | "sqlite3" | "db3" => Some(dbiji_core::DbKind::Sqlite),
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
    /// The AI assistant, a panel at the right of the tabs (its conversation
    /// stays while the panel is hidden).
    pub ai_panel: Option<crate::tabs::ai::AiAssistantTab>,
    pub ai_open: bool,
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
    /// Title last given to the window.
    window_title: String,
    /// Benchmark mode: quit after this instant.
    exit_at: Option<Instant>,
    /// The window was asked to close while tabs had unsaved changes.
    confirm_quit: bool,
    /// Connections to disconnect (and reconnect) once their uncommitted transactions are confirmed.
    confirm_disconnect: Option<(Vec<String>, bool)>,
    /// Dirty tabs waiting for "discard and close?" (the first is asked).
    close_confirm: Vec<u64>,
    quit_confirmed: bool,
    /// "Generate SQL" requests waiting for the table definition.
    pending_generate: Vec<(String, dbiji_core::ObjectRef, String)>,
    /// Tab last revealed in the navigator (Link with editor).
    linked_tab: Option<u64>,
    /// "Execute…" of routines waiting for their definition.
    /// Routines to call; the job starts once the session of the routine's database is open.
    pending_routines: Vec<(String, dbiji_core::ObjectRef, Option<crate::backend::Job<dbiji_core::DbResult<dbiji_core::ddl::RoutineDef>>>)>,
    script_counter: u32,
    /// `DBIJI_BENCH_SCENARIO=workload`: connections still to open editors for.
    bench_workload: Option<Vec<String>>,
    /// "What's new" and update checks.
    pub updates: crate::updates::Updates,
    /// Wizards and actions shown in pop-up windows instead of tabs.
    pub popups: Vec<Tab>,
    /// Bring this pop-up to the front (opened again while open).
    raise_popup: Option<u64>,
    /// The pop-up last clicked in (or just opened): shortcuts are its own.
    popup_focus: Option<u64>,
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
        let mut conns = conns;
        conns.time_zone = settings.general.time_zone;
        let mut app = DbijiApp {
            backend,
            conns,
            services,
            settings,
            keymap,
            dock: DockState::new(Vec::new()),
            next_tab_id: 1,
            nav: Navigator::default(),
            ai_panel: None,
            ai_open: false,
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
            window_title: "DBiji".into(),
            exit_at: std::env::var("DBIJI_BENCH_EXIT_AFTER_SECS").ok().and_then(|v| v.parse::<u64>().ok()).map(|s| Instant::now() + std::time::Duration::from_secs(s)),
            pending_generate: Vec::new(),
            pending_routines: Vec::new(),
            linked_tab: None,
            script_counter: 0,
            confirm_quit: false,
            confirm_disconnect: None,
            close_confirm: Vec::new(),
            quit_confirmed: false,
            bench_workload: None,
            updates: Default::default(),
            popups: Vec::new(),
            raise_popup: None,
            popup_focus: None,
            #[cfg(target_os = "macos")]
            app_menu: None,
            existing_install: false,
            tip: None,
            pending_tx: Default::default(),
            diagnostics: None,
        };
        app.existing_install = crate::updates::is_existing_install(&app.services.paths);
        app.updates.data_dir = Some(app.services.paths.data_dir().to_path_buf());
        app
    }

    /// Queue a command for the next frame (as a menu item would).
    pub fn push_command(&mut self, cmd: AppCommand) {
        self.commands.push(cmd);
    }

    /// A new SQL editor with `text`, run once connected when `run`.
    fn new_sql_editor(&mut self, connection: Option<String>, schema: Option<dbiji_core::meta::SchemaRef>, text: String, run: bool) {
        self.script_counter += 1;
        let title = format!("Script-{}", self.script_counter);
        let mut tab = crate::tabs::sql_editor::SqlEditorTab::new(connection.clone(), schema, text, None, false, title);
        tab.run_on_open = run;
        if let Some(c) = &connection
            && !self.conns.is_connected(c)
        {
            self.request_connect(c);
        }
        self.open_tab(Box::new(tab));
    }

    /// Open a tab, or focus an existing tab with the same dedupe key.
    pub fn open_tab(&mut self, view: Box<dyn TabView>) -> u64 {
        if view.popup_size().is_some() {
            if let Some(key) = view.dedupe_key()
                && let Some(t) = self.popups.iter().find(|t| t.view.dedupe_key().as_deref() == Some(key.as_str()))
            {
                self.raise_popup = Some(t.id);
                self.popup_focus = Some(t.id);
                return t.id;
            }
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.popups.push(Tab::new(id, view));
            self.popup_focus = Some(id);
            return id;
        }
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
    pub(crate) fn focus_tab_path(&mut self, path: egui_dock::TabPath) {
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

    /// Close a tab or pop-up (it may veto, e.g. to ask about unsaved changes).
    /// A vetoing tab stays where it is.
    pub fn close_tab(&mut self, id: u64) {
        let popup = self.popups.iter().position(|t| t.id == id);
        let tab = match popup {
            Some(i) => self.popups.get_mut(i),
            None => self.dock.iter_all_tabs_mut().find(|(_, t)| t.id == id).map(|(_, t)| t),
        };
        let Some(tab) = tab else { return };
        if tab.needs_close_confirm() || tab.close_rolls_back(&self.conns) {
            if !self.close_confirm.contains(&id) {
                self.close_confirm.push(id);
            }
            return;
        }
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
                popup: false,
            };
            if !tab.view.on_close(&mut cx) {
                return;
            }
        }
        match popup {
            Some(i) => {
                self.popups.remove(i);
            }
            None => {
                if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
                    self.dock.remove_tab(path);
                }
            }
        }
    }

    /// "Discard the unsaved changes of … and close?" for the first waiting tab.
    fn close_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(&id) = self.close_confirm.first() else { return };
        let tab = self.popups.iter().chain(self.dock.iter_all_tabs().map(|(_, t)| t)).find(|t| t.id == id);
        let Some((title, dirty)) = tab.map(|t| (t.view.title(), t.view.is_dirty())) else {
            self.close_confirm.remove(0);
            return;
        };
        let mut decided: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new(("confirm_close", id))).show(ctx, |ui| {
            // Pending edits, or edits saved inside a transaction that closing rolls back.
            let (heading, text, button) = if dirty {
                (tr("Unsaved changes"), dbiji_core::trf("\"{}\" has unsaved changes; closing it discards them.", &[&title]), tr("Discard and close"))
            } else {
                (tr("Uncommitted transaction"), dbiji_core::trf("\"{}\" has uncommitted changes; closing it rolls them back.", &[&title]), tr("Roll back and close"))
            };
            ui.heading(heading);
            ui.label(text);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(button).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    decided = Some(true);
                }
                if ui.button(tr("Cancel")).clicked() {
                    decided = Some(false);
                }
            });
        });
        if decided.is_none() && modal.should_close() {
            decided = Some(false);
        }
        if let Some(close) = decided {
            self.close_confirm.remove(0);
            if close {
                if let Some(t) = self.popups.iter_mut().chain(self.dock.iter_all_tabs_mut().map(|(_, t)| t)).find(|t| t.id == id) {
                    t.force_close = true;
                }
                self.close_tab(id);
            }
        }
    }

    /// Disconnect (and reconnect), asking first when a session of the connection has
    /// uncommitted changes: closing it rolls them back.
    fn disconnect_checked(&mut self, ids: Vec<String>, reconnect: bool) {
        let pending = crate::pending_tx::collect(&self.dock, &self.conns);
        let (ask, now): (Vec<String>, Vec<String>) = ids.into_iter().partition(|id| pending.iter().any(|p| &p.conn == id));
        for id in now {
            self.finish_disconnect(&id, reconnect);
        }
        if !ask.is_empty() {
            let c = self.confirm_disconnect.get_or_insert_with(|| (Vec::new(), reconnect));
            for id in ask {
                if !c.0.contains(&id) {
                    c.0.push(id);
                }
            }
        }
    }

    fn finish_disconnect(&mut self, id: &str, reconnect: bool) {
        self.conns.disconnect(&self.backend, id);
        if reconnect {
            self.request_connect(id);
        }
    }

    fn confirm_disconnect_ui(&mut self, ctx: &egui::Context) {
        let Some((ids, reconnect)) = self.confirm_disconnect.clone() else { return };
        let pending: Vec<_> = crate::pending_tx::collect(&self.dock, &self.conns).into_iter().filter(|p| ids.contains(&p.conn)).collect();
        let mut decided: Option<bool> = None;
        let mut review = false;
        if pending.is_empty() {
            // Committed or rolled back meanwhile (Review transactions): nothing is lost any more.
            decided = Some(true);
        } else {
            let modal = egui::Modal::new(egui::Id::new("confirm_disconnect")).show(ctx, |ui| {
                ui.heading(tr("Uncommitted transactions"));
                ui.label(tr("Disconnecting rolls back the uncommitted changes of:"));
                for p in pending.iter().take(10) {
                    ui.label(format!("• {} — {}", p.conn_name, p.owner));
                }
                ui.horizontal(|ui| {
                    if ui.button(tr("Review transactions")).clicked() {
                        review = true;
                    }
                    let label = if reconnect { tr("Roll back and reconnect") } else { tr("Roll back and disconnect") };
                    if ui.button(RichText::new(label).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        decided = Some(false);
                    }
                });
            });
            if modal.should_close() && decided.is_none() && !review {
                decided = Some(false);
            }
        }
        // The review window cannot be used under the modal: the question closes (disconnect again after).
        if review {
            self.pending_tx.open = true;
            decided = Some(false);
        }
        match decided {
            Some(true) => {
                self.confirm_disconnect = None;
                for id in &ids {
                    self.finish_disconnect(id, reconnect);
                }
            }
            Some(false) => self.confirm_disconnect = None,
            None => {}
        }
    }

    /// Request connecting, prompting for credentials when needed.
    pub(crate) fn request_connect(&mut self, id: &str) {
        // Encrypted vault locked (auto-lock): ask for the master password first, unless the
        // user skipped it: then the passwords are asked when connecting.
        if self.conns.secrets.is_locked() && !self.dialogs.vault.skipped {
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
        {
            let actions = self.app_menu.as_mut().map(|m| m.actions()).unwrap_or_default();
            for a in actions {
                self.run_menu_action(ctx, a);
            }
            let state = self.menu_state();
            if let Some(menu) = &mut self.app_menu {
                menu.sync(&state);
            }
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
            AppCommand::Disconnect(id) => self.disconnect_checked(vec![id], false),
            AppCommand::Reconnect(id) => self.disconnect_checked(vec![id], true),
            AppCommand::DisconnectAll => self.disconnect_checked(self.conns.live_ids(), false),
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
            AppCommand::NewFolder { parent } => self.dialogs.folder_dialog(FolderAction::New { parent }),
            AppCommand::RenameFolder(path) => self.dialogs.folder_dialog(FolderAction::Rename(path)),
            AppCommand::DeleteFolder(path) => self.dialogs.folder_dialog(FolderAction::Delete(path)),
            AppCommand::MoveToFolder { id, folder } => {
                if let Some(c) = self.conns.config_mut(&id) {
                    c.folder = folder;
                }
            }
            AppCommand::ShowPalette => {
                crate::palette::reset(ctx);
                self.dialogs.palette_open = true;
            }
            AppCommand::Action(a) => self.dispatch_action(ctx, a),
            AppCommand::NewSqlEditor { connection, schema, text } => self.new_sql_editor(connection, schema, text.unwrap_or_default(), false),
            AppCommand::RunSqlInEditor { connection, schema, text } => self.new_sql_editor(connection, schema, text, true),
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
                self.pending_routines.push((connection, routine, None));
            }
            AppCommand::GenerateSql { connection, object, kind } => {
                // A view has rows but no table definition: SELECT * from it.
                if kind == "select" && matches!(object.kind, dbiji_core::meta::ObjectKind::View | dbiji_core::meta::ObjectKind::MaterializedView) {
                    if let Some(live) = self.conns.live(&connection) {
                        let text = format!("{};\n", live.dialect.select_table(&object, None, None, None, None));
                        self.commands.push(AppCommand::NewSqlEditor { connection: Some(connection), schema: Some(object.schema_ref()), text: Some(text) });
                    }
                    return;
                }
                if object.kind.is_table_like() {
                    // The definition is read by poll_generate in the object's own database.
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
            AppCommand::OpenMaintenance { connection, objects, op } => {
                self.open_tab(Box::new(crate::tabs::dbtools::MaintenanceTab::new(connection, objects).with_op(op)));
            }
            AppCommand::ChangePassword { connection } => {
                self.open_tab(Box::new(crate::tabs::dbtools::ChangePasswordTab::new(connection)));
            }
            AppCommand::EditDatabase { connection, database, rename } => {
                if self.conns.is_connected(&connection) {
                    self.dialogs.alter_db = Some(crate::dialogs::alter_db::AlterDatabaseDialog::new(connection, database, rename));
                }
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
            // The toolbar button, the menu and the shortcut show or hide the panel.
            AppCommand::OpenAiAssistant { connection } => match self.ai_panel.as_mut() {
                Some(_) if self.ai_open => self.ai_open = false,
                Some(p) => {
                    p.follow(connection);
                    self.ai_open = true;
                }
                None => {
                    self.ai_panel = Some(crate::tabs::ai::AiAssistantTab::new(connection));
                    self.ai_open = true;
                }
            },
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
                        // A database without a schema: all of it.
                        (None, Some(s)) if s.schema.is_none() => live.meta.clear_database(s.database.as_deref(), live.current_database.as_deref()),
                        (None, Some(s)) => live.meta.clear_schema(&s),
                        (None, None) => {
                            live.meta.clear();
                            live.retry_failed_sessions();
                        }
                    }
                }
            }
            other => crate::router::route(self, ctx, other),
        }
    }

    /// Keyboard shortcuts of the focused context (active tab) and global ones.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        // Modal questions take the keyboard.
        if self.dialogs.captures_keyboard() || self.confirm_quit || self.confirm_disconnect.is_some() || !self.close_confirm.is_empty() {
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

    /// Schema of the ER diagram: the navigator's selection, else the active tab's schema,
    /// else the connection's current one (a connection node selected, a single-schema database).
    fn er_schema(&self, conn: Option<&str>) -> Option<dbiji_core::meta::SchemaRef> {
        let conn = conn?;
        let usable = |s: &dbiji_core::meta::SchemaRef| s.schema.is_some() || s.database.is_none();
        if let Some(s) = self.nav.selected_schema().filter(usable) {
            return Some(s);
        }
        let active = self.active_tab?;
        let tab = self.dock.iter_all_tabs().find(|(_, t)| t.id == active).map(|(_, t)| t);
        if let Some(s) = tab.filter(|t| t.view.connection_id() == Some(conn)).and_then(|t| t.view.schema_context()).filter(|s| s.schema.is_some()) {
            return Some(s);
        }
        let live = self.conns.live(conn)?;
        live.current_schema.clone().or_else(|| live.current_database.clone()).map(|s| dbiji_core::meta::SchemaRef::new(None, Some(s)))
    }

    /// Run an action: the active tab gets the first chance, then the global handler.
    pub fn dispatch_action(&mut self, ctx: &egui::Context, action: crate::keys::Action) {
        use crate::keys::Action as A;
        // Working in the navigator: Refresh reloads its selection, not the active tab.
        if action == A::Refresh
            && self.show_nav
            && self.nav.has_focus
            && let Some(node) = self.nav.selected.clone()
        {
            self.nav.refresh_node(&node, &mut self.commands);
            return;
        }
        // Working in a pop-up: Close closes it, and the tab behind gets no shortcut.
        let in_popup = self.popup_focus.filter(|id| self.popups.iter().any(|t| t.id == *id));
        if let Some(id) = in_popup
            && action == A::CloseTab
        {
            self.close_tab(id);
            return;
        }
        let handled = match self.dock.find_active_focused().filter(|_| in_popup.is_none()) {
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
                    popup: false,
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
            A::NewSqlEditor => {
                // On the navigator's selection: its schema (or database) too.
                let nav_conn = self.nav.selected.as_ref().and_then(|n| n.connection());
                let schema = if nav_conn.is_some() && nav_conn == conn.as_deref() { self.nav.selected_schema() } else { None };
                Some(AppCommand::NewSqlEditor { connection: conn, schema, text: None })
            }
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
            A::OpenErDiagram => match (conn.clone(), self.er_schema(conn.as_deref())) {
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
        // Its pane gets the focus too: the next Ctrl+Tab continues from it (splits).
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            self.focus_tab_path(path);
            self.active_tab = Some(id);
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
    /// SQLite database files become (or reuse) a connection.
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
                        // The session's connection and schema: the editor's last, not the file's saved ones.
                        let conn = t.connection_id.clone().filter(|c| self.conns.config(c).is_some());
                        let schema = (t.schema.is_some() || t.database.is_some()).then(|| dbiji_core::meta::SchemaRef::new(t.database.clone(), t.schema.clone()));
                        let _ = self.open_script_with(p, conn.map(|c| (c, schema)));
                    }
                }
                dbiji_config::project::TabKind::SqlScript | dbiji_config::project::TabKind::SqlConsole => {
                    let conn = t.connection_id.clone().filter(|c| self.conns.config(c).is_some());
                    let schema = (t.schema.is_some() || t.database.is_some()).then(|| dbiji_core::meta::SchemaRef::new(t.database.clone(), t.schema.clone()));
                    let console = t.kind == dbiji_config::project::TabKind::SqlConsole;
                    self.script_counter += 1;
                    let title = t.title.clone().unwrap_or_else(|| format!("Script-{}", self.script_counter));
                    self.open_tab(Box::new(crate::tabs::sql_editor::SqlEditorTab::new(conn, schema, t.content.clone().unwrap_or_default(), None, console, title)));
                }
                dbiji_config::project::TabKind::ObjectEditor | dbiji_config::project::TabKind::DataEditor => {
                    let (Some(conn), Some(obj)) = (t.connection_id.clone().filter(|c| self.conns.config(c).is_some()), t.object.clone()) else { continue };
                    let page = if t.kind == dbiji_config::project::TabKind::DataEditor { crate::command::ObjectPage::Data } else { crate::command::ObjectPage::Properties };
                    self.open_tab(Box::new(crate::tabs::object::ObjectEditorTab::new(conn, obj, page)));
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
                }
                _ => {}
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
        self.open_script_with(path, None)
    }

    /// Open a script with this connection and schema instead of the ones saved for it.
    fn open_script_with(&mut self, path: std::path::PathBuf, binding: Option<(String, Option<dbiji_core::meta::SchemaRef>)>) -> Option<u64> {
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
        let (conn, schema) = match binding {
            Some((c, s)) => (Some(c), s),
            None => (conn, schema),
        };
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
        for (conn, obj, job) in std::mem::take(&mut self.pending_routines) {
            // Another database of the server: wait until its session is open.
            let mut job = match job {
                Some(j) => j,
                None => {
                    let Some(live) = self.conns.live_mut(&conn) else { continue };
                    match live.ensure_session(&self.backend, obj.database.as_deref()) {
                        Some(session) => {
                            let r = obj.clone();
                            self.backend.spawn(async move { session.lock().await.routine_definition(&r).await })
                        }
                        None => {
                            match live.session_error(obj.database.as_deref()) {
                                Some(e) => self.toasts.error(tr("Cannot read the routine"), Some(e)),
                                None => still.push((conn, obj, None)),
                            }
                            continue;
                        }
                    }
                }
            };
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
                None => still.push((conn, obj, Some(job))),
            }
        }
        self.pending_routines = still;
    }

    fn poll_generate(&mut self) {
        let mut still = Vec::new();
        for (conn, obj, kind) in std::mem::take(&mut self.pending_generate) {
            let Some(live) = self.conns.live_mut(&conn) else { continue };
            // Read in the object's database (main is another database's session).
            if live.meta.table_defs.get(&obj).is_none_or(|l| l.is_not_loaded()) {
                match live.ensure_session(&self.backend, obj.database.as_deref()) {
                    Some(session) => live.meta.request_table_def(&self.backend, &session, &obj),
                    None => {
                        match live.session_error(obj.database.as_deref()) {
                            Some(e) => self.toasts.error(tr("Cannot read the table"), Some(e)),
                            None => still.push((conn, obj, kind)),
                        }
                        continue;
                    }
                }
            }
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
        // Another time zone: the connections whose sessions have one reconnect.
        if self.conns.time_zone != self.settings.general.time_zone {
            self.conns.time_zone = self.settings.general.time_zone;
            for id in self.conns.live_ids() {
                if self.conns.config(&id).is_some_and(|c| c.kind != dbiji_core::DbKind::Sqlite) {
                    self.commands.push(AppCommand::Reconnect(id));
                }
            }
        }
        self.settings.results.apply_memory_limits();
        let (keymap, _) = crate::keys::Keymap::from_config(&self.settings.keymap, crate::keys::IS_MAC);
        self.keymap = keymap;
        // History and the task schedule follow the settings at once (not at the next start).
        match (self.settings.history.enabled, self.services.history.is_some()) {
            (true, false) => {
                let mut warnings = Vec::new();
                self.services.history = crate::services::Services::open_history(&self.services.paths, &self.settings, &mut warnings);
                for w in warnings {
                    self.toasts.warning(w, None);
                }
            }
            (false, true) => self.services.history = None,
            _ => {}
        }
        if let Some(h) = &self.services.history {
            let mut h = h.lock();
            h.set_redaction(self.settings.history.redact_secrets);
            let _ = h.set_retention(Some(self.settings.history.retention()));
        }
        if let Some(s) = &self.services.scheduler {
            match (self.settings.tasks.scheduler_enabled, s.is_started()) {
                (true, false) => {
                    if let Err(e) = s.start() {
                        self.toasts.error(tr("Cannot start the task scheduler"), Some(e.to_string()));
                    }
                }
                (false, true) => s.stop_schedule(),
                _ => {}
            }
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

    /// The connection the Database menu works on: the selected one, when connected.
    fn menu_connection(&self) -> Option<String> {
        self.selected_connection().filter(|c| self.conns.is_connected(c))
    }

    /// The state of the main menu's items (enabled, checked).
    pub(crate) fn menu_state(&self) -> crate::menu::MenuState {
        let (new_database, new_schema) = self.menu_connection().as_deref().and_then(|c| self.conns.live(c)).map_or((false, false), |l| {
            let probe = dbiji_core::NewDatabase { name: "x".into(), ..Default::default() };
            (l.dialect.create_database_sql(&probe).is_some(), l.dialect.create_schema_sql("x", None).is_some())
        });
        crate::menu::MenuState {
            new_database,
            new_schema,
            properties: self.settings.navigator.show_properties,
            theme: self.settings.general.theme,
            diagnostics: self.diagnostics.is_none(),
            updates: !self.updates.checking(),
        }
    }

    /// Run an item of the main menu (the in-window one or macOS's).
    pub(crate) fn run_menu_action(&mut self, ctx: &egui::Context, action: crate::menu::MenuAction) {
        use crate::menu::MenuAction as M;
        let cmd = match action {
            M::NewSqlEditor => Some(AppCommand::NewSqlEditor { connection: None, schema: None, text: None }),
            M::OpenScript => rfd::FileDialog::new().add_filter("SQL", &["sql"]).pick_file().map(AppCommand::OpenScript),
            M::Settings => Some(AppCommand::OpenSettings),
            M::Quit => Some(AppCommand::Quit),
            M::NewConnection => Some(AppCommand::NewConnection { kind: None, folder: None }),
            M::ImportConnections => Some(AppCommand::ImportConnections),
            M::NewDatabase => self.menu_connection().map(|c| AppCommand::NewDatabase { connection: c }),
            M::NewSchema => {
                let database = self.nav.selected_schema().and_then(|s| s.database);
                self.menu_connection().map(|c| AppCommand::NewSchema { connection: c, database })
            }
            M::DisconnectAll => Some(AppCommand::DisconnectAll),
            M::PendingTransactions => {
                self.pending_tx.open = true;
                None
            }
            M::CompareSchemas => Some(AppCommand::OpenSchemaCompare { connection: None, schema: None }),
            M::CompareData => Some(AppCommand::OpenDataCompare { connection: None, table: None }),
            M::Search => Some(AppCommand::OpenSearch { connection: None, data: false }),
            M::QueryHistory => Some(AppCommand::OpenHistory),
            M::Tasks => Some(AppCommand::OpenTasks),
            M::AiAssistant => Some(AppCommand::OpenAiAssistant { connection: self.nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string) }),
            M::BackgroundOperations => Some(AppCommand::ToggleOperations),
            M::ToggleNavigator => Some(AppCommand::ToggleNavigator),
            M::PropertiesPanel => {
                self.settings.navigator.show_properties = !self.settings.navigator.show_properties;
                self.save_settings();
                None
            }
            M::Theme(t) => {
                self.set_theme(ctx, t);
                None
            }
            M::CommandPalette => Some(AppCommand::ShowPalette),
            M::KeyboardShortcuts => Some(AppCommand::ShowShortcuts),
            M::DatabaseDrivers => {
                self.dialogs.drivers_open = true;
                None
            }
            M::CreateSampleDatabase => Some(AppCommand::CreateSampleDatabase),
            M::TipOfTheDay => {
                self.show_tip();
                None
            }
            M::ErrorLog => {
                self.open_tab(Box::new(crate::tabs::errorlog::ErrorLogTab::new()));
                None
            }
            M::CollectDiagnostics => {
                if self.diagnostics.is_none() {
                    self.collect_diagnostics(None);
                }
                None
            }
            M::WhatsNew => {
                self.updates.show_whats_new();
                None
            }
            M::CheckForUpdates => Some(AppCommand::CheckForUpdates),
            M::About => Some(AppCommand::ShowAbout),
        };
        if let Some(c) = cmd {
            self.commands.push(c);
        }
    }

    /// The in-window menu bar (macOS shows the same menus in its own menu bar instead).
    fn menu_bar(&mut self, ui: &mut Ui) {
        use crate::menu::MenuAction as M;
        let state = self.menu_state();
        let chosen: std::cell::Cell<Option<M>> = std::cell::Cell::new(None);
        egui::MenuBar::new().ui(ui, |ui| {
            // An item: an optional (coloured) icon and its text.
            let item = |ui: &mut Ui, enabled: bool, glyph: Option<&str>, text: &str, action: M| {
                let button = match glyph {
                    Some(g) => egui::Button::new((dbiji_icons::ic(g), text)),
                    None => egui::Button::new(text),
                };
                if ui.add_enabled(enabled, button).clicked() {
                    chosen.set(Some(action));
                    ui.close();
                }
            };
            ui.menu_button(tr("File"), |ui| {
                item(ui, true, Some(icon::FILE_SQL), tr("New SQL editor"), M::NewSqlEditor);
                item(ui, true, Some(icon::FOLDER_OPEN), tr("Open script…"), M::OpenScript);
                ui.separator();
                item(ui, true, Some(icon::GEAR), tr("Settings…"), M::Settings);
                ui.separator();
                item(ui, true, None, tr("Quit"), M::Quit);
            });
            ui.menu_button(tr("Database"), |ui| {
                item(ui, true, Some(icon::PLUS), tr("New connection…"), M::NewConnection);
                item(ui, true, Some(icon::DOWNLOAD_SIMPLE), tr("Import connections…"), M::ImportConnections);
                ui.separator();
                item(ui, state.new_database, Some(icon::DATABASE), tr("New database…"), M::NewDatabase);
                item(ui, state.new_schema, Some(icon::STACK), tr("New schema…"), M::NewSchema);
                ui.separator();
                item(ui, true, Some(icon::PLUGS), tr("Disconnect all"), M::DisconnectAll);
                item(ui, true, Some(icon::QUEUE), tr("Pending transactions…"), M::PendingTransactions);
                ui.separator();
                item(ui, true, Some(icon::GIT_DIFF), tr("Compare schemas…"), M::CompareSchemas);
                item(ui, true, Some(icon::ARROWS_LEFT_RIGHT), tr("Compare data…"), M::CompareData);
                item(ui, true, Some(icon::MAGNIFYING_GLASS), tr("Search…"), M::Search);
            });
            ui.menu_button(tr("Tools"), |ui| {
                item(ui, true, Some(icon::CLOCK_COUNTER_CLOCKWISE), tr("Query history"), M::QueryHistory);
                item(ui, true, Some(icon::LIST_CHECKS), tr("Tasks"), M::Tasks);
                item(ui, true, Some(icon::SPARKLE), tr("AI assistant"), M::AiAssistant);
                item(ui, true, Some(icon::HOURGLASS), tr("Background operations"), M::BackgroundOperations);
            });
            ui.menu_button(tr("View"), |ui| {
                item(ui, true, None, tr("Toggle navigator"), M::ToggleNavigator);
                let mut props = state.properties;
                if ui.checkbox(&mut props, tr("Properties panel")).clicked() {
                    chosen.set(Some(M::PropertiesPanel));
                    ui.close();
                }
                ui.menu_button((dbiji_icons::ic(icon::CIRCLE_HALF), tr("Theme")), |ui| {
                    use dbiji_config::settings::Theme;
                    for (t, label) in [(Theme::System, tr("System")), (Theme::Light, tr("Light")), (Theme::Dark, tr("Dark"))] {
                        if ui.radio(state.theme == t, label).clicked() {
                            chosen.set(Some(M::Theme(t)));
                            ui.close();
                        }
                    }
                });
                item(ui, true, None, tr("Command palette"), M::CommandPalette);
            });
            ui.menu_button(tr("Help"), |ui| {
                item(ui, true, None, tr("Keyboard shortcuts"), M::KeyboardShortcuts);
                item(ui, true, None, tr("Database drivers"), M::DatabaseDrivers);
                item(ui, true, None, tr("Create sample database"), M::CreateSampleDatabase);
                item(ui, true, Some(icon::LIGHTBULB), tr("Tip of the day"), M::TipOfTheDay);
                ui.separator();
                item(ui, true, Some(icon::WARNING_OCTAGON), tr("Error log"), M::ErrorLog);
                item(ui, state.diagnostics, Some(icon::FILE_ZIP), tr("Collect diagnostic info"), M::CollectDiagnostics);
                ui.separator();
                item(ui, true, Some(icon::SPARKLE), tr("What's new"), M::WhatsNew);
                item(ui, state.updates, Some(icon::ARROW_CIRCLE_UP), tr("Check for updates…"), M::CheckForUpdates);
                item(ui, true, None, tr("About DBiji"), M::About);
            });
        });
        if let Some(a) = chosen.get() {
            self.run_menu_action(ui.ctx(), a);
        }
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
        let needs_connection = matches!(
            action,
            A::EditConnection | A::Connect | A::Disconnect | A::Reconnect | A::OpenSqlConsole | A::OpenSessionManager | A::OpenErDiagram | A::NewDatabase | A::Export | A::Import
        );
        if needs_connection && self.selected_connection().is_none() {
            return Err(tr("Select a connection in the navigator first."));
        }
        Ok(())
    }

    /// Main toolbar (customisable in the settings): its groups' buttons with
    /// short labels, as the grid's toolbar; when the window is too narrow for
    /// them all, the last ones show their icon only.
    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let hidden = &self.settings.toolbar.hidden;
            let items: Vec<Option<crate::keys::Action>> = self
                .settings
                .toolbar
                .items
                .iter()
                .filter(|id| !hidden.contains(id))
                .filter_map(|id| if id == "|" { Some(None) } else { crate::keys::Action::from_id(id).map(Some) })
                .collect();
            // Labels from the first button on, as many as fit next to the connection and schema.
            let font = egui::TextStyle::Button.resolve(ui.style());
            let pad = 2.0 * ui.spacing().button_padding.x + ui.spacing().item_spacing.x;
            let icon_w = 16.0 + pad;
            let label_w = |a: crate::keys::Action| {
                ui.fonts_mut(|f| f.layout_no_wrap(crate::keys::toolbar_label(a).to_string(), font.clone(), egui::Color32::WHITE).size().x) + ui.spacing().icon_spacing + 4.0
            };
            let actions: Vec<crate::keys::Action> = items.iter().flatten().copied().collect();
            let widths: Vec<f32> = actions.iter().map(|&a| label_w(a)).collect();
            let separators = items.iter().filter(|i| i.is_none()).count() as f32 * 8.0;
            let budget = ui.available_width() - 300.0 - separators - icon_w * actions.len() as f32;
            let mut labelled = 0;
            let mut used = 0.0;
            for w in &widths {
                if used + w > budget {
                    break;
                }
                used += w;
                labelled += 1;
            }
            let mut index = 0;
            for item in items {
                let Some(action) = item else {
                    ui.separator();
                    continue;
                };
                let with_label = index < labelled;
                index += 1;
                let keys = crate::keys::label(&self.keymap, action);
                let name = crate::keys::action_label(action);
                let tip = if keys.is_empty() { name.to_string() } else { format!("{name}  ({keys})") };
                let state = self.toolbar_action_state(action);
                let mut icon = RichText::new(crate::keys::action_icon(action)).size(16.0);
                if let Some(c) = crate::keys::action_color(action, ui.visuals().dark_mode).filter(|_| state.is_ok()) {
                    icon = icon.color(c);
                }
                // The AI assistant's button shows whether its panel is open.
                let open = action == crate::keys::Action::AiAssistant && self.ai_open;
                let b = if with_label { egui::Button::new((icon, crate::keys::toolbar_label(action))) } else { egui::Button::new(icon) };
                let b = b.selected(open).min_size(egui::vec2(26.0, 22.0));
                let resp = ui.add_enabled(state.is_ok(), b).on_hover_text(&tip);
                let resp = match state {
                    Err(why) => resp.on_disabled_hover_text(format!("{tip}\n{why}")),
                    Ok(()) => resp,
                };
                if resp.clicked() {
                    self.commands.push(AppCommand::Action(action));
                }
            }
            self.toolbar_location(ui);
            // An update to install.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.updates.toolbar_button(ui));
        });
    }

    /// Connection (logo, name, type colour) and schema@database of the
    /// focused tab; a click shows it in the navigator.
    fn toolbar_location(&mut self, ui: &mut Ui) {
        let Some((_, tab)) = self.dock.find_active_focused() else { return };
        let Some(loc) = crate::tabs::tab_location(tab.view.as_ref(), &self.conns) else { return };
        let object = tab.view.navigator_object();
        ui.separator();
        ui.spacing_mut().item_spacing.x = 4.0;
        let dark = ui.visuals().dark_mode;
        let color = crate::theme::logo_color(loc.kind, dark);
        let name = RichText::new(&loc.name).strong();
        let mut button = match crate::dblogo::image(ui.ctx(), loc.kind, 16.0, color) {
            Some(logo) => egui::Button::new((logo, name)),
            None => egui::Button::new((RichText::new(crate::nav::db_icon(loc.kind)).color(color), name)),
        };
        let ty = self.conns.type_of(&loc.conn);
        if let Some(c) = crate::theme::connection_marker(ty.color) {
            button = button.fill(crate::theme::tint(c, dark));
        }
        let tip = format!("{}: {} ({})\n{}", tr("Connection"), loc.name, ty.name, tr("Show in the navigator"));
        let mut reveal = ui.add(button).on_hover_text(&tip).clicked();
        if let Some(place) = &loc.place {
            let text = (dbiji_icons::ic(icon::STACK), place.as_str());
            reveal |= ui.add(egui::Button::new(text)).on_hover_text(format!("{}\n{}", tr("Schema"), tr("Show in the navigator"))).clicked();
        }
        if reveal {
            self.show_nav = true;
            self.show_project = false;
            match object {
                Some((conn, obj)) => self.nav.reveal(&conn, &obj, &self.conns),
                None => self.nav.select(crate::nav::NavNode::Connection(loc.conn)),
            }
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            // Where the active tab works, as DBeaver's status bar:
            // connection › database › schema › object.
            let active = self.dock.find_active_focused();
            let location = active.as_ref().and_then(|(_, t)| crate::tabs::tab_location(t.view.as_ref(), &self.conns));
            let object = active.as_ref().and_then(|(_, t)| t.view.navigator_object());
            match location {
                Some(loc) => {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let ty = self.conns.type_of(&loc.conn);
                    if let Some(color) = crate::theme::connection_marker(ty.color) {
                        ui.label(RichText::new(format!("■ {}", ty.name)).color(color)).on_hover_text(tr("Connection type"));
                        ui.separator();
                    }
                    let dark = ui.visuals().dark_mode;
                    match crate::dblogo::image(ui.ctx(), loc.kind, 14.0, crate::theme::logo_color(loc.kind, dark)) {
                        Some(img) => ui.add(img),
                        None => ui.label(dbiji_icons::ic(crate::nav::db_icon(loc.kind))),
                    };
                    let server = match self.conns.live(&loc.conn) {
                        // `version()` texts repeat the product ("PostgreSQL 16.4 on …"): show the number.
                        Some(live) => {
                            let number = live.server.version.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())).unwrap_or("");
                            format!("{} {number}", live.server.product)
                        }
                        None => tr("not connected").to_string(),
                    };
                    ui.label(&loc.name).on_hover_text(server);
                    let crumb = |ui: &mut Ui, kind: dbiji_core::meta::ObjectKind, name: &str| {
                        ui.weak(icon::CARET_RIGHT);
                        ui.label(dbiji_icons::ic(crate::nav::kind_icon(kind)));
                        ui.label(name);
                    };
                    if self.conns.is_connected(&loc.conn) {
                        if let Some(d) = &loc.database {
                            crumb(ui, dbiji_core::meta::ObjectKind::Database, d);
                        }
                        if let Some(s) = &loc.schema {
                            crumb(ui, dbiji_core::meta::ObjectKind::Schema, s);
                        }
                    }
                    if let Some((_, obj)) = object.filter(|(c, _)| *c == loc.conn) {
                        crumb(ui, obj.kind, &obj.name);
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
                ui.weak(format!("{} {}", icon::MEMORY, crate::widgets::format_bytes(mem as i64))).on_hover_text(tr("Memory used by result sets"));
                ui.separator();
                // Language and time zone, as in DBeaver ("en", "WIB"): a click chooses another.
                let locale = self.settings.general.effective_locale();
                let lang = ui.add(egui::Button::new(locale.code()).frame_when_inactive(false)).on_hover_text(tr("Language"));
                let mut changed = false;
                egui::Popup::menu(&lang).show(|ui| {
                    let mut v = self.settings.general.locale;
                    for (choice, label) in [(None, tr("System")), (Some(dbiji_core::i18n::Locale::En), "English"), (Some(dbiji_core::i18n::Locale::Id), "Bahasa Indonesia")] {
                        changed |= ui.radio_value(&mut v, choice, label).changed();
                    }
                    self.settings.general.locale = v;
                });
                let tz = self.settings.general.time_zone;
                let tz_button = ui.add(egui::Button::new(crate::conn::time_zone_label(tz)).frame_when_inactive(false));
                let tz_tip = crate::conn::time_zone_choices().into_iter().find(|(c, _)| *c == tz).map(|(_, l)| l).unwrap_or_default();
                let tz_button = tz_button.on_hover_text(format!("{}: {tz_tip}", tr("Time zone")));
                egui::Popup::menu(&tz_button).show(|ui| {
                    let mut v = tz;
                    for (choice, label) in crate::conn::time_zone_choices() {
                        changed |= ui.radio_value(&mut v, choice, label).changed();
                    }
                    self.settings.general.time_zone = v;
                });
                if changed {
                    self.commands.push(AppCommand::SettingsChanged);
                    self.save_settings();
                }
            });
        });
    }
}

impl eframe::App for DbijiApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        crate::theme::sync_fonts(&ctx);
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
        // The status dots follow a connection the server closed while DBiji was idle.
        if !self.conns.live_ids().is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_secs(5));
        }
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
            // Unsaved tabs, open transactions, or work in progress (a migration, an import).
            let dirty = self.dock.iter_all_tabs().any(|(_, t)| t.view.is_dirty()) || !crate::pending_tx::collect(&self.dock, &self.conns).is_empty() || self.ops.running() > 0;
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
                ui.heading(tr("Quit DBiji?"));
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
                let running = self.ops.running();
                if running > 0 {
                    ui.label(dbiji_core::trf("{} operation(s) are still running (export, import, migration …); quitting stops them half way.", &[&running]));
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
                Some(false) => {
                    self.confirm_quit = false;
                    self.updates.quit_cancelled();
                }
                None if modal.should_close() => {
                    self.confirm_quit = false;
                    self.updates.quit_cancelled();
                }
                None => {}
            }
        }
        self.confirm_disconnect_ui(&ctx);
        self.close_confirm_ui(&ctx);
        // Files dropped on the window: scripts open in editors, database files as connections.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for path in dropped {
            self.open_path(&path, false);
        }

        // On macOS the menus are in the system's menu bar (as in DBeaver).
        #[cfg(target_os = "macos")]
        let native_menu = self.app_menu.is_some();
        #[cfg(not(target_os = "macos"))]
        let native_menu = false;
        if !native_menu {
            egui::Panel::top("dbiji_menu").frame(crate::theme::chrome_frame(ui.style())).show(ui, |ui| self.menu_bar(ui));
        }
        egui::Panel::top("dbiji_toolbar").frame(crate::theme::chrome_frame(ui.style())).show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("dbiji_status").exact_size(24.0).frame(crate::theme::chrome_frame(ui.style())).show(ui, |ui| self.status_bar(ui));

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
            popup: false,
        };
        if self.show_nav {
            egui::Panel::left("dbiji_navigator").resizable(true).default_size(280.0).size_range(160.0..=600.0).show(ui, |ui| {
                // The tree's toolbar on the switch's row (as in DBeaver); below it
                // when the panel is too narrow for both.
                let mut toolbar_below = false;
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.show_project, (dbiji_icons::ic(icon::DATABASE), tr("Database"))).clicked() {
                        self.show_project = false;
                    }
                    if ui.selectable_label(self.show_project, (dbiji_icons::ic(icon::FOLDER_SIMPLE), tr("Project"))).clicked() {
                        self.show_project = true;
                    }
                    if !self.show_project {
                        if ui.available_width() >= crate::nav::Navigator::TOOLBAR_WIDTH {
                            self.nav.toolbar_ui(ui, &mut cx);
                        } else {
                            toolbar_below = true;
                        }
                    }
                });
                if toolbar_below {
                    ui.horizontal(|ui| self.nav.toolbar_ui(ui, &mut cx));
                }
                ui.separator();
                if self.show_project {
                    self.project.ui(ui, &mut cx);
                    self.nav.has_focus = false;
                } else {
                    self.nav.ui(ui, &mut cx);
                    self.nav.track_focus(ui);
                }
            });
        }
        // The AI assistant at the right of the tabs (as an agent panel), resizable up
        // to 720 points while the tabs keep 640 (narrower, their toolbars no longer fit).
        if self.ai_open
            && let Some(ai) = self.ai_panel.as_mut()
        {
            let max = (ui.available_width() - 640.0).clamp(320.0, 720.0);
            egui::Panel::right("dbiji_ai").resizable(true).default_size(440.0).size_range(320.0..=max).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} {}", icon::SPARKLE, tr("AI assistant"))).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close = egui::Button::new(dbiji_icons::ic(icon::X)).frame_when_inactive(false);
                        if ui.add(close).on_hover_text(tr("Hide the AI assistant")).clicked() {
                            self.ai_open = false;
                        }
                    });
                });
                ui.separator();
                crate::tabs::TabView::ui(ai, ui, &mut cx);
            });
        }
        self.active_tab = self.dock.find_active_focused().map(|(_, t)| t.id);
        // The window is named after the active tab, as in DBeaver ("DBiji - orders"):
        // windows are told apart in the Dock, Mission Control and ⌘-Tab.
        let title = self.dock.find_active_focused().map(|(_, t)| format!("DBiji - {}", t.view.title())).unwrap_or_else(|| "DBiji".into());
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
        if cx.settings.navigator.link_with_editor && self.active_tab != self.linked_tab {
            self.linked_tab = self.active_tab;
            if let Some((conn, obj)) = self.dock.find_active_focused().and_then(|(_, t)| t.view.navigator_object()) {
                self.nav.reveal(&conn, &obj, cx.conns);
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| {
            // A tab too wide for the space left (toolbars, grids) is cut at its edge
            // instead of painting over the AI assistant.
            ui.shrink_clip_rect(ui.max_rect());
            let suffixes = crate::tabs::tab_suffixes(self.dock.iter_all_tabs().map(|(_, t)| t), cx.conns);
            // Without tabs the editor area is the start page (as in DBeaver), not a tab.
            if self.dock.iter_all_tabs().next().is_none() {
                ui.painter().rect_filled(ui.max_rect(), 0.0, ui.visuals().window_fill);
                crate::tabs::welcome::show(ui, &mut cx);
                return;
            }
            let mut viewer = DockViewer { cx: &mut cx, close_vetoed: Vec::new(), close_confirm: Vec::new(), active_tab: self.active_tab, style: ui.style().clone(), suffixes };
            DockArea::new(&mut self.dock).style(crate::theme::dock_style(ui.style())).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
            for id in viewer.close_confirm {
                if !self.close_confirm.contains(&id) {
                    self.close_confirm.push(id);
                }
            }
        });
        // Wizards and actions in pop-up windows over the tabs.
        let center = ctx.content_rect().center();
        let raise = self.raise_popup.take();
        let pressed = ctx.input(|i| i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten());
        let mut clicked_popup = None;
        for tab in &mut self.popups {
            let mut open = true;
            let size = tab.view.popup_size().unwrap_or(egui::vec2(720.0, 520.0));
            // Title: coloured icon, the tab's title and its connection.
            let mut title = egui::text::LayoutJob::default();
            let style = ctx.global_style();
            let font = egui::FontSelection::Default;
            dbiji_icons::ic(tab.view.icon()).append_to(&mut title, &style, font.clone(), egui::Align::Center);
            RichText::new(format!("  {}", tab.view.title())).append_to(&mut title, &style, font.clone(), egui::Align::Center);
            if let Some(name) = tab.view.connection_id().and_then(|c| cx.conns.config(c)).map(|c| c.name.clone()) {
                RichText::new(format!("  ·  {name}")).weak().append_to(&mut title, &style, font, egui::Align::Center);
            }
            let shown = egui::Window::new(title)
                .id(egui::Id::new(("dbiji_popup", tab.id)))
                .default_size(size)
                .min_size(egui::vec2(360.0, 240.0))
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(center)
                .collapsible(false)
                .resizable(true)
                .open(&mut open)
                .show(&ctx, |ui| {
                    cx.tab_active = true;
                    cx.current_tab = Some(tab.id);
                    cx.popup = true;
                    tab.view.ui(ui, &mut cx);
                    cx.popup = false;
                    cx.current_tab = None;
                });
            if let Some(shown) = &shown
                && raise == Some(tab.id)
            {
                ctx.move_to_top(shown.response.layer_id);
            }
            if let (Some(p), Some(shown)) = (pressed, &shown)
                && shown.response.rect.contains(p)
            {
                clicked_popup = Some(tab.id);
            }
            // Closed by its button, or done (the toast told the result).
            if !open || tab.view.take_done() {
                cx.commands.push(AppCommand::CloseTab(tab.id));
            }
        }
        if pressed.is_some() {
            self.popup_focus = clicked_popup;
        }

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
        self.updates.install_on_exit();
        if let Some(s) = self.services.scheduler.clone() {
            self.backend.block_on(async move { s.shutdown(std::time::Duration::from_secs(3)).await });
        }
        self.ops.cancel_all();
        // Waited for: the after-disconnect commands (tunnels, VPN) run before DBiji exits.
        self.conns.disconnect_all_and_wait(&self.backend, std::time::Duration::from_secs(10));
    }
}
