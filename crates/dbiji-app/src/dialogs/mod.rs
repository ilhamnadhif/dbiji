//! Modal and floating dialogs owned by the application.

pub mod connection;
pub mod create_db;
pub mod drop;
pub mod import;
pub mod vault;

use crate::command::AppCommand;
use crate::conn::{ConnectionManager, Credentials, PromptField};
use crate::cx::AppContext;
use connection::ConnectionDialog;
use dbiji_core::config::{SecretSlot, secret_key};
use dbiji_core::{Secret, tr, trf};
use egui::RichText;
use egui_phosphor::regular as icon;

struct CredentialPrompt {
    conn_id: String,
    conn_name: String,
    fields: Vec<PromptField>,
    password: String,
    ssh_password: String,
    ssh_passphrase: String,
    proxy_password: String,
    save: bool,
}

struct DeleteConfirm {
    conn_id: String,
    name: String,
}

/// What the folder dialog does.
#[derive(Debug, Clone, PartialEq)]
pub enum FolderAction {
    /// A new folder inside `parent` (`None`: the top level).
    New { parent: Option<String> },
    /// Rename this folder (a `/` separated path).
    Rename(String),
    /// Delete this folder; its connections and subfolders move up.
    Delete(String),
}

struct FolderDialog {
    action: FolderAction,
    name: String,
    error: Option<String>,
    focused: bool,
}

/// The folder a path is in, and its own name.
fn split_folder(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, path),
    }
}

#[derive(Default)]
pub struct Dialogs {
    folder: Option<FolderDialog>,
    connection: Vec<ConnectionDialog>,
    prompts: Vec<CredentialPrompt>,
    ready: Vec<(String, Credentials)>,
    delete: Option<DeleteConfirm>,
    pub palette_open: bool,
    pub about_open: bool,
    pub shortcuts_open: bool,
    pub vault_open: bool,
    pub drivers_open: bool,
    pub vault: vault::VaultDialog,
    pub import: Option<import::ImportDialog>,
    pub drop: Option<drop::DropDialog>,
    pub create_db: Option<create_db::CreateDatabaseDialog>,
}

impl Dialogs {
    pub fn new_connection(&mut self, kind: Option<dbiji_core::DbKind>, folder: Option<String>, _conns: &ConnectionManager, default_view: dbiji_core::config::NavigatorView) {
        let mut d = ConnectionDialog::new(kind, folder);
        d.cfg.navigator_view = default_view;
        // One new-connection dialog: asking again starts it over (another database type).
        self.connection.retain(|d| !d.is_new);
        self.connection.push(d);
    }

    pub fn edit_connection(&mut self, id: &str, conns: &ConnectionManager) {
        if self.connection.iter().any(|d| d.cfg.id == id) {
            return;
        }
        if let Some(cfg) = conns.config(id) {
            self.connection.push(ConnectionDialog::edit(cfg.clone()));
        }
    }

    pub fn confirm_delete_connection(&mut self, id: &str, conns: &ConnectionManager) {
        if let Some(cfg) = conns.config(id) {
            self.delete = Some(DeleteConfirm { conn_id: id.to_string(), name: cfg.name.clone() });
        }
    }

    /// New / rename / delete a navigator folder.
    pub fn folder_dialog(&mut self, action: FolderAction) {
        let name = match &action {
            FolderAction::Rename(path) => split_folder(path).1.to_string(),
            _ => String::new(),
        };
        self.folder = Some(FolderDialog { action, name, error: None, focused: false });
    }

    pub fn open_credentials(&mut self, id: &str, fields: Vec<PromptField>, conns: &ConnectionManager) {
        if self.prompts.iter().any(|p| p.conn_id == id) {
            return;
        }
        let Some(cfg) = conns.config(id) else { return };
        self.prompts.push(CredentialPrompt {
            conn_id: id.to_string(),
            conn_name: cfg.name.clone(),
            fields,
            password: String::new(),
            ssh_password: String::new(),
            ssh_passphrase: String::new(),
            proxy_password: String::new(),
            save: cfg.save_password && matches!(cfg.auth, dbiji_core::config::AuthMethod::Password),
        });
    }

