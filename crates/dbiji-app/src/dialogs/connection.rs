//! Connection wizard (FR-C1..C8): create, edit, test connections.

use crate::backend::Job;
use crate::conn::{ConnectionManager, Credentials};
use crate::cx::AppContext;
use dbiji_core::config::*;
use dbiji_core::{DbKind, DbResult, Secret, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Ssh,
    Proxy,
    Ssl,
    Init,
    Properties,
    Filters,
    General,
}

pub struct ConnectionDialog {
    pub cfg: ConnectionConfig,
    pub is_new: bool,
    page: Page,
    password: String,
    ssh_password: String,
    ssh_passphrase: String,
    proxy_password: String,
    password_loaded: bool,
    /// Saved secrets that could not be read (locked vault, keychain denied): left as they are on save.
    unreadable: Vec<SecretSlot>,
    auto_commit_choice: u8, // 0 = type default, 1 = on, 2 = off
    new_prop: (String, String),
    test: Option<Job<DbResult<String>>>,
    test_result: Option<Result<String, String>>,
    url: String,
    error: Option<String>,
    pub open: bool,
}

impl ConnectionDialog {
    pub fn new(kind: Option<DbKind>, folder: Option<String>) -> Self {
        let kind = kind.unwrap_or(DbKind::Postgres);
        let mut cfg = ConnectionConfig::new(kind, "");
        cfg.folder = folder;
        Self::from_config(cfg, true)
    }

    pub fn edit(cfg: ConnectionConfig) -> Self {
        Self::from_config(cfg, false)
    }

