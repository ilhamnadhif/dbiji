//! Import connections from DBeaver, `~/.pgpass` or a connection URL (FR-C8).

use crate::cx::AppContext;
use dbiji_config::ImportResult;
use dbiji_config::import::{DuplicatePolicy, merge_into_store};
use dbiji_core::{DbResult, tr, trf};
use egui::RichText;
use egui_phosphor::regular as icon;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Dbeaver,
    Pgpass,
    Url,
}

pub struct ImportDialog {
    source: Source,
    workspaces: Vec<PathBuf>,
    workspace: Option<PathBuf>,
    pgpass_path: String,
    pgpass_store_passwords: bool,
    url: String,
    url_name: String,
    result: Option<ImportResult>,
    selected: Vec<bool>,
    policy: DuplicatePolicy,
    keep_shell_commands: bool,
    folder_prefix: String,
    error: Option<String>,
}

impl Default for ImportDialog {
    fn default() -> Self {
        let workspaces = dbiji_config::import::dbeaver::find_workspaces();
        let pgpass_path = dbiji_config::import::pgpass::PgPass::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        ImportDialog {
            source: Source::Dbeaver,
            workspace: workspaces.first().cloned(),
            workspaces,
            pgpass_path,
            pgpass_store_passwords: false,
            url: String::new(),
            url_name: String::new(),
            result: None,
            selected: Vec::new(),
            policy: DuplicatePolicy::Skip,
            keep_shell_commands: false,
            folder_prefix: String::new(),
            error: None,
        }
    }
}

impl ImportDialog {
    fn load(&mut self) {
        self.error = None;
        let res: DbResult<ImportResult> = match self.source {
            Source::Dbeaver => match &self.workspace {
                Some(ws) => dbiji_config::import::dbeaver::import_workspace(ws),
                None => Err(dbiji_core::DbError::config(tr("Choose a DBeaver workspace folder."))),
            },
            Source::Pgpass => dbiji_config::import::pgpass::import_pgpass_with(std::path::Path::new(self.pgpass_path.trim()), self.pgpass_store_passwords),
            Source::Url => dbiji_config::parse_connection_url(self.url.trim()).map(|(mut cfg, password)| {
                if !self.url_name.trim().is_empty() {
                    cfg.name = self.url_name.trim().to_string();
                }
                let mut r = ImportResult::default();
                if let Some(pw) = password {
                    cfg.save_password = true;
                    r.secrets.push((dbiji_core::config::secret_key(&cfg.id, dbiji_core::config::SecretSlot::Password), pw));
                }
                r.connections.push(cfg);
                r
            }),
        };
        match res {
            Ok(r) => {
                self.selected = vec![true; r.connections.len()];
                self.result = Some(r);
            }
            Err(e) => {
                self.result = None;
                self.error = Some(e.full_text());
            }
        }
    }

    fn apply(&mut self, cx: &mut AppContext) -> bool {
        let Some(mut result) = self.result.take() else { return false };
        let mut keep = self.selected.iter();
        result.connections.retain(|_| *keep.next().unwrap_or(&false));
        if !self.keep_shell_commands {
            for c in &mut result.connections {
                c.init.shell_before_connect = None;
                c.init.shell_after_disconnect = None;
            }
        }
        if !self.folder_prefix.trim().is_empty() {
            result.prefix_folders(self.folder_prefix.trim());
        }
        let warnings = result.warnings.clone();
        let summary = merge_into_store(&mut cx.conns.store, result, self.policy);
        if let Err(e) = cx.services.secrets.read().store_all(&summary.secrets) {
            cx.toasts.error(tr("Cannot store the imported passwords"), Some(e.to_string()));
        }
        cx.conns.dirty = true;
        let detail = if warnings.is_empty() { None } else { Some(warnings.join("\n")) };
        cx.toasts.success(trf(
            "Imported {} connection(s), replaced {}, skipped {}",
            &[&summary.added, &summary.replaced, &summary.skipped],
        ));
        if let Some(d) = detail {
            cx.toasts.warning(tr("Import notes"), Some(d));
        }
        true
    }

