//! Administration views (FR-A1): session manager, lock manager and the
//! server health dashboard. Each view uses its own database session so that
//! a slow catalog query never blocks the navigator or an editor.

use crate::backend::{Job, Loadable};
use crate::command::AppCommand;
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::admin::{KillMode, LockList, Metric, SessionList, TabularData};
use dbiji_core::value::FormatOptions;
use dbiji_core::{DbResult, tr, trf};
use dbiji_visual::dashboard::{DashboardLayout, DashboardState, show_dashboard};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::time::{Duration, Instant};

/// A dedicated session of a connection, opened on first use.
#[derive(Default)]
pub struct AdminSession {
    session: Loadable<SessionHandle>,
    runtime: Option<tokio::runtime::Handle>,
}

impl AdminSession {
    /// The session once open; `None` while connecting (shows a spinner) or
    /// when the connection is closed (shows a connect button).
    pub fn get(&mut self, ui: &mut Ui, cx: &mut AppContext, conn: &str, label: &str) -> Option<SessionHandle> {
        self.get_in(ui, cx, conn, label, None)
    }

    /// The session, connected to `database` (another database of the same
    /// server, e.g. a table of a database shown in the navigator).
    pub fn get_in(&mut self, ui: &mut Ui, cx: &mut AppContext, conn: &str, label: &str, database: Option<&str>) -> Option<SessionHandle> {
        self.session.poll();
        if let Loadable::Loaded(s) = &self.session {
            if !s.is_closed() {
                return Some(s.clone());
            }
            self.session = Loadable::NotLoaded;
        }
        let Some(live) = cx.conns.live(conn) else {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(tr("The connection is closed."));
                if ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked() {
                    cx.commands.push(AppCommand::Connect(conn.to_string()));
                }
            });
            return None;
        };
        match &self.session {
            Loadable::NotLoaded => {
                self.runtime = Some(cx.backend.handle());
                let database = database.filter(|_| live.multiple_databases()).map(str::to_string);
                self.session = Loadable::Loading(live.open_session_job(cx.backend, format!("{} ({label})", live.config.name), database));
            }
            Loadable::Failed(e) => {
                let text = e.full_text();
                crate::widgets::error_box(ui, &text);
                if ui.button(tr("Retry")).clicked() {
                    self.session = Loadable::NotLoaded;
                }
                return None;
            }
            _ => {}
        }
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(tr("Opening a session…"));
        });
        None
    }
}

impl Drop for AdminSession {
    fn drop(&mut self) {
        if let (Loadable::Loaded(s), Some(rt)) = (&self.session, &self.runtime) {
            let s = s.clone();
            rt.spawn(async move { s.close().await });
        }
    }
}

fn cells(data: &TabularData, fmt: &FormatOptions) -> Vec<Vec<String>> {
    data.rows.iter().map(|r| r.iter().map(|v| v.display(fmt).into_owned()).collect()).collect()
}

/// Shared refresh controls: manual refresh plus an auto-refresh interval.
struct Refresh {
    interval: u32,
    last: Option<Instant>,
}