    fn from_config(cfg: ConnectionConfig, is_new: bool) -> Self {
        let auto_commit_choice = match cfg.auto_commit {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        ConnectionDialog {
            cfg,
            is_new,
            page: Page::Main,
            password: String::new(),
            ssh_password: String::new(),
            ssh_passphrase: String::new(),
            proxy_password: String::new(),
            password_loaded: false,
            unreadable: Vec::new(),
            auto_commit_choice,
            new_prop: Default::default(),
            test: None,
            test_result: None,
            url: String::new(),
            error: None,
            open: true,
        }
    }

    fn load_saved_secrets(&mut self, conns: &ConnectionManager) {
        if self.password_loaded || self.is_new {
            self.password_loaded = true;
            return;
        }
        self.password_loaded = true;
        let id = self.cfg.id.clone();
        let mut unreadable = Vec::new();
        let mut get = |slot| match conns.secrets.get(&secret_key(&id, slot)) {
            Ok(s) => s.map(|s| s.expose().to_string()).unwrap_or_default(),
            Err(_) => {
                unreadable.push(slot);
                String::new()
            }
        };
        self.password = get(SecretSlot::Password);
        self.ssh_password = get(SecretSlot::SshPassword);
        self.ssh_passphrase = get(SecretSlot::SshPassphrase);
        self.proxy_password = get(SecretSlot::ProxyPassword);
        self.unreadable = unreadable;
    }

    fn credentials(&self) -> Credentials {
        let opt = |s: &str| if s.is_empty() { None } else { Some(Secret::new(s)) };
        Credentials { password: opt(&self.password), ssh_password: opt(&self.ssh_password), ssh_passphrase: opt(&self.ssh_passphrase), proxy_password: opt(&self.proxy_password) }
    }

    fn validate(&self) -> Option<String> {
        if self.cfg.name.trim().is_empty() {
            return Some(tr("Enter a connection name.").to_string());
        }
        if self.cfg.kind.is_file_based() {
            if self.cfg.file_path.as_deref().is_none_or(|p| p.trim().is_empty()) {
                return Some(tr("Choose a database file.").to_string());
            }
        } else if self.cfg.host.trim().is_empty() {
            return Some(tr("Enter a host name.").to_string());
        }
        if let Some(ssh) = self.cfg.ssh.as_ref().filter(|s| s.enabled)
            && (ssh.host.trim().is_empty() || ssh.user.trim().is_empty())
        {
            return Some(tr("SSH tunnel needs a host and a user.").to_string());
        }
        None
    }

    fn sync_auto_commit(&mut self) {
        self.cfg.auto_commit = match self.auto_commit_choice {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
    }

    /// Store secrets and the configuration. Returns true when saved.
    fn save(&mut self, cx: &mut AppContext) -> bool {
        self.sync_auto_commit();
        if let Some(e) = self.validate() {
            self.error = Some(e);
            return false;
        }
        let id = self.cfg.id.clone();
        let unreadable = self.unreadable.clone();
        let store = |slot, value: &str, keep: bool| -> DbResult<()> {
            let key = secret_key(&id, slot);
            if keep && !value.is_empty() {
                cx.conns.secrets.set(&key, &Secret::new(value))
            } else if keep && unreadable.contains(&slot) {
                // Not read, not retyped: the saved one stays.
                Ok(())
            } else {
                cx.conns.secrets.delete(&key)
            }
        };
        let ssh_save = self.cfg.ssh.as_ref().is_some_and(|s| s.enabled && s.save_credentials);
        let proxy_save = self.cfg.proxy.as_ref().is_some_and(|p| p.enabled && p.save_credentials);
        let results = [
            store(SecretSlot::Password, &self.password, self.cfg.save_password && matches!(self.cfg.auth, AuthMethod::Password)),
            store(SecretSlot::SshPassword, &self.ssh_password, ssh_save),
            store(SecretSlot::SshPassphrase, &self.ssh_passphrase, ssh_save),
            store(SecretSlot::ProxyPassword, &self.proxy_password, proxy_save),
        ];
        if let Some(Err(e)) = results.into_iter().find(|r| r.is_err()) {
            self.error = Some(trf("Cannot store the password securely: {}", &[&e]));
            return false;
        }
        let was_connected = cx.conns.is_connected(&id);
        cx.conns.upsert(self.cfg.clone());
        cx.push(crate::command::AppCommand::SaveConnections);
        if was_connected {
            cx.toasts.info(tr("Reconnect to apply the changed settings."));
        }
        true
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        self.load_saved_secrets(cx.conns);
        if let Some(job) = self.test.as_mut()
            && let Some(r) = job.poll()
        {
            self.test_result = Some(r.map_err(|e| e.full_text()));
            self.test = None;
        }
        let title = if self.is_new { tr("New connection") } else { tr("Edit connection") };
        let mut open = self.open;
        let mut close = false;
        egui::Window::new(format!("{} {}", icon::PLUGS_CONNECTED, title))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([780.0, 520.0])
            .max_height((ctx.content_rect().height() - 80.0).max(360.0))
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                // Fixed body height: the vertical separators would otherwise
                // stretch the window to the full screen height.
                let body = (ctx.content_rect().height() - 220.0).clamp(240.0, 420.0);
                ui.horizontal_top(|ui| {
                    ui.set_max_height(body);
                    if self.is_new {
                        ui.vertical(|ui| {
                            ui.set_width(150.0);
                            ui.strong(tr("Database"));
                            for kind in DbKind::ALL {
                                if cx.conns.registry.get(kind).is_none() {
                                    continue;
                                }
                                let sel = self.cfg.kind == kind;
                                let color = crate::theme::logo_color(kind, ui.visuals().dark_mode);
                                let r = match crate::dblogo::image(ui.ctx(), kind, 18.0, color) {
                                    Some(logo) => ui.selectable_label(sel, (logo, kind.display_name())),
                                    None => ui.selectable_label(sel, (RichText::new(crate::nav::db_icon(kind)).color(color), kind.display_name())),
                                };
                                if r.clicked() && !sel {
                                    let name = self.cfg.name.clone();
                                    let folder = self.cfg.folder.clone();
                                    let id = self.cfg.id.clone();
                                    self.cfg = ConnectionConfig::new(kind, name);
                                    self.cfg.folder = folder;
                                    self.cfg.id = id;
                                }
                            }
                        });
                        ui.separator();
                    }
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            for (p, label) in [
                                (Page::Main, tr("Main")),
                                (Page::Ssh, tr("SSH")),
                                (Page::Proxy, tr("Proxy")),
                                (Page::Ssl, tr("SSL")),
                                (Page::Init, tr("Initialization")),
                                (Page::Properties, tr("Driver properties")),
                                (Page::Filters, tr("Filters")),
                                (Page::General, tr("General")),
                            ] {
                                if self.cfg.kind.is_file_based() && matches!(p, Page::Ssh | Page::Proxy | Page::Ssl) {
                                    continue;
                                }
                                ui.selectable_value(&mut self.page, p, label);
                            }
                        });
                        ui.separator();
                        egui::ScrollArea::vertical().max_height(body - 50.0).auto_shrink([false, true]).show(ui, |ui| match self.page {
                            Page::Main => self.main_page(ui, cx),
                            Page::Ssh => self.ssh_page(ui, cx),
                            Page::Proxy => self.proxy_page(ui, cx),
                            Page::Ssl => self.ssl_page(ui),
                            Page::Init => self.init_page(ui),
                            Page::Properties => self.properties_page(ui),
                            Page::Filters => self.filters_page(ui),
                            Page::General => self.general_page(ui, cx),
                        });
                    });
                });
                ui.separator();
                if let Some(e) = &self.error {
                    ui.colored_label(crate::theme::error_color(ui.visuals().dark_mode), e);
                }
                match &self.test_result {
                    Some(Ok(info)) => {
                        ui.colored_label(crate::theme::success_color(ui.visuals().dark_mode), format!("{} {} {info}", icon::CHECK_CIRCLE, tr("Connected:")));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(crate::theme::error_color(ui.visuals().dark_mode), format!("{} {e}", icon::X_CIRCLE));
                    }
                    None => {}
                }
                ui.horizontal(|ui| {
                    let testing = self.test.is_some();
                    if ui.add_enabled(!testing, egui::Button::new((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Test connection")))).clicked() {
                        self.sync_auto_commit();
                        self.test_result = None;
                        match self.validate() {
                            Some(e) => self.error = Some(e),
                            None => {
                                self.error = None;
                                self.test = Some(cx.conns.test_connection(cx.backend, self.cfg.clone(), self.credentials()));
                            }
                        }
                    }
                    if testing {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tr("Cancel")).clicked() {
                            close = true;
                        }
                        if ui.button(RichText::new(tr("Save")).strong()).clicked() && self.save(cx) {
                            close = true;
                        }
                        if self.is_new && ui.add(crate::widgets::primary_button(ui, icon::PLUGS_CONNECTED, tr("Save and connect"))).clicked() && self.save(cx) {
                            cx.push(crate::command::AppCommand::Connect(self.cfg.id.clone()));
                            close = true;
                        }
                    });
                });
            });
        self.open = open && !close;
    }

    fn main_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let kind = self.cfg.kind;
        egui::Grid::new("conn_main").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("Connection name"));
            ui.add(egui::TextEdit::singleline(&mut self.cfg.name).hint_text(kind.display_name()).desired_width(320.0));
            ui.end_row();
            if kind.is_file_based() {
                ui.label(tr("Database file"));
                ui.horizontal(|ui| {
                    let mut path = self.cfg.file_path.clone().unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut path).desired_width(260.0)).changed() {
                        self.cfg.file_path = Some(path);
                    }
                    if ui.button(dbiji_icons::ic(icon::FOLDER_OPEN)).on_hover_text(tr("Open existing file")).clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        self.cfg.file_path = Some(p.display().to_string());
                        if self.cfg.name.is_empty() {
                            self.cfg.name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        }
                    }
                    if ui.button(dbiji_icons::ic(icon::FILE_PLUS)).on_hover_text(tr("Create new database file")).clicked()
                        && let Some(p) = rfd::FileDialog::new().set_file_name("database.db").save_file()
                    {
                        self.cfg.file_path = Some(p.display().to_string());
                        if self.cfg.name.is_empty() {
                            self.cfg.name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        }
                    }
                });
                ui.end_row();
            } else {
                ui.label(tr("Host"));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.host).desired_width(220.0));
                    ui.label(tr("Port"));
                    let mut port = self.cfg.port.map(|p| p.to_string()).unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut port).desired_width(60.0).hint_text(kind.default_port().map(|p| p.to_string()).unwrap_or_default())).changed() {
                        self.cfg.port = port.trim().parse().ok();
                    }
                });
                ui.end_row();
                ui.label(tr("Database"));
                let mut db = self.cfg.database.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut db).desired_width(220.0).hint_text(kind.default_database().unwrap_or(""))).changed() {
                    self.cfg.database = if db.is_empty() { None } else { Some(db) };
                }
                ui.end_row();
                ui.label(tr("Authentication"));
                let current = auth_label(&self.cfg.auth);
                egui::ComboBox::from_id_salt("auth_method").selected_text(current).show_ui(ui, |ui| {
                    let mut options = vec![AuthMethod::Password, AuthMethod::Prompt];
                    if kind.is_postgres_family() {
                        options.push(AuthMethod::Pgpass);
                    }
                    options.push(AuthMethod::Command { command: String::new() });
                    options.push(AuthMethod::None);
                    for o in options {
                        let label = auth_label(&o);
                        let same = std::mem::discriminant(&o) == std::mem::discriminant(&self.cfg.auth);
                        if ui.selectable_label(same, label).clicked() && !same {
                            self.cfg.auth = o;
                        }
                    }
                });
                ui.end_row();
                if !matches!(self.cfg.auth, AuthMethod::None) {
                    ui.label(tr("User"));
                    let mut user = self.cfg.user.clone().unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut user).desired_width(220.0)).changed() {
                        self.cfg.user = if user.is_empty() { None } else { Some(user) };
                    }
                    ui.end_row();
                }
                match &mut self.cfg.auth {
                    AuthMethod::Password => {
                        ui.label(tr("Password"));
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(220.0));
                            ui.checkbox(&mut self.cfg.save_password, tr("Save password"));
                        });
                        ui.end_row();
                    }
                    AuthMethod::Command { command } => {
                        ui.label(tr("Password command"));
                        ui.add(egui::TextEdit::singleline(command).hint_text("security find-generic-password -w -s db").desired_width(320.0));
                        ui.end_row();
                    }
                    AuthMethod::Pgpass => {
                        ui.label("");
                        ui.weak(tr("The password is read from ~/.pgpass (or PGPASSFILE)."));
                        ui.end_row();
                    }
                    _ => {}
                }
                if cx.conns.registry.get(kind).is_some_and(|d| d.info().capabilities.multiple_databases) {
                    ui.label("");
                    ui.checkbox(&mut self.cfg.init.show_all_databases, tr("Show all databases"));
                    ui.end_row();
                }
            }
            ui.label(tr("Connection type"));
            ui.horizontal(|ui| {
                let types = cx.conns.connection_types();
                let current = types.iter().find(|t| t.id == self.cfg.connection_type).map(|t| t.name.clone()).unwrap_or_default();
                egui::ComboBox::from_id_salt("conn_type").selected_text(tr(&current)).show_ui(ui, |ui| {
                    for t in &types {
                        let marker = crate::theme::connection_marker(t.color).map(|c| RichText::new("■ ").color(c)).unwrap_or_else(|| RichText::new("□ "));
                        ui.horizontal(|ui| {
                            ui.label(marker);
                            ui.selectable_value(&mut self.cfg.connection_type, t.id.clone(), tr(&t.name));
                        });
                    }
                });
                ui.checkbox(&mut self.cfg.read_only, tr("Read-only"));
            });
            ui.end_row();
            ui.label(tr("Auto-commit"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.auto_commit_choice, 0, tr("Connection type default"));
                ui.radio_value(&mut self.auto_commit_choice, 1, tr("On"));
                ui.radio_value(&mut self.auto_commit_choice, 2, tr("Off (manual commit)"));
            });
            ui.end_row();
        });
        if self.cfg.kind.is_file_based() {
            return;
        }
        ui.add_space(8.0);
        ui.collapsing(tr("Connect with a URL"), |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("postgres://user@host:5432/db").desired_width(380.0));
                if ui.button(tr("Apply")).clicked() {
                    match crate::urls::parse(&self.url) {
                        Ok((mut cfg, pw)) => {
                            cfg.id = self.cfg.id.clone();
                            if cfg.name.is_empty() || !self.cfg.name.is_empty() {
                                cfg.name = if self.cfg.name.is_empty() { cfg.host.clone() } else { self.cfg.name.clone() };
                            }
                            cfg.folder = self.cfg.folder.clone();
                            cfg.connection_type = self.cfg.connection_type.clone();
                            self.cfg = cfg;
                            if let Some(pw) = pw {
                                self.password = pw.expose().to_string();
                            }
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            });
        });
    }

    fn network_profile_selector(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let profiles: Vec<(String, String)> = cx.conns.store.network_profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
        if profiles.is_empty() && self.cfg.network_profile.is_none() {
            return false;
        }
        ui.horizontal(|ui| {
            ui.label(tr("Network profile"));
            let current = self
                .cfg
                .network_profile
                .as_ref()
                .and_then(|id| profiles.iter().find(|(i, _)| i == id))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| tr("(none — settings below)").to_string());
            egui::ComboBox::from_id_salt("net_profile").selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.network_profile, None, tr("(none — settings below)"));
                for (id, name) in &profiles {
                    ui.selectable_value(&mut self.cfg.network_profile, Some(id.clone()), name);
                }
            });
        });
        if self.cfg.network_profile.is_some() {
            ui.weak(tr("SSH and proxy settings come from the network profile (Settings → Network profiles)."));
            true
        } else {
            false
        }
    }

    fn ssh_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.network_profile_selector(ui, cx) {
            return;
        }
        let ssh = self.cfg.ssh.get_or_insert_with(SshConfig::default);
        ssh_form(ui, "ssh_grid", ssh, &mut self.ssh_password, &mut self.ssh_passphrase);
    }

    fn proxy_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.network_profile_selector(ui, cx) {
            return;
        }
        let proxy = self.cfg.proxy.get_or_insert_with(ProxyConfig::default);
        proxy_form(ui, "proxy_grid", proxy, &mut self.proxy_password);
    }

    fn ssl_page(&mut self, ui: &mut Ui) {
        let ssl = &mut self.cfg.ssl;
        egui::Grid::new("ssl_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("SSL mode"));
            egui::ComboBox::from_id_salt("sslmode").selected_text(ssl.mode.label()).show_ui(ui, |ui| {
                for m in SslMode::ALL {
                    ui.selectable_value(&mut ssl.mode, m, m.label());
                }
            });
            ui.end_row();
            ui.label("");
            ui.weak(match ssl.mode {
                SslMode::Disable => tr("No encryption."),
                SslMode::Prefer => tr("Encrypt when the server supports it; the certificate is not verified."),
                SslMode::Require => tr("Always encrypt; the certificate is not verified."),
                SslMode::VerifyCa => tr("Always encrypt and verify the certificate chain."),
                SslMode::VerifyFull => tr("Always encrypt and verify the certificate chain and host name."),
            });
            ui.end_row();
            for (label, field) in [(tr("CA certificate"), &mut ssl.ca_cert), (tr("Client certificate"), &mut ssl.client_cert), (tr("Client key"), &mut ssl.client_key)] {
                ui.label(label);
                ui.horizontal(|ui| {
                    let mut v = field.clone().unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut v).desired_width(260.0)).changed() {
                        *field = if v.is_empty() { None } else { Some(v) };
                    }
                    if ui.button(dbiji_icons::ic(icon::FOLDER_OPEN)).clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("PEM", &["pem", "crt", "cer", "key"]).pick_file()
                    {
                        *field = Some(p.display().to_string());
                    }
                });
                ui.end_row();
            }
            ui.label("");
            ui.checkbox(&mut ssl.only_custom_ca, tr("Trust only this CA (ignore system certificates)"));
            ui.end_row();
        });
    }

    fn init_page(&mut self, ui: &mut Ui) {
        let init = &mut self.cfg.init;
        egui::Grid::new("init_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("Default schema"));
            let mut s = init.default_schema.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut s).desired_width(220.0)).changed() {
                init.default_schema = if s.is_empty() { None } else { Some(s) };
            }
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut init.separate_editor_connection, tr("Separate connection for each SQL editor"));
            ui.end_row();
            ui.label(tr("Keep-alive (s)"));
            ui.add(egui::DragValue::new(&mut init.keep_alive_secs).range(0..=3600));
            ui.end_row();
            ui.label(tr("Connect timeout (s)"));
            ui.add(egui::DragValue::new(&mut init.connect_timeout_secs).range(1..=600));
            ui.end_row();
            ui.label(tr("Query timeout (s)"));
            ui.add(egui::DragValue::new(&mut init.query_timeout_secs).range(0..=86_400));
            ui.end_row();
            ui.label(tr("Fetch size"));
            let mut fs = init.fetch_size.map(|f| f.to_string()).unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut fs).hint_text(tr("global setting")).desired_width(80.0)).changed() {
                init.fetch_size = fs.trim().parse().ok();
            }
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut init.show_system_objects, tr("Show system objects"));
            ui.end_row();
            ui.label(tr("Shell command before connect"));
            let mut sb = init.shell_before_connect.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut sb).desired_width(320.0)).changed() {
                init.shell_before_connect = if sb.is_empty() { None } else { Some(sb) };
            }
            ui.end_row();
            ui.label(tr("Shell command after disconnect"));
            let mut sa = init.shell_after_disconnect.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut sa).desired_width(320.0)).changed() {
                init.shell_after_disconnect = if sa.is_empty() { None } else { Some(sa) };
            }
            ui.end_row();
        });
        ui.label(tr("Initialization SQL (run after every connect)"));
        ui.add(egui::TextEdit::multiline(&mut init.init_sql).code_editor().desired_rows(5).desired_width(f32::INFINITY));
    }

    fn properties_page(&mut self, ui: &mut Ui) {
        ui.weak(tr("Driver specific properties, e.g. application_name, options, charset."));
        let mut remove = None;
        egui::Grid::new("props").num_columns(3).striped(true).show(ui, |ui| {
            for (k, v) in self.cfg.properties.iter_mut() {
                ui.label(k);
                ui.add(egui::TextEdit::singleline(v).desired_width(260.0));
                if ui.small_button(dbiji_icons::ic(icon::TRASH)).clicked() {
                    remove = Some(k.clone());
                }
                ui.end_row();
            }
            ui.add(egui::TextEdit::singleline(&mut self.new_prop.0).hint_text(tr("name")).desired_width(140.0));
            ui.add(egui::TextEdit::singleline(&mut self.new_prop.1).hint_text(tr("value")).desired_width(260.0));
            if ui.small_button(dbiji_icons::ic(icon::PLUS)).clicked() && !self.new_prop.0.trim().is_empty() {
                let (k, v) = std::mem::take(&mut self.new_prop);
                self.cfg.properties.insert(k.trim().to_string(), v);
            }
            ui.end_row();
        });
        if let Some(k) = remove {
            self.cfg.properties.remove(&k);
        }
    }

    fn filters_page(&mut self, ui: &mut Ui) {
        ui.weak(tr("Patterns use * and ? wildcards, separated by commas."));
        let f = &mut self.cfg.filters;
        for (label, filter) in [
            (tr("Databases"), &mut f.databases),
            (tr("Schemas"), &mut f.schemas),
            (tr("Tables"), &mut f.tables),
            (tr("Views"), &mut f.views),
            (tr("Procedures and functions"), &mut f.routines),
            (tr("Other objects"), &mut f.other),
        ] {
            ui.collapsing(label, |ui| {
                ui.checkbox(&mut filter.enabled, tr("Enable filter"));
                ui.add_enabled_ui(filter.enabled, |ui| {
                    let mut inc = filter.include.join(", ");
                    let mut exc = filter.exclude.join(", ");
                    ui.horizontal(|ui| {
                        ui.label(tr("Include"));
                        if ui.add(egui::TextEdit::singleline(&mut inc).desired_width(320.0)).changed() {
                            filter.include = split_patterns(&inc);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(tr("Exclude"));
                        if ui.add(egui::TextEdit::singleline(&mut exc).desired_width(320.0)).changed() {
                            filter.exclude = split_patterns(&exc);
                        }
                    });
                });
            });
        }
    }

    fn general_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        egui::Grid::new("general_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("Folder"));
            let mut folder = self.cfg.folder.clone().unwrap_or_default();
            ui.horizontal(|ui| {
                if ui.add(egui::TextEdit::singleline(&mut folder).hint_text(tr("none")).desired_width(200.0)).changed() {
                    self.cfg.folder = if folder.trim().is_empty() { None } else { Some(folder.trim().trim_matches('/').to_string()) };
                }
                egui::ComboBox::from_id_salt("folders").selected_text("").width(24.0).show_ui(ui, |ui| {
                    for f in cx.conns.folders() {
                        if ui.selectable_label(self.cfg.folder.as_deref() == Some(f), f).clicked() {
                            self.cfg.folder = Some(f.clone());
                        }
                    }
                });
            });
            ui.end_row();
            ui.label(tr("Description"));
            let mut d = self.cfg.description.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::multiline(&mut d).desired_rows(2).desired_width(320.0)).changed() {
                self.cfg.description = if d.is_empty() { None } else { Some(d) };
            }
            ui.end_row();
            ui.label(tr("Colour"));
            ui.horizontal(|ui| {
                let mut use_color = self.cfg.color.is_some();
                if ui.checkbox(&mut use_color, tr("Custom colour")).changed() {
                    self.cfg.color = if use_color { Some([120, 160, 220]) } else { None };
                }
                if let Some(c) = &mut self.cfg.color {
                    ui.color_edit_button_srgb(c);
                }
            });
            ui.end_row();
            ui.label(tr("Navigator view"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.cfg.navigator_view, NavigatorView::Advanced, tr("Advanced"));
                ui.radio_value(&mut self.cfg.navigator_view, NavigatorView::Simple, tr("Simple"));
            });
            ui.end_row();
        });
    }
}

