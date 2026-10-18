//! Master password dialog of the encrypted secret vault (FR-C4).

use crate::backend::Job;
use crate::cx::AppContext;
use dbiji_core::{DbError, DbResult, Secret, tr};
use egui::RichText;
use egui_phosphor::regular as icon;

#[derive(Default)]
pub struct VaultDialog {
    password: String,
    confirm: String,
    job: Option<Job<DbResult<()>>>,
    error: Option<String>,
}

impl VaultDialog {
    /// Draw the dialog; clears `open` when it is done or skipped.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext, open: &mut bool) {
        let vault = cx.services.secrets.read().vault().cloned();
        let Some(vault) = vault else {
            *open = false;
            return;
        };
        if let Some(job) = self.job.as_mut()
            && let Some(res) = job.poll() {
                self.job = None;
                match res {
                    Ok(()) => {
                        self.password.clear();
                        self.confirm.clear();
                        self.error = None;
                        cx.toasts.success(tr("Saved passwords unlocked"));
                        *open = false;
                        return;
                    }
                    Err(e) => self.error = Some(e.message),
                }
            }
        let create = !vault.exists();
        let busy = self.job.is_some();
        let mut submit = false;
        let mut skip = false;
        let modal = egui::Modal::new(egui::Id::new("vault_unlock")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(format!(
                "{} {}",
                icon::LOCK_KEY,
                if create { tr("Create the password vault") } else { tr("Unlock saved passwords") }
            ));
            ui.label(tr("Saved passwords are kept in a file encrypted with your master password."));
            ui.add_space(6.0);
            egui::Grid::new("vault_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label(tr("Master password"));
                let r = ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.password).password(true).desired_width(220.0));
                if !busy && self.password.is_empty() {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !create {
                    submit = true;
                }
                ui.end_row();
                if create {
                    ui.label(tr("Repeat password"));
                    let r = ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.confirm).password(true).desired_width(220.0));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    ui.end_row();
                }
            });
            if let Some(e) = &self.error {
                crate::widgets::error_box(ui, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if busy {
                    ui.spinner();
                    ui.weak(tr("Deriving the key…"));
                } else {
                    let label = if create { tr("Create") } else { tr("Unlock") };
                    if ui.button(RichText::new(label).strong()).clicked() {
                        submit = true;
                    }
                    if ui.button(tr("Skip")).on_hover_text(tr("Passwords will be asked when connecting.")).clicked() {
                        skip = true;
                    }
                }
            });
        });
        if submit && !busy {
            if self.password.is_empty() {
                self.error = Some(tr("Enter the master password.").to_string());
            } else if create && self.password != self.confirm {
                self.error = Some(tr("The passwords do not match.").to_string());
            } else {
                let master = Secret::new(std::mem::take(&mut self.password));
                self.confirm.clear();
                self.error = None;
                self.job = Some(cx.backend.spawn(async move {
                    tokio::task::spawn_blocking(move || if create { vault.create(&master) } else { vault.unlock(&master) })
                        .await
                        .map_err(|e| DbError::internal(e.to_string()))?
                }));
            }
        }
        if skip || (modal.should_close() && !busy) {
            *open = false;
        }
    }
}