impl Refresh {
    fn ui(&mut self, ui: &mut Ui) -> bool {
        let mut now = ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked();
        egui::ComboBox::from_id_salt(ui.next_auto_id())
            .selected_text(if self.interval == 0 { tr("Auto-refresh off").to_string() } else { trf("Every {} s", &[&self.interval]) })
            .show_ui(ui, |ui| {
                for s in [0u32, 1, 2, 5, 10, 30, 60] {
                    let label = if s == 0 { tr("Off").to_string() } else { trf("{} s", &[&s]) };
                    ui.selectable_value(&mut self.interval, s, label);
                }
            });
        if self.interval > 0 {
            let due = self.last.is_none_or(|t| t.elapsed() >= Duration::from_secs(self.interval as u64));
            now |= due;
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        if self.last.is_none() {
            now = true;
        }
        now
    }
}

// --------------------------------------------------------------- sessions

pub struct SessionManagerTab {
    conn: String,
    admin: AdminSession,
    data: Loadable<SessionList>,
    refresh: Refresh,
    selected: Option<usize>,
    filter: String,
    hide_idle: bool,
    kill: Option<(String, KillMode)>,
    kill_job: Option<Job<DbResult<()>>>,
}

impl SessionManagerTab {
    pub fn new(conn: String) -> Self {
        SessionManagerTab {
            conn,
            admin: AdminSession::default(),
            data: Loadable::NotLoaded,
            refresh: Refresh { interval: 5, last: None },
            selected: None,
            filter: String::new(),
            hide_idle: false,
            kill: None,
            kill_job: None,
        }
    }
}

impl TabView for SessionManagerTab {
    fn title(&self) -> String {
        tr("Sessions").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::USERS
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }
    fn dedupe_key(&self) -> Option<String> {
        Some(format!("sessions:{}", self.conn))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let conn = self.conn.clone();
        let Some(session) = self.admin.get(ui, cx, &conn, "sessions") else { return };
        self.data.poll();
        if let Some(j) = self.kill_job.as_mut()
            && let Some(r) = j.poll()
        {
            self.kill_job = None;
            match r {
                Ok(()) => cx.toasts.success(tr("Done")),
                Err(e) => cx.toasts.error(tr("Cannot end the session"), Some(e.full_text())),
            }
            self.refresh.last = None;
        }
        ui.horizontal(|ui| {
            let now = self.refresh.ui(ui);
            if now && !self.data.is_loading() {
                self.refresh.last = Some(Instant::now());
                let s = session.clone();
                self.data = Loadable::Loading(cx.backend.spawn(async move { s.lock().await.list_sessions().await }));
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("Filter…")).desired_width(160.0));
            ui.checkbox(&mut self.hide_idle, tr("Hide idle"));
            ui.separator();
            let sel_id = match (&self.data, self.selected) {
                (Loadable::Loaded(l), Some(i)) => l.data.rows.get(i).and_then(|r| r.get(l.id_column)).map(|v| v.to_text().into_owned()),
                _ => None,
            };
            ui.add_enabled_ui(sel_id.is_some(), |ui| {
                if ui.button((dbiji_icons::ic(icon::STOP), tr("Cancel query"))).clicked() {
                    self.kill = sel_id.clone().map(|i| (i, KillMode::CancelQuery));
                }
                if ui.button((dbiji_icons::ic(icon::X_CIRCLE), tr("Terminate session"))).clicked() {
                    self.kill = sel_id.clone().map(|i| (i, KillMode::Terminate));
                }
            });
        });
        let mut retry = false;
        if crate::widgets::load_state(ui, &self.data, &mut retry) {
            let Loadable::Loaded(list) = &self.data else { return };
            let fmt = cx.settings.results.format.clone();
            let filter = self.filter.to_lowercase();
            let all = cells(&list.data, &fmt);
            let mut index = Vec::new();
            let rows: Vec<Vec<String>> = all
                .into_iter()
                .enumerate()
                .filter(|(_, r)| filter.is_empty() || r.iter().any(|c| c.to_lowercase().contains(&filter)))
                .filter(|(_, r)| {
                    !self.hide_idle || list.state_column.and_then(|c| r.get(c)).is_none_or(|s| !s.to_lowercase().starts_with("idle") && !s.eq_ignore_ascii_case("sleep"))
                })
                .map(|(i, r)| {
                    index.push(i);
                    r
                })
                .collect();
            let headers: Vec<&str> = list.data.columns.iter().map(String::as_str).collect();
            let selected_visible = self.selected.and_then(|s| index.iter().position(|i| *i == s));
            let blocked = list.blocked_by_column.map(|c| rows.iter().filter(|r| r.get(c).is_some_and(|v| !v.is_empty() && v != &fmt.null_text)).count()).unwrap_or(0);
            ui.weak(trf("{} sessions, {} waiting for a lock", &[&rows.len(), &blocked]));
            let r = crate::widgets::simple_table(ui, ("sessions", &self.conn), &headers, &rows, selected_visible);
            if let Some(i) = r.clicked.or(r.context_row) {
                self.selected = index.get(i).copied();
            }
            if let Some(i) = r.double_clicked
                && let (Some(q), Some(row)) = (list.query_column, index.get(i).and_then(|i| list.data.rows.get(*i)))
            {
                let sql = row.get(q).map(|v| v.to_text().into_owned()).unwrap_or_default();
                if !sql.trim().is_empty() {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: None, text: Some(sql) });
                }
            }
        }
        if retry {
            self.refresh.last = None;
        }
        if let Some((id, mode)) = self.kill.clone() {
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new(("kill", &self.conn))).show(ui.ctx(), |ui| {
                ui.heading(if mode == KillMode::Terminate { tr("Terminate session") } else { tr("Cancel query") });
                ui.label(trf("Session {} — continue?", &[&id]));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Yes")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        let s = session.clone();
                        let id = id.clone();
                        self.kill_job = Some(cx.backend.spawn(async move { s.lock().await.kill_session(&id, mode).await }));
                        close = true;
                    }
                    if ui.button(tr("No")).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.kill = None;
            }
        }
    }

    crate::impl_tab_any!();
}

// ------------------------------------------------------------------ locks

pub struct LockManagerTab {
    conn: String,
    admin: AdminSession,
    data: Loadable<LockList>,
    refresh: Refresh,
    only_waiting: bool,
}