fn split_patterns(s: &str) -> Vec<String> {
    s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

fn auth_label(a: &AuthMethod) -> &'static str {
    match a {
        AuthMethod::Password => tr("Password"),
        AuthMethod::Prompt => tr("Ask every time"),
        AuthMethod::Pgpass => tr("pgpass file"),
        AuthMethod::Command { .. } => tr("Shell command"),
        AuthMethod::None => tr("No authentication"),
    }
}

fn host_key_label(p: HostKeyPolicy) -> &'static str {
    match p {
        HostKeyPolicy::AcceptNew => tr("Trust on first use"),
        HostKeyPolicy::Strict => tr("Only known hosts"),
        HostKeyPolicy::AcceptAll => tr("Accept any key (insecure)"),
    }
}

fn default_key_path() -> String {
    std::env::var("HOME").map(|h| format!("{h}/.ssh/id_ed25519")).unwrap_or_default()
}

/// SSH tunnel settings form (connection dialog and network profiles).
pub fn ssh_form(ui: &mut Ui, id: &str, ssh: &mut SshConfig, password: &mut String, passphrase: &mut String) {
    ui.checkbox(&mut ssh.enabled, tr("Use SSH tunnel"));
    ui.add_enabled_ui(ssh.enabled, |ui| {
        egui::Grid::new(id).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("SSH host"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut ssh.host).desired_width(220.0));
                ui.label(tr("Port"));
                ui.add(egui::DragValue::new(&mut ssh.port).range(1..=65535));
            });
            ui.end_row();
            ui.label(tr("User"));
            ui.add(egui::TextEdit::singleline(&mut ssh.user).desired_width(220.0));
            ui.end_row();
            ui.label(tr("Authentication"));
            ui.horizontal(|ui| {
                let is_pw = matches!(ssh.auth, SshAuth::Password);
                let is_key = matches!(ssh.auth, SshAuth::PublicKey { .. });
                let is_agent = matches!(ssh.auth, SshAuth::Agent);
                if ui.radio(is_pw, tr("Password")).clicked() {
                    ssh.auth = SshAuth::Password;
                }
                if ui.radio(is_key, tr("Private key")).clicked() && !is_key {
                    ssh.auth = SshAuth::PublicKey { key_path: default_key_path() };
                }
                if ui.radio(is_agent, tr("SSH agent")).clicked() {
                    ssh.auth = SshAuth::Agent;
                }
            });
            ui.end_row();
            match &mut ssh.auth {
                SshAuth::Password => {
                    ui.label(tr("Password"));
                    ui.add(egui::TextEdit::singleline(password).password(true).desired_width(220.0));
                    ui.end_row();
                }
                SshAuth::PublicKey { key_path } => {
                    ui.label(tr("Private key file"));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(key_path).desired_width(260.0));
                        if ui.button(dbiji_icons::ic(icon::FOLDER_OPEN)).clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_file()
                        {
                            *key_path = p.display().to_string();
                        }
                    });
                    ui.end_row();
                    ui.label(tr("Passphrase"));
                    ui.add(egui::TextEdit::singleline(passphrase).password(true).desired_width(220.0));
                    ui.end_row();
                }
                SshAuth::Agent => {}
            }
            ui.label("");
            ui.checkbox(&mut ssh.save_credentials, tr("Save credentials"));
            ui.end_row();
            ui.label(tr("Local port"));
            let mut lp = ssh.local_port.map(|p| p.to_string()).unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut lp).hint_text(tr("automatic")).desired_width(80.0)).changed() {
                ssh.local_port = lp.trim().parse().ok();
            }
            ui.end_row();
            ui.label(tr("Keep-alive (s)"));
            ui.add(egui::DragValue::new(&mut ssh.keepalive_secs).range(0..=3600));
            ui.end_row();
            ui.label(tr("Host key check"));
            egui::ComboBox::from_id_salt((id, "hostkey")).selected_text(host_key_label(ssh.host_key_policy)).show_ui(ui, |ui| {
                for p in [HostKeyPolicy::AcceptNew, HostKeyPolicy::Strict, HostKeyPolicy::AcceptAll] {
                    ui.selectable_value(&mut ssh.host_key_policy, p, host_key_label(p));
                }
            });
            ui.end_row();
        });
    });
}

/// Proxy settings form (connection dialog and network profiles).
pub fn proxy_form(ui: &mut Ui, id: &str, proxy: &mut ProxyConfig, password: &mut String) {
    ui.checkbox(&mut proxy.enabled, tr("Use proxy"));
    ui.add_enabled_ui(proxy.enabled, |ui| {
        egui::Grid::new(id).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tr("Type"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut proxy.kind, ProxyKind::Socks5, "SOCKS5");
                ui.radio_value(&mut proxy.kind, ProxyKind::Http, "HTTP CONNECT");
            });
            ui.end_row();
            ui.label(tr("Proxy host"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut proxy.host).desired_width(220.0));
                ui.label(tr("Port"));
                ui.add(egui::DragValue::new(&mut proxy.port).range(1..=65535));
            });
            ui.end_row();
            ui.label(tr("User"));
            let mut user = proxy.user.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut user).desired_width(220.0)).changed() {
                proxy.user = if user.is_empty() { None } else { Some(user) };
            }
            ui.end_row();
            ui.label(tr("Password"));
            ui.add(egui::TextEdit::singleline(password).password(true).desired_width(220.0));
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut proxy.save_credentials, tr("Save credentials"));
            ui.end_row();
        });
    });
}
