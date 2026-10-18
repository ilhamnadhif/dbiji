//! Main window: menu bar, toolbar, navigator, dock area, status bar, dialogs.

use crate::backend::Backend;
use crate::command::AppCommand;
use crate::conn::{ConnectionManager, Credentials, PromptField};
use crate::cx::AppContext;
use crate::dialogs::Dialogs;
use crate::nav::Navigator;
use crate::ops::Operations;
use crate::services::{Services, Settings};
use crate::tabs::{DockViewer, Tab, TabView};
use crate::toasts::Toasts;
use dbiji_core::tr;
use egui::{Key, KeyboardShortcut, Modifiers, RichText, Ui};
use egui_dock::{DockArea, DockState};
use egui_phosphor::regular as icon;
use std::sync::Arc;
use std::time::Instant;

pub struct DbijiApp {
    pub backend: Arc<Backend>,
    pub conns: ConnectionManager,
    pub services: Services,
    pub settings: Settings,
    pub dock: DockState<Tab>,
    next_tab_id: u64,
    pub nav: Navigator,
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
}

impl DbijiApp {
    pub fn new(backend: Arc<Backend>, conns: ConnectionManager, services: Services, settings: Settings) -> Self {
        let mut app = DbijiApp {
            backend,
            conns,
            services,
            settings,
            dock: DockState::new(Vec::new()),
            next_tab_id: 1,
            nav: Navigator::default(),
            show_nav: true,
            toasts: Toasts::default(),
            ops: Operations::default(),
            commands: Vec::new(),
            dialogs: Dialogs::default(),
            started: Instant::now(),
            first_frame: true,
            exit_after_first_frame: false,
            active_tab: None,
            exit_at: std::env::var("DBIJI_BENCH_EXIT_AFTER_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| Instant::now() + std::time::Duration::from_secs(s)),
        };
        app.open_tab(Box::new(crate::tabs::welcome::WelcomeTab));
        app
    }

    /// Open a tab, or focus an existing tab with the same dedupe key.
    pub fn open_tab(&mut self, view: Box<dyn TabView>) -> u64 {
        if let Some(key) = view.dedupe_key() {
            if let Some(path) = self.dock.find_tab_from(|t| t.view.dedupe_key().as_deref() == Some(key.as_str())) {
                let id = self.dock.iter_all_tabs().find(|(p, _)| *p == path).map(|(_, t)| t.id).unwrap_or(0);
                let _ = self.dock.set_active_tab(path);
                return id;
            }
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.dock.push_to_focused_leaf(Tab::new(id, view));
        id
    }

    fn close_tab(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            if let Some(mut tab) = self.dock.remove_tab(path) {
                let mut cx = AppContext {
                    backend: &self.backend,
                    conns: &mut self.conns,
                    services: &self.services,
                    settings: &mut self.settings,
                    commands: &mut self.commands,
                    toasts: &mut self.toasts,
                    ops: &mut self.ops,
                    tab_active: false,
                };
                if !tab.view.on_close(&mut cx) {
                    // put it back
                    self.dock.push_to_focused_leaf(tab);
                }
            }
        }
    }

    /// Request connecting, prompting for credentials when needed.
    fn request_connect(&mut self, id: &str) {
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
            AppCommand::NewConnection { kind, folder } => self.dialogs.new_connection(kind, folder, &self.conns),
            AppCommand::EditConnection(id) => self.dialogs.edit_connection(&id, &self.conns),
            AppCommand::DuplicateConnection(id) => {
                if let Some(c) = self.conns.config(&id).cloned() {
                    let dup = c.duplicate();
                    // copy saved secrets
                    for slot in [
                        dbiji_core::config::SecretSlot::Password,
                        dbiji_core::config::SecretSlot::SshPassword,
                        dbiji_core::config::SecretSlot::SshPassphrase,
                        dbiji_core::config::SecretSlot::ProxyPassword,
                    ] {
                        if let Ok(Some(s)) = self.conns.secrets.get(&dbiji_core::config::secret_key(&id, slot)) {
                            let _ = self.conns.secrets.set(&dbiji_core::config::secret_key(&dup.id, slot), &s);
                        }
                    }
                    self.conns.upsert(dup);
                    self.commands.push(AppCommand::SaveConnections);
                }
            }
            AppCommand::DeleteConnection(id) => self.dialogs.confirm_delete_connection(&id, &self.conns),
            AppCommand::ShowPalette => self.dialogs.palette_open = true,
            AppCommand::ToggleNavigator => self.show_nav = !self.show_nav,
            AppCommand::ToggleOperations => self.ops.panel_open = !self.ops.panel_open,
            AppCommand::ShowAbout => self.dialogs.about_open = true,
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

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        let palette = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::P);
        let new_editor = KeyboardShortcut::new(Modifiers::COMMAND, Key::CloseBracket);
        let toggle_nav = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::B);
        ctx.input_mut(|i| {
            if i.consume_shortcut(&palette) {
                self.commands.push(AppCommand::ShowPalette);
            }
            if i.consume_shortcut(&new_editor) {
                let conn = self.nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string);
                self.commands.push(AppCommand::NewSqlEditor { connection: conn, schema: None, text: None });
            }
            if i.consume_shortcut(&toggle_nav) {
                self.commands.push(AppCommand::ToggleNavigator);
            }
        });
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
                if ui.button(format!("{} {}", icon::PLUGS, tr("Disconnect all"))).clicked() {
                    self.commands.push(AppCommand::DisconnectAll);
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
                if ui.button(tr("About DBiji")).clicked() {
                    self.commands.push(AppCommand::ShowAbout);
                    ui.close();
                }
            });
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
                        ui.weak(format!("{} {}", live.server.product, live.server.version.split_whitespace().next().unwrap_or("")));
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
        for id in self.conns.just_connected.clone() {
            self.nav.expand(crate::nav::NavNode::Connection(id.clone()));
        }
        for (id, err) in self.conns.errors() {
            let name = self.conns.config(&id).map(|c| c.name.clone()).unwrap_or_default();
            self.toasts.error(dbiji_core::trf("Cannot connect to {}", &[&name]), Some(err));
            self.conns.take_error(&id);
        }
        self.global_shortcuts(&ctx);

        egui::Panel::top("dbiji_menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("dbiji_status").exact_size(24.0).show(ui, |ui| self.status_bar(ui));

        let mut cx = AppContext {
            backend: &self.backend,
            conns: &mut self.conns,
            services: &self.services,
            settings: &mut self.settings,
            commands: &mut self.commands,
            toasts: &mut self.toasts,
            ops: &mut self.ops,
            tab_active: false,
        };
        if self.show_nav {
            egui::Panel::left("dbiji_navigator")
                .resizable(true)
                .default_size(280.0)
                .size_range(160.0..=600.0)
                .show(ui, |ui| self.nav.ui(ui, &mut cx));
        }
        self.active_tab = self.dock.find_active_focused().map(|(_, t)| t.id);
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| {
            let mut viewer = DockViewer { cx: &mut cx, close_vetoed: Vec::new(), active_tab: self.active_tab };
            DockArea::new(&mut self.dock)
                .style(egui_dock::Style::from_egui(ui.style().as_ref()))
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, &mut viewer);
        });

        self.dialogs.show(&ctx, &mut cx);
        if self.ops.panel_open {
            let mut open = true;
            egui::Window::new(format!("{} {}", icon::HOURGLASS, tr("Background operations")))
                .open(&mut open)
                .default_width(360.0)
                .show(&ctx, |ui| self.ops.ui(ui));
            self.ops.panel_open = open;
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
        self.process_commands(&ctx);
        if self.conns.is_connecting() || self.ops.running() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let _ = PromptField::Password;
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.ops.cancel_all();
        self.conns.disconnect_all(&self.backend);
    }
}
