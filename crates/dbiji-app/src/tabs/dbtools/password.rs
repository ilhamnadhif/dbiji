//! Change the password of the connection user. The statement comes from the
//! dialect (PostgreSQL sends a SCRAM verifier, never the clear text) and runs
//! on a separate short-lived session, so it never appears in a transaction
//! log or the query history. Passwords are never logged or displayed; the
//! saved password is updated when the connection stores it.

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::config::{AuthMethod, ConnectionConfig, SecretSlot, secret_key};
use dbiji_core::driver::TransactionMode;
use dbiji_core::{DbError, DbResult, Secret, tr};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Changed {
    saved_updated: bool,
    saved_error: Option<String>,
}

/// Change-password tool of a connection.
pub struct ChangePasswordTab {
    conn: String,
    new_password: String,
    confirm: String,
    show: bool,
    update_saved: bool,
    job: Option<Job<Result<Changed, String>>>,
    outcome: Option<Result<Changed, String>>,
}

/// The connection keeps its password in the secret store.
fn saves_password(cfg: &ConnectionConfig) -> bool {
    cfg.save_password && matches!(cfg.auth, AuthMethod::Password)
}

/// Remove every occurrence of the secrets from a (server) message.
fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    let mut list: Vec<&str> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    // longest first, so that a statement is removed before the password inside it
    list.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in list {
        out = out.replace(s, "***");
    }
    out
}

impl ChangePasswordTab {
    pub fn new(conn: String) -> Self {
        ChangePasswordTab { conn, new_password: String::new(), confirm: String::new(), show: false, update_saved: true, job: None, outcome: None }
    }

    /// Enter the new password (both fields), as typed by the user.
    pub fn set_new_password(&mut self, password: &str) {
        self.new_password = password.to_string();
        self.confirm = password.to_string();
    }

    /// The change is in progress.
    pub fn is_running(&self) -> bool {
        self.job.is_some()
    }

    /// `Some(Ok(saved password updated))` after a successful change, `Some(Err(message))` after a failure.
    pub fn outcome(&self) -> Option<Result<bool, String>> {
        self.outcome.as_ref().map(|o| o.as_ref().map(|c| c.saved_updated).map_err(|e| e.clone()))
    }

    fn problem(&self, cx: &AppContext) -> Option<String> {
        let live = cx.conns.live(&self.conn)?;
        if live.config.read_only {
            return Some(tr("The connection is read-only.").to_string());
        }
        if live.dialect.change_password_sql(None, "x").is_none() {
            return Some(tr("Changing the password is not supported for this database.").to_string());
        }
        if self.new_password.is_empty() {
            return Some(tr("Enter the new password.").to_string());
        }
        if self.new_password != self.confirm {
            return Some(tr("The passwords do not match.").to_string());
        }
        None
    }

    /// Change the password (the tab must be connected and the fields valid).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.job.is_some() || self.problem(cx).is_some() {
            return;
        }
        let Some(live) = cx.conns.live(&self.conn) else { return };
        let Some(sql) = live.dialect.change_password_sql(None, &self.new_password) else { return };
        let driver = live.driver.clone();
        let params = live.params().clone();
        let cfg = live.config.clone();
        let save = self.update_saved && saves_password(&cfg);
        let secrets = cx.conns.secrets.clone();
        let key = secret_key(&self.conn, SecretSlot::Password);
        let secret = Secret::new(self.new_password.clone());
        let literal = live.dialect.string_literal(&self.new_password);
        let password = self.new_password.clone();
        self.outcome = None;
        self.job = Some(cx.backend.spawn(async move {
            let changed: DbResult<()> = async {
                let mut s = driver.connect(&params).await?;
                let r = async {
                    crate::conn::init_session(s.as_mut(), &cfg, &params).await?;
                    dbiji_core::driver::execute_update(s.as_mut(), &sql, &[]).await?;
                    if s.transaction_mode() == TransactionMode::Manual {
                        s.commit().await?;
                    }
                    Ok::<(), DbError>(())
                }
                .await;
                let _ = s.close().await;
                r
            }
            .await;
            if let Err(e) = changed {
                return Err(scrub(&e.full_text(), &[&sql, &literal, &password]));
            }
            let mut out = Changed { saved_updated: false, saved_error: None };
            if save {
                match tokio::task::spawn_blocking(move || secrets.set(&key, &secret)).await {
                    Ok(Ok(())) => out.saved_updated = true,
                    Ok(Err(e)) => out.saved_error = Some(scrub(&e.full_text(), &[&password])),
                    Err(e) => out.saved_error = Some(e.to_string()),
                }
            }
            Ok(out)
        }));
    }

    fn poll(&mut self) {
        if let Some(r) = self.job.as_mut().and_then(|j| j.poll()) {
            self.job = None;
            if r.is_ok() {
                self.new_password.clear();
                self.confirm.clear();
            }
            self.outcome = Some(r);
        }
    }
}

