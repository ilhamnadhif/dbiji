//! Pending transactions: every session with an open transaction (SQL
//! editors in manual commit mode, the navigator / data editor session of a
//! connection), with commit and rollback per session or for all.

use crate::backend::{Backend, Job};
use crate::conn::ConnectionManager;
use crate::conn::session::SessionHandle;
use crate::tabs::Tab;
use crate::toasts::Toasts;
use dbiji_core::{DbResult, tr, trf};
use egui::RichText;
use egui_dock::DockState;
use egui_phosphor::regular as icon;

/// One session with uncommitted work.
#[derive(Clone)]
pub struct PendingTx {
    pub conn: String,
    pub conn_name: String,
    /// The tab (or "Navigator / data editor") owning the session.
    pub owner: String,
    pub session: SessionHandle,
    /// Statements executed in the transaction.
    pub statements: usize,
    pub since: Option<chrono::DateTime<chrono::Local>>,
}

/// The sessions with an open transaction.
pub fn collect(dock: &DockState<Tab>, conns: &ConnectionManager) -> Vec<PendingTx> {
    let mut out: Vec<PendingTx> = Vec::new();
    let mut push = |conn: &str, owner: String, s: SessionHandle| {
        if s.is_closed() || out.iter().any(|p| p.session.same(&s)) {
            return;
        }
        let tx = s.tx_state();
        if !tx.in_transaction {
            return;
        }
        let conn_name = conns.config(conn).map(|c| c.name.clone()).unwrap_or_else(|| conn.to_string());
        out.push(PendingTx { conn: conn.to_string(), conn_name, owner, statements: tx.log.len(), since: tx.log.first().map(|e| e.at), session: s });
    };
    for (_, tab) in dock.iter_all_tabs() {
        if let (Some(conn), Some(s)) = (tab.view.connection_id(), tab.view.transaction_session()) {
            push(conn, tab.view.title(), s);
        }
    }
    let mut ids = conns.live_ids();
    ids.sort();
    for id in ids {
        let Some(live) = conns.live(&id) else { continue };
        push(&id, tr("Navigator / data editor").to_string(), live.main.clone());
        for (db, s) in &live.db_sessions {
            if let Some(s) = s.get() {
                push(&id, format!("{} ({db})", tr("Navigator / data editor")), s.clone());
            }
        }
    }
    out
}

#[derive(Default)]
pub struct PendingTxDialog {
    pub open: bool,
    jobs: Vec<(String, bool, Job<DbResult<()>>)>,
}

impl PendingTxDialog {
    /// A commit or rollback is running.
    pub fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    fn end(&mut self, backend: &Backend, p: &PendingTx, commit: bool) {
        let s = p.session.clone();
        let job = backend.spawn(async move { if commit { s.commit().await } else { s.rollback().await } });
        self.jobs.push((p.conn_name.clone(), commit, job));
    }

    /// Finish the commits and rollbacks that are done (errors become toasts).
    pub fn poll(&mut self, toasts: &mut Toasts) {
        self.jobs.retain_mut(|(name, commit, job)| match job.poll() {
            Some(Ok(())) => false,
            Some(Err(e)) => {
                let title = if *commit { trf("Commit failed: {}", &[name]) } else { trf("Rollback failed: {}", &[name]) };
                toasts.error(title, Some(e.full_text()));
                false
            }
            None => true,
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, backend: &Backend, items: &[PendingTx]) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut action: Option<(Option<usize>, bool)> = None;
        egui::Window::new(format!("{} {}", icon::CLOCK_COUNTDOWN, tr("Pending transactions")))
            .id(egui::Id::new("pending_transactions"))
            .open(&mut open)
            .collapsible(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                if items.is_empty() {
                    ui.weak(tr("No connection has uncommitted changes."));
                    return;
                }
                egui::Grid::new("pending_tx_grid").num_columns(5).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                    for h in [tr("Connection"), tr("Opened by"), tr("Statements"), tr("Since"), ""] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for (i, p) in items.iter().enumerate() {
                        ui.label(&p.conn_name);
                        ui.label(&p.owner);
                        ui.label(p.statements.to_string());
                        ui.label(p.since.map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_else(|| "—".into()));
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(!self.busy(), |ui| {
                                if ui.button(format!("{} {}", icon::CHECK_CIRCLE, tr("Commit"))).clicked() {
                                    action = Some((Some(i), true));
                                }
                                if ui.button(format!("{} {}", icon::ARROW_U_UP_LEFT, tr("Rollback"))).clicked() {
                                    action = Some((Some(i), false));
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.busy(), |ui| {
                        if ui.button(tr("Commit all")).clicked() {
                            action = Some((None, true));
                        }
                        if ui.button(tr("Rollback all")).clicked() {
                            action = Some((None, false));
                        }
                    });
                    if self.busy() {
                        ui.spinner();
                    }
                });
            });
        if let Some((which, commit)) = action {
            match which {
                Some(i) => self.end(backend, &items[i], commit),
                None => {
                    for p in items {
                        self.end(backend, p, commit);
                    }
                }
            }
        }
        if !self.jobs.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        self.open = open;
    }
}
