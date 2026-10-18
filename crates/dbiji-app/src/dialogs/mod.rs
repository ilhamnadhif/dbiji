//! Modal and floating dialogs owned by the application.

pub mod connection;
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

#[derive(Default)]
pub struct Dialogs {
    connection: Vec<ConnectionDialog>,
    prompts: Vec<CredentialPrompt>,
    ready: Vec<(String, Credentials)>,
    delete: Option<DeleteConfirm>,
    pub palette_open: bool,
    pub about_open: bool,
    pub shortcuts_open: bool,
    pub vault_open: bool,
    pub drivers_open: bool,
    vault: vault::VaultDialog,
    pub import: Option<import::ImportDialog>,
    pub drop: Option<drop::DropDialog>,
}

impl Dialogs {
    pub fn new_connection(&mut self, kind: Option<dbiji_core::DbKind>, folder: Option<String>, _conns: &ConnectionManager) {
        self.connection.push(ConnectionDialog::new(kind, folder));
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
        self.palette_open || self.vault_open || self.delete.is_some() || self.drop.is_some() || !self.prompts.is_empty() || self.import.is_some() || !self.connection.is_empty()
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
        if self.vault_open {
            let mut open = true;
            self.vault.show(ctx, cx, &mut open);
            self.vault_open = open;
        }
        if let Some(d) = self.import.as_mut() {
            if !d.show(ctx, cx) {
                self.import = None;
            }
        }
        if let Some(d) = self.drop.as_mut() {
            if !d.show(ctx, cx) {
                self.drop = None;
            }
        }
        if self.about_open {
            let mut open = true;
            egui::Window::new(tr("About DBiji")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("DBiji").size(26.0).strong());
                    ui.label(format!("{} {}", tr("Version"), env!("CARGO_PKG_VERSION")));
                    ui.label(tr("Lightweight SQL database client written 100% in Rust."));
                    ui.add_space(6.0);
                    ui.weak(tr("License: MIT OR Apache-2.0"));
                    ui.weak(tr("No telemetry. Only connects to the servers you configure."));
                });
            });
            self.about_open = open;
        }
        if self.drivers_open {
            let mut open = true;
            egui::Window::new(format!("{} {}", icon::PLUGS, tr("Database drivers"))).open(&mut open).default_width(720.0).show(ctx, |ui| {
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
            self.drivers_open = open;
        }
        if self.shortcuts_open {
            let mut open = true;
            egui::Window::new(tr("Keyboard shortcuts")).open(&mut open).default_width(420.0).show(ctx, |ui| {
                crate::keys::shortcuts_table(ui, cx);
            });
            self.shortcuts_open = open;
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
            egui::Window::new(format!("{} {}", icon::KEY, trf("Connect to {}", &[&p.conn_name])))
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
                            if first {
                                r.request_focus();
                                first = false;
                            }
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                submit = true;
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
                let creds = Credentials {
                    password: opt(&p.password),
                    ssh_password: opt(&p.ssh_password),
                    ssh_passphrase: opt(&p.ssh_passphrase),
                    proxy_password: opt(&p.proxy_password),
                };
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
                        if !cfg.save_password {
                            cfg.save_password = true;
                            cx.push(AppCommand::SaveConnections);
                        }
                    }
                }
                self.ready.push((p.conn_id.clone(), creds));
                done.push(i);
            } else if !open {
                done.push(i);
            }
        }
        done.sort_unstable();
        done.dedup();
        for i in done.into_iter().rev() {
            self.prompts.remove(i);
        }
    }

    fn show_delete(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        let Some(d) = &self.delete else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("delete_conn")).show(ctx, |ui| {
            ui.heading(tr("Delete connection"));
            ui.label(trf("Delete the connection \"{}\" and its saved passwords?", &[&d.name]));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Delete")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
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