    /// A dialog that takes keyboard input is open (shortcuts are suspended).
    pub fn captures_keyboard(&self) -> bool {
        self.palette_open
            || self.vault_open
            || self.delete.is_some()
            || self.folder.is_some()
            || self.drop.is_some()
            || !self.prompts.is_empty()
            || self.import.is_some()
            || self.create_db.is_some()
            || !self.connection.is_empty()
    }

    /// Credentials entered by the user since the last call.
    pub fn take_credentials(&mut self) -> Vec<(String, Credentials)> {
        std::mem::take(&mut self.ready)
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        for d in &mut self.connection {
            d.show(ctx, cx);
        }
        self.connection.retain(|d| d.open);
        self.show_prompts(ctx, cx);
        self.show_delete(ctx, cx);
        self.show_folder(ctx, cx);
        if self.vault_open {
            let mut open = true;
            self.vault.show(ctx, cx, &mut open);
            self.vault_open = open;
        }
        if let Some(d) = self.import.as_mut()
            && !d.show(ctx, cx)
        {
            self.import = None;
        }
        if let Some(d) = self.drop.as_mut()
            && !d.show(ctx, cx)
        {
            self.drop = None;
        }
        if let Some(d) = self.create_db.as_mut()
            && !d.show(ctx, cx)
        {
            self.create_db = None;
        }
        if self.about_open {
            let mut open = true;
            let shown = egui::Window::new(tr("About DBiji")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("DBiji").size(26.0).strong());
                    ui.label(format!("{} {}", tr("Version"), env!("CARGO_PKG_VERSION")));
                    ui.label(tr("Lightweight SQL database client written 100% in Rust."));
                    ui.add_space(6.0);
                    ui.weak(tr("License: MIT OR Apache-2.0"));
                    ui.weak(tr("No telemetry. Only connects to the servers you configure."));
                });
            });
            self.about_open = open && !crate::widgets::escape_closes(ctx, shown.map(|r| r.response.layer_id));
        }
        if self.drivers_open {
            let mut open = true;
            let shown = egui::Window::new(format!("{} {}", icon::PLUGS, tr("Database drivers"))).open(&mut open).default_width(720.0).show(ctx, |ui| {
                ui.label(tr("Drivers are built into DBiji (no downloads); network drivers are written in Rust, SQLite and DuckDB are embedded engines."));
                ui.add_space(4.0);
                let mut kinds = cx.conns.registry.kinds();
                kinds.sort_by_key(|k| k.display_name());
                let yes_no = |b: bool| if b { "✓" } else { "—" };
                egui::Grid::new("drivers_grid").num_columns(9).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                    for h in [tr("Database"), tr("Driver"), tr("Transactions"), tr("Explain"), tr("Sessions"), tr("Locks"), tr("Backup"), tr("Debugger"), tr("Cancel")] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for k in kinds {
                        let Some(d) = cx.conns.registry.get(k) else { continue };
                        let info = d.info();
                        let c = &info.capabilities;
                        ui.label(format!("{} {}", crate::nav::db_icon(k), k.display_name()));
                        ui.label(info.name).on_hover_text(info.description);
                        ui.label(yes_no(c.transactions));
                        ui.label(if c.explain_analyze { "✓ + ANALYZE" } else { yes_no(c.explain) });
                        ui.label(yes_no(c.session_manager));
                        ui.label(yes_no(c.lock_manager));
                        ui.label(yes_no(c.backup));
                        ui.label(yes_no(c.debugger));
                        ui.label(yes_no(c.cancel));
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.weak(tr("Not available: Oracle, Db2, Firebird, H2 and Trino (no Rust network driver yet)."));
            });
            self.drivers_open = open && !crate::widgets::escape_closes(ctx, shown.map(|r| r.response.layer_id));
        }
        if self.shortcuts_open {
            let mut open = true;
            let shown = egui::Window::new(tr("Keyboard shortcuts")).open(&mut open).default_width(420.0).show(ctx, |ui| {
                crate::keys::shortcuts_table(ui, cx);
            });
            self.shortcuts_open = open && !crate::widgets::escape_closes(ctx, shown.map(|r| r.response.layer_id));
        }
        if self.palette_open {
            let mut open = true;
            crate::palette::show(ctx, cx, &mut open);
            self.palette_open = open;
        }
    }

    fn show_prompts(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        let mut done: Vec<usize> = Vec::new();
        for (i, p) in self.prompts.iter_mut().enumerate() {
            let mut open = true;
            let mut submit = false;
            let shown = egui::Window::new(format!("{} {}", icon::KEY, trf("Connect to {}", &[&p.conn_name])))
                .id(egui::Id::new(("cred_prompt", &p.conn_id)))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new(("cred_grid", &p.conn_id)).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        let mut first = true;
                        for f in p.fields.clone() {
                            let (label, value) = match f {
                                PromptField::Password => (tr("Password"), &mut p.password),
                                PromptField::SshPassword => (tr("SSH password"), &mut p.ssh_password),
                                PromptField::SshPassphrase => (tr("Key passphrase"), &mut p.ssh_passphrase),
                                PromptField::ProxyPassword => (tr("Proxy password"), &mut p.proxy_password),
                            };
                            ui.label(label);
                            let r = ui.add(egui::TextEdit::singleline(value).password(true).desired_width(220.0));
                            // Enter first: re-focusing would hide that the field lost focus.
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                submit = true;
                            }
                            if first {
                                if !submit && !ui.ctx().memory(|m| m.focused().is_some()) {
                                    r.request_focus();
                                }
                                first = false;
                            }
                            ui.end_row();
                        }
                    });
                    ui.checkbox(&mut p.save, tr("Save password"));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(tr("Connect")).strong()).clicked() {
                            submit = true;
                        }
                        if ui.button(tr("Cancel")).clicked() {
                            done.push(i);
                        }
                    });
                });
            if submit {
                let opt = |s: &str| if s.is_empty() { None } else { Some(Secret::new(s)) };
                let creds =
                    Credentials { password: opt(&p.password), ssh_password: opt(&p.ssh_password), ssh_passphrase: opt(&p.ssh_passphrase), proxy_password: opt(&p.proxy_password) };
                if p.save {
                    let pairs = [
                        (SecretSlot::Password, &creds.password),
                        (SecretSlot::SshPassword, &creds.ssh_password),
                        (SecretSlot::SshPassphrase, &creds.ssh_passphrase),
                        (SecretSlot::ProxyPassword, &creds.proxy_password),
                    ];
                    for (slot, v) in pairs {
                        if let Some(v) = v {
                            let key = match cx.conns.config(&p.conn_id) {
                                Some(cfg) => cx.conns.store.network_secret_key(cfg, slot),
                                None => secret_key(&p.conn_id, slot),
                            };
                            if let Err(e) = cx.conns.secrets.set(&key, v) {
                                cx.toasts.error(tr("Cannot store the password securely"), Some(e.to_string()));
                            }
                        }
                    }
                    if let Some(cfg) = cx.conns.config_mut(&p.conn_id) {
                        // "Ask every time" with a saved password would keep asking: it is used from now on.
                        let ask = matches!(cfg.auth, dbiji_core::config::AuthMethod::Prompt) && creds.password.is_some();
                        if !cfg.save_password || ask {
                            cfg.save_password = true;
                            if ask {
                                cfg.auth = dbiji_core::config::AuthMethod::Password;
                            }
                            cx.push(AppCommand::SaveConnections);
                        }
                    }
                }
                self.ready.push((p.conn_id.clone(), creds));
                done.push(i);
            } else if !open || crate::widgets::escape_closes(ctx, shown.map(|r| r.response.layer_id)) {
                done.push(i);
            }
        }
        done.sort_unstable();
        done.dedup();
        for i in done.into_iter().rev() {
            self.prompts.remove(i);
        }
    }

    fn show_folder(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        let Some(d) = &mut self.folder else { return };
        let mut close = false;
        let red = |ui: &egui::Ui| crate::theme::error_color(ui.visuals().dark_mode);
        let modal = egui::Modal::new(egui::Id::new("dbiji_folder")).show(ctx, |ui| {
            ui.set_width(420.0);
            match d.action.clone() {
                FolderAction::Delete(path) => {
                    crate::widgets::icon_heading(ui, icon::FOLDER, trf("Delete the folder \"{}\"?", &[&split_folder(&path).1]));
                    let inside = |f: &Option<String>| f.as_deref().is_some_and(|f| f == path || f.starts_with(&format!("{path}/")));
                    let n = cx.conns.configs().iter().filter(|c| inside(&c.folder)).count();
                    let target = split_folder(&path).0.map(str::to_string).unwrap_or_else(|| tr("the top level").to_string());
                    ui.label(trf("Its {} connection(s) and its subfolders move to {}. No connection is deleted.", &[&n, &target]));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(format!("{}  {}", icon::TRASH, tr("Delete folder"))).color(red(ui))).clicked() {
                            match cx.conns.store.delete_folder(&path) {
                                Ok(_) => {
                                    cx.push(AppCommand::SaveConnections);
                                    close = true;
                                }
                                Err(e) => d.error = Some(e.to_string()),
                            }
                        }
                        if ui.button(tr("Cancel")).clicked() {
                            close = true;
                        }
                    });
                }
                action => {
                    let (title, parent) = match &action {
                        FolderAction::New { parent } => (tr("New folder"), parent.clone()),
                        FolderAction::Rename(path) => (tr("Rename folder"), split_folder(path).0.map(str::to_string)),
                        FolderAction::Delete(_) => unreachable!(),
                    };
                    crate::widgets::icon_heading(ui, icon::FOLDER, title);
                    if let Some(p) = &parent {
                        ui.weak(trf("In {}", &[p]));
                    }
                    ui.add_space(4.0);
                    let edit = ui.add(egui::TextEdit::singleline(&mut d.name).hint_text(tr("Folder name")).desired_width(f32::INFINITY));
                    if !d.focused {
                        edit.request_focus();
                        d.focused = true;
                    }
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let name = d.name.trim().to_string();
                    let path = match &parent {
                        Some(p) => format!("{p}/{name}"),
                        None => name.clone(),
                    };
                    let problem = if name.is_empty() {
                        Some(tr("Type a name"))
                    } else if name.contains('/') {
                        Some(tr("The name cannot contain \"/\""))
                    } else if cx.conns.folders().iter().any(|f| f == &path) && action != FolderAction::Rename(path.clone()) {
                        Some(tr("A folder with this name already exists"))
                    } else {
                        None
                    };
                    if let Some(p) = problem.filter(|_| !name.is_empty()) {
                        ui.colored_label(red(ui), p);
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let label = if matches!(action, FolderAction::New { .. }) { tr("Create") } else { tr("Rename") };
                        let ok = ui.add_enabled(problem.is_none(), crate::widgets::primary_button(ui, icon::CHECK, label)).clicked() || (enter && problem.is_none());
                        if ok {
                            let result = match &action {
                                FolderAction::New { .. } => {
                                    cx.conns.store.add_folder(&path);
                                    Ok(())
                                }
                                FolderAction::Rename(old) => cx.conns.store.rename_folder(old, &path),
                                FolderAction::Delete(_) => Ok(()),
                            };
                            match result {
                                Ok(()) => {
                                    cx.push(AppCommand::SaveConnections);
                                    close = true;
                                }
                                Err(e) => d.error = Some(e.to_string()),
                            }
                        }
                        if ui.button(tr("Cancel")).clicked() {
                            close = true;
                        }
                    });
                }
            }
            if let Some(e) = &d.error {
                ui.colored_label(red(ui), e);
            }
        });
        if close || modal.should_close() {
            self.folder = None;
        }
    }

    fn show_delete(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        let Some(d) = &self.delete else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("delete_conn")).show(ctx, |ui| {
            ui.heading(tr("Delete connection"));
            ui.label(trf("Delete the connection \"{}\" and its saved passwords?", &[&d.name]));
            // Disconnected first, the usual way: open transactions are asked about, the
            // editors' sessions close and the after-disconnect command runs.
            let connected = cx.conns.is_connected(&d.conn_id) || cx.conns.status(&d.conn_id) == crate::conn::ConnStatus::Connecting;
            if connected {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().warn_fg_color, tr("It is connected: disconnect it first."));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if connected && ui.button(tr("Disconnect")).clicked() {
                    cx.push(AppCommand::Disconnect(d.conn_id.clone()));
                }
                let delete = egui::Button::new(RichText::new(tr("Delete")).color(crate::theme::error_color(ui.visuals().dark_mode)));
                if ui.add_enabled(!connected, delete).clicked() {
                    cx.conns.remove(&d.conn_id);
                    cx.push(AppCommand::SaveConnections);
                    close = true;
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.delete = None;
        }
    }
}
