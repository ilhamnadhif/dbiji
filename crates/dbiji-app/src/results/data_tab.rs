//! Table data editor (FR-D1/D2): the data page of the object editor and the
//! tabs opened by foreign key navigation. Uses its own session so scrolling
//! a large table is never interrupted by navigator queries.

use crate::cx::AppContext;
use crate::exec::{ExecEvent, ExecOpts, Execution, StmtPlan};
use crate::results::panel::ResultPanel;
use crate::results::source::ResultSource;
use crate::tabs::admin::AdminSession;
use crate::tabs::{EmbeddedView, TabView};
use dbiji_core::meta::ObjectRef;
use dbiji_core::tr;
use egui::Ui;
use egui_phosphor::regular as icon;

pub struct TableDataView {
    conn: String,
    table: ObjectRef,
    where_clause: Option<String>,
    order_by: Option<String>,
    session: AdminSession,
    exec: Option<Execution>,
    pub panel: Option<ResultPanel>,
    error: Option<String>,
    started: bool,
}

impl TableDataView {
    pub fn new(conn: String, table: ObjectRef, where_clause: Option<String>) -> Self {
        TableDataView { conn, table, where_clause, order_by: None, session: AdminSession::default(), exec: None, panel: None, error: None, started: false }
    }

    /// The error that stopped loading the data, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn start(&mut self, cx: &mut AppContext, session: crate::conn::session::SessionHandle) {
        let Some(live) = cx.conns.live(&self.conn) else { return };
        // The table's default filter / order (virtual model), unless opened with a condition.
        if self.where_clause.is_none() && self.order_by.is_none() {
            let vm = cx.services.projects.as_ref().and_then(|p| p.virtual_model(&self.conn).ok());
            if let Some(vt) = vm.as_ref().and_then(|m| crate::results::panel::vm_lookup(m, &self.table)) {
                self.where_clause = vt.default_filter.clone();
                self.order_by = vt.default_order.clone();
            }
        }
        let sql = live.dialect.select_table(&self.table, None, self.where_clause.as_deref(), self.order_by.as_deref(), None);
        let plan = StmtPlan { index: 0, original: sql.clone(), range: 0..sql.len(), sql, is_write: false, client: None };
        let opts = ExecOpts { fetch_size: cx.settings.results.effective_fetch_size() as usize, store_budget: cx.settings.results.store_budget_bytes(), ..Default::default() };
        self.exec = Some(Execution::start(cx.backend, session, vec![plan], opts, None));
        self.started = true;
    }

    fn poll(&mut self, cx: &mut AppContext) {
        let Some(exec) = self.exec.as_mut() else { return };
        for ev in exec.poll() {
            match ev {
                ExecEvent::Rows { sql, columns, store, token, exhausted, elapsed, .. } => {
                    let budget = cx.settings.results.store_budget_bytes();
                    let mut src = ResultSource::from_executed(exec.session.clone(), sql, Vec::new(), columns, store, token, exhausted, budget);
                    src.exec_duration = elapsed;
                    let mut panel = ResultPanel::new(self.conn.clone(), self.table.name.clone(), src, Some(self.table.clone()));
                    if let Some(w) = &self.where_clause {
                        panel.view.toolbar.filter_text = w.clone();
                    }
                    if let Some(o) = &self.order_by {
                        panel.view.toolbar.order_text = o.clone();
                    }
                    self.panel = Some(panel);
                }
                ExecEvent::Error { error, .. } => self.error = Some(error.full_text()),
                _ => {}
            }
        }
        if exec.is_finished() {
            self.exec = None;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let conn = self.conn.clone();
        // A table of another database of the server needs a session there.
        let database = self.table.database.clone();
        let Some(session) = self.session.get_in(ui, cx, &conn, "data", database.as_deref()) else { return };
        // The tab's session was lost (server restart, idle timeout) and opened again:
        // the grid saves and fetches through the new one instead of failing with "closed".
        if let Some(p) = self.panel.as_mut()
            && !p.src.session.same(&session)
        {
            p.src.session = session.clone();
        }
        if !self.started {
            self.start(cx, session.clone());
        }
        self.poll(cx);
        if let Some(e) = self.error.clone() {
            crate::widgets::error_box(ui, &e);
            if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Retry"))).clicked() {
                self.error = None;
                self.started = false;
            }
            return;
        }
        match self.panel.as_mut() {
            Some(p) => p.ui(ui, cx),
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Reading data…"));
                });
            }
        }
    }
}

impl EmbeddedView for TableDataView {
    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        TableDataView::ui(self, ui, cx);
    }

    fn is_dirty(&self) -> bool {
        self.panel.as_ref().is_some_and(|p| p.has_changes())
    }

    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        use crate::keys::Action as A;
        let Some(p) = self.panel.as_mut() else { return false };
        match action {
            A::SaveData | A::Save => {
                p.save(cx);
                true
            }
            A::CancelDataChanges => {
                p.discard();
                true
            }
            A::Refresh => {
                p.src.refresh(cx.backend, cx.settings.results.effective_fetch_size() as usize);
                true
            }
            A::Export => {
                cx.push(crate::command::AppCommand::OpenExport { connection: self.conn.clone(), objects: vec![self.table.clone()], sql: None });
                true
            }
            _ => false,
        }
    }

    fn focus_context(&self) -> crate::keys::ActionContext {
        crate::keys::ActionContext::ResultsGrid
    }
}

/// Stand-alone tab with a table's data (foreign key navigation, "View data").
pub struct TableDataTab {
    conn: String,
    table: ObjectRef,
    view: TableDataView,
}

impl TableDataTab {
    pub fn new(conn: String, table: ObjectRef, where_clause: Option<String>) -> Self {
        TableDataTab { view: TableDataView::new(conn.clone(), table.clone(), where_clause), conn, table }
    }
}

impl TabView for TableDataTab {
    fn navigator_object(&self) -> Option<(String, ObjectRef)> {
        Some((self.conn.clone(), self.table.clone()))
    }

    fn title(&self) -> String {
        self.view.panel.as_ref().map(|p| p.title.clone()).unwrap_or_else(|| self.table.name.clone())
    }
    fn icon(&self) -> &'static str {
        icon::TABLE
    }
    fn tooltip(&self) -> Option<String> {
        Some(self.table.display_path())
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }
    fn is_dirty(&self) -> bool {
        EmbeddedView::is_dirty(&self.view)
    }
    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.view.ui(ui, cx);
    }
    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        EmbeddedView::on_action(&mut self.view, action, cx)
    }
    fn focus_context(&self) -> crate::keys::ActionContext {
        crate::keys::ActionContext::ResultsGrid
    }
    crate::impl_tab_any!();
}