impl TabView for ChangePasswordTab {
    fn popup_size(&self) -> Option<egui::Vec2> {
        Some(egui::vec2(520.0, 380.0))
    }

    fn title(&self) -> String {
        tr("Change password").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::PASSWORD
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }
    fn dedupe_key(&self) -> Option<String> {
        Some(format!("password:{}", self.conn))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll();
        let Some(cfg) = cx.conns.config(&self.conn).cloned() else {
            crate::widgets::error_box(ui, tr("The connection does not exist."));
            return;
        };
        ui.heading(tr("Change password"));
        egui::Grid::new(("pw_info", &self.conn)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Connection"));
            ui.label(RichText::new(&cfg.name).strong());
            ui.end_row();
            ui.label(tr("User"));
            ui.label(cfg.user.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| tr("(current user)").to_string()));
            ui.end_row();
        });
        ui.add_space(6.0);
        if !cx.conns.is_connected(&self.conn) {
            ui.horizontal(|ui| {
                ui.label(tr("Connect first: the password is changed by the server."));
                if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                    cx.push(AppCommand::Connect(self.conn.clone()));
                }
            });
            return;
        }
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            egui::Grid::new(("pw_fields", &self.conn)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr("New password"));
                ui.add(egui::TextEdit::singleline(&mut self.new_password).password(!self.show).desired_width(240.0));
                ui.end_row();
                ui.label(tr("Confirm password"));
                ui.add(egui::TextEdit::singleline(&mut self.confirm).password(!self.show).desired_width(240.0));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.show, tr("Show the password"));
                ui.end_row();
            });
            if saves_password(&cfg) {
                ui.checkbox(&mut self.update_saved, tr("Update the saved password of the connection"));
            } else {
                ui.weak(tr("The connection does not save its password: enter the new password when you connect next time."));
            }
        });
        let problem = self.problem(cx);
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy && problem.is_none(), egui::Button::new(format!("{} {}", icon::KEY, tr("Change password")))).clicked() {
                self.start(cx);
            }
            if busy {
                ui.spinner();
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            } else if let Some(p) = &problem
                && (!self.new_password.is_empty() || !self.confirm.is_empty() || p != tr("Enter the new password."))
            {
                ui.label(RichText::new(p).color(crate::theme::warning_color(ui.visuals().dark_mode)));
            }
        });
        let dark = ui.visuals().dark_mode;
        match &self.outcome {
            Some(Ok(c)) => {
                ui.label(RichText::new(format!("{} {}", icon::CHECK_CIRCLE, tr("The password was changed."))).color(crate::theme::success_color(dark)));
                if c.saved_updated {
                    ui.label(tr("The saved password was updated."));
                }
                if let Some(e) = &c.saved_error {
                    crate::widgets::error_box(ui, &format!("{}\n{e}", tr("The saved password could not be updated.")));
                }
                ui.horizontal(|ui| {
                    ui.label(tr("Reconnect so that new sessions use the new password."));
                    if ui.button(format!("{} {}", icon::ARROW_CLOCKWISE, tr("Reconnect"))).clicked() {
                        cx.push(AppCommand::Reconnect(self.conn.clone()));
                    }
                });
            }
            Some(Err(e)) => {
                crate::widgets::error_box(ui, &format!("{}\n{e}", tr("The password could not be changed.")));
            }
            None => {}
        }
    }

    crate::impl_tab_any!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing() {
        let sql = "ALTER USER CURRENT_USER() IDENTIFIED BY 's3cret'";
        let msg = format!("You have an error near '{sql}' and s3cret");
        let out = scrub(&msg, &[sql, "'s3cret'", "s3cret"]);
        assert!(!out.contains("s3cret"), "{out}");
        assert_eq!(scrub("nothing", &["", "x"]), "nothing");
    }
}