impl LockManagerTab {
    pub fn new(conn: String) -> Self {
        LockManagerTab { conn, admin: AdminSession::default(), data: Loadable::NotLoaded, refresh: Refresh { interval: 5, last: None }, only_waiting: false }
    }
}

impl TabView for LockManagerTab {
    fn title(&self) -> String {
        tr("Locks").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::LOCK
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }
    fn dedupe_key(&self) -> Option<String> {
        Some(format!("locks:{}", self.conn))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let conn = self.conn.clone();
        let Some(session) = self.admin.get(ui, cx, &conn, "locks") else { return };
        self.data.poll();
        ui.horizontal(|ui| {
            if self.refresh.ui(ui) && !self.data.is_loading() {
                self.refresh.last = Some(Instant::now());
                let s = session.clone();
                self.data = Loadable::Loading(cx.backend.spawn(async move { s.lock().await.list_locks().await }));
            }
            ui.checkbox(&mut self.only_waiting, tr("Only waiting locks"));
            if ui.button((dbiji_icons::ic(icon::USERS), tr("Sessions"))).clicked() {
                cx.push(AppCommand::OpenSessionManager { connection: self.conn.clone() });
            }
        });
        let mut retry = false;
        if crate::widgets::load_state(ui, &self.data, &mut retry) {
            let Loadable::Loaded(list) = &self.data else { return };
            let fmt = cx.settings.results.format.clone();
            let rows: Vec<Vec<String>> = cells(&list.data, &fmt)
                .into_iter()
                .filter(|r| {
                    !self.only_waiting
                        || list.granted_column.and_then(|c| r.get(c)).is_some_and(|g| matches!(g.to_lowercase().as_str(), "false" | "f" | "0" | "no" | "☐"))
                        || list.blocked_by_column.and_then(|c| r.get(c)).is_some_and(|b| !b.is_empty() && b != &fmt.null_text)
                })
                .collect();
            let headers: Vec<&str> = list.data.columns.iter().map(String::as_str).collect();
            ui.weak(trf("{} locks", &[&rows.len()]));
            crate::widgets::simple_table(ui, ("locks", &self.conn), &headers, &rows, None);
        }
        if retry {
            self.refresh.last = None;
        }
    }

    crate::impl_tab_any!();
}

// -------------------------------------------------------------- dashboard

pub struct DashboardTab {
    conn: String,
    admin: AdminSession,
    state: DashboardState,
    layout: DashboardLayout,
    job: Option<Job<DbResult<Vec<Metric>>>>,
    started: Instant,
    last: Option<Instant>,
    interval: Duration,
    error: Option<String>,
    paused: bool,
}

impl DashboardTab {
    pub fn new(conn: String) -> Self {
        let layout = DashboardLayout { id: format!("dash-{conn}"), ..Default::default() };
        DashboardTab {
            conn,
            admin: AdminSession::default(),
            state: DashboardState::new(),
            layout,
            job: None,
            started: Instant::now(),
            last: None,
            interval: Duration::from_secs(1),
            error: None,
            paused: false,
        }
    }
}

impl TabView for DashboardTab {
    fn title(&self) -> String {
        tr("Dashboard").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::CHART_LINE
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }
    fn dedupe_key(&self) -> Option<String> {
        Some(format!("dashboard:{}", self.conn))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let conn = self.conn.clone();
        let Some(session) = self.admin.get(ui, cx, &conn, "dashboard") else { return };
        if let Some(j) = self.job.as_mut()
            && let Some(r) = j.poll()
        {
            self.job = None;
            match r {
                Ok(m) => {
                    self.state.push(self.started.elapsed().as_secs_f64(), &m);
                    self.error = None;
                }
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        let due = self.last.is_none_or(|t| t.elapsed() >= self.interval);
        if due && self.job.is_none() && !self.paused && ui.is_visible() {
            self.last = Some(Instant::now());
            let s = session.clone();
            self.job = Some(cx.backend.spawn(async move { s.lock().await.server_metrics().await }));
        }
        ui.horizontal(|ui| {
            ui.label(tr("Sample every"));
            let mut secs = self.interval.as_secs_f32();
            if ui.add(egui::DragValue::new(&mut secs).range(0.5..=60.0).speed(0.1).suffix(" s")).changed() {
                self.interval = Duration::from_secs_f32(secs);
            }
        });
        if let Some(e) = &self.error {
            crate::widgets::error_box(ui, e);
        }
        let r = show_dashboard(ui, &self.state, &self.layout);
        self.paused = r.paused;
        if !self.paused {
            ui.ctx().request_repaint_after(self.interval.saturating_sub(self.last.map(|t| t.elapsed()).unwrap_or_default()));
        }
    }

    crate::impl_tab_any!();
}