    /// Draw the dialog; returns false when it was closed.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext) -> bool {
        let mut open = true;
        let mut done = false;
        egui::Window::new(format!("{} {}", icon::DOWNLOAD_SIMPLE, tr("Import connections")))
            .open(&mut open)
            .collapsible(false)
            .default_size([640.0, 480.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (s, label) in [(Source::Dbeaver, "DBeaver"), (Source::Pgpass, ".pgpass"), (Source::Url, "URL")] {
                        if ui.selectable_label(self.source == s, label).clicked() && self.source != s {
                            self.source = s;
                            self.result = None;
                            self.error = None;
                        }
                    }
                });
                ui.separator();
                match self.source {
                    Source::Dbeaver => {
                        ui.label(tr("Workspace folder (contains General/.dbeaver/data-sources.json):"));
                        ui.horizontal(|ui| {
                            let current = self.workspace.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| tr("(none found)").to_string());
                            egui::ComboBox::from_id_salt("dbeaver_ws").selected_text(current).width(420.0).show_ui(ui, |ui| {
                                for w in &self.workspaces {
                                    ui.selectable_value(&mut self.workspace, Some(w.clone()), w.display().to_string());
                                }
                            });
                            if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Browse…"))).clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                    self.workspaces.push(p.clone());
                                    self.workspace = Some(p);
                                }
                            }
                        });
                        ui.weak(tr("Passwords saved by DBeaver with the default key are imported into the secure store."));
                    }
                    Source::Pgpass => {
                        ui.horizontal(|ui| {
                            ui.label(tr("File"));
                            ui.add(egui::TextEdit::singleline(&mut self.pgpass_path).desired_width(380.0));
                            if ui.button(format!("{} {}", icon::FOLDER_OPEN, tr("Browse…"))).clicked() {
                                if let Some(p) = rfd::FileDialog::new().pick_file() {
                                    self.pgpass_path = p.display().to_string();
                                }
                            }
                        });
                        ui.checkbox(&mut self.pgpass_store_passwords, tr("Copy the passwords into the secure store (otherwise the connections read .pgpass)"));
                    }
                    Source::Url => {
                        egui::Grid::new("import_url").num_columns(2).show(ui, |ui| {
                            ui.label(tr("URL"));
                            ui.add(egui::TextEdit::singleline(&mut self.url).hint_text("postgres://user@host:5432/db").desired_width(420.0));
                            ui.end_row();
                            ui.label(tr("Name"));
                            ui.add(egui::TextEdit::singleline(&mut self.url_name).desired_width(240.0));
                            ui.end_row();
                        });
                        ui.weak(tr("Supported: postgres://, mysql://, mariadb://, sqlserver://, clickhouse://, sqlite:, duckdb:, jdbc:…"));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Find connections"))).clicked() {
                        self.load();
                    }
                });
                if let Some(e) = &self.error {
                    crate::widgets::error_box(ui, e);
                }
                if let Some(r) = &self.result {
                    ui.separator();
                    if r.connections.is_empty() {
                        ui.weak(tr("No connections found."));
                    } else {
                        ui.horizontal(|ui| {
                            ui.strong(trf("{} connection(s) found", &[&r.connections.len()]));
                            if ui.small_button(tr("Select all")).clicked() {
                                self.selected.iter_mut().for_each(|s| *s = true);
                            }
                            if ui.small_button(tr("Select none")).clicked() {
                                self.selected.iter_mut().for_each(|s| *s = false);
                            }
                        });
                        egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                            for (i, c) in r.connections.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if let Some(s) = self.selected.get_mut(i) {
                                        ui.checkbox(s, "");
                                    }
                                    ui.label(format!("{} {}", crate::nav::db_icon(c.kind), c.name));
                                    let target = if c.kind.is_file_based() {
                                        c.file_path.clone().unwrap_or_default()
                                    } else {
                                        format!("{}:{}", c.host, c.effective_port())
                                    };
                                    ui.weak(target);
                                    if let Some(f) = &c.folder {
                                        ui.weak(format!("{} {f}", icon::FOLDER));
                                    }
                                    if c.init.shell_before_connect.is_some() || c.init.shell_after_disconnect.is_some() {
                                        ui.label(RichText::new(icon::TERMINAL).color(crate::theme::warning_color(ui.visuals().dark_mode)))
                                            .on_hover_text(tr("Runs a shell command when connecting or disconnecting."));
                                    }
                                });
                            }
                        });
                        if !r.warnings.is_empty() {
                            egui::CollapsingHeader::new(trf("{} note(s)", &[&r.warnings.len()])).show(ui, |ui| {
                                for w in &r.warnings {
                                    ui.weak(w);
                                }
                            });
                        }
                        egui::Grid::new("import_opts").num_columns(2).show(ui, |ui| {
                            ui.label(tr("Existing connections"));
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.policy, DuplicatePolicy::Skip, tr("Skip"));
                                ui.radio_value(&mut self.policy, DuplicatePolicy::Replace, tr("Replace"));
                                ui.radio_value(&mut self.policy, DuplicatePolicy::KeepBoth, tr("Keep both"));
                            });
                            ui.end_row();
                            ui.label(tr("Put into folder"));
                            ui.add(egui::TextEdit::singleline(&mut self.folder_prefix).hint_text(tr("(top level)")).desired_width(200.0));
                            ui.end_row();
                        });
                        ui.checkbox(&mut self.keep_shell_commands, tr("Keep shell commands that run on connect / disconnect"));
                        ui.add_space(4.0);
                        let n = self.selected.iter().filter(|s| **s).count();
                        if ui.add_enabled(n > 0, egui::Button::new(RichText::new(trf("Import {} connection(s)", &[&n])).strong())).clicked() {
                            done = self.apply(cx);
                        }
                    }
                }
            });
        open && !done
    }
}
