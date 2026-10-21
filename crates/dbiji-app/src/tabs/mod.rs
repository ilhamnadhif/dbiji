//! Central tab area: SQL editors, object editors, diagrams, admin views ...
//! Tabs live in an `egui_dock` tree so they can be moved, split and detached.

pub mod admin;
pub mod ai;
pub mod catalog;
pub mod compare;
pub mod data_compare;
pub mod dbtools;
pub mod er;
pub mod errorlog;
pub mod history;
pub mod object;
pub mod query_builder;
pub mod search;
pub mod settings;
pub mod sql_editor;
pub mod table_designer;
pub mod tasks;
pub mod transfer;
pub mod welcome;

use crate::cx::AppContext;
use dbiji_core::SchemaRef;
use egui::{Id, Ui, WidgetText};
use std::any::Any;

/// Behaviour shared by every tab kind.
pub trait TabView: Any {
    /// Title shown on the tab button.
    fn title(&self) -> String;

    /// Phosphor icon glyph.
    fn icon(&self) -> &'static str {
        egui_phosphor::regular::FILE
    }

    /// Tooltip for the tab button.
    fn tooltip(&self) -> Option<String> {
        None
    }

    /// Connection this tab belongs to (status bar, colour marker, palette).
    fn connection_id(&self) -> Option<&str> {
        None
    }

    /// Database and schema the tab works in, as far as it knows them (the
    /// connection's current ones fill the gaps, see [`tab_location`]).
    fn schema_context(&self) -> Option<SchemaRef> {
        None
    }

    /// Stable key used to focus an existing tab instead of opening a duplicate
    /// (e.g. "object:<conn>:<schema>.<table>").
    fn dedupe_key(&self) -> Option<String> {
        None
    }

    /// The database object this tab shows (Link with editor).
    fn navigator_object(&self) -> Option<(String, dbiji_core::ObjectRef)> {
        None
    }

    /// Whether the tab can run `action` now (toolbar buttons are disabled
    /// otherwise); `None` when the tab does not handle the action.
    fn action_enabled(&self, action: crate::keys::Action) -> Option<bool> {
        let _ = action;
        None
    }

    /// The session of the tab's own transaction (Pending transactions).
    fn transaction_session(&self) -> Option<crate::conn::session::SessionHandle> {
        None
    }

    /// Unsaved changes (editor text, pending data edits).
    fn is_dirty(&self) -> bool {
        false
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext);

    /// Handle an action (shortcut, menu, palette) routed to the active tab.
    /// Return true if handled.
    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        let _ = (action, cx);
        false
    }

    /// Which part of the tab had keyboard focus last frame (selects the
    /// shortcut context: SQL editor, results grid ...).
    fn focus_context(&self) -> crate::keys::ActionContext {
        crate::keys::ActionContext::Global
    }

    /// Opens in a pop-up window of this size instead of a tab: wizards and
    /// one-off actions (export, import, backup …).
    fn popup_size(&self) -> Option<egui::Vec2> {
        None
    }

    /// True once after its job finished successfully: a pop-up then closes
    /// (the result was reported by a toast). Failures keep it open.
    fn take_done(&mut self) -> bool {
        false
    }

    /// The tab asks about its unsaved changes itself in [`on_close`](Self::on_close);
    /// for the others the application asks before closing a dirty tab.
    fn prompts_on_close(&self) -> bool {
        false
    }

    /// Called before closing; return false to keep the tab open (e.g. to ask
    /// about unsaved changes — the tab then shows its own confirmation).
    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        let _ = cx;
        true
    }

    /// State persisted to restore the tab on the next start (FR-S1).
    fn session_state(&self) -> Option<serde_json::Value> {
        None
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// A tab in the dock tree.
pub struct Tab {
    pub id: u64,
    pub view: Box<dyn TabView>,
    /// The user chose to discard its unsaved changes: close without asking.
    pub force_close: bool,
}

impl Tab {
    pub fn new(id: u64, view: Box<dyn TabView>) -> Self {
        Tab { id, view, force_close: false }
    }

    /// Closing it would lose unsaved changes the tab does not ask about itself.
    pub fn needs_close_confirm(&self) -> bool {
        !self.force_close && !self.view.prompts_on_close() && self.view.is_dirty()
    }
}

/// egui_dock adapter that renders tabs with access to the application context.
pub struct DockViewer<'a, 'b> {
    pub cx: &'a mut AppContext<'b>,
    /// Tab ids that the user asked to close but which vetoed (dirty).
    pub close_vetoed: Vec<u64>,
    /// Dirty tabs whose closing the application must confirm.
    pub close_confirm: Vec<u64>,
    pub active_tab: Option<u64>,
    /// For the tab titles (a coloured icon and the title).
    pub style: std::sync::Arc<egui::Style>,
}

/// Where a tab works: its connection and database / schema (main toolbar,
/// status bar, tab tooltip).
pub struct TabLocation {
    pub conn: String,
    pub name: String,
    pub kind: dbiji_core::DbKind,
    /// `schema@database` (DBeaver's notation), or the one level the database has.
    pub place: Option<String>,
}

pub fn tab_location(view: &dyn TabView, conns: &crate::conn::ConnectionManager) -> Option<TabLocation> {
    let conn = view.connection_id()?;
    let cfg = conns.config(conn)?;
    let live = conns.live(conn);
    // PostgreSQL, SQL Server: databases hold schemas. Elsewhere one level.
    let two_levels = live.is_some_and(|l| l.multiple_databases());
    let current = live.map(|l| SchemaRef::new(l.current_database.clone(), l.current_schema.clone())).unwrap_or_default();
    let place = place_label(view.schema_context().unwrap_or_default(), &current, two_levels);
    Some(TabLocation { conn: conn.to_string(), name: cfg.name.clone(), kind: cfg.kind, place })
}

/// `schema@database` of `at`, its gaps filled from the session's `current`
/// database / schema; one name where the database has a single level.
fn place_label(mut at: SchemaRef, current: &SchemaRef, two_levels: bool) -> Option<String> {
    if two_levels && at.database.is_none() {
        at.database = current.database.clone();
    }
    if at.schema.is_none() && (!two_levels || at.database == current.database) {
        at.schema = current.schema.clone();
    }
    if !two_levels && at.schema.is_none() {
        at.schema = current.database.clone();
    }
    match (at.schema, at.database) {
        (Some(s), Some(d)) if two_levels && s != d => Some(format!("{s}@{d}")),
        (s, d) => s.or(d),
    }
}

impl egui_dock::TabViewer for DockViewer<'_, '_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> Id {
        Id::new(("dbiji_tab", tab.id))
    }

    fn title(&mut self, tab: &mut Tab) -> WidgetText {
        let dirty = if tab.view.is_dirty() { "● " } else { "" };
        let mut title = format!(" {dirty}{}", tab.view.title());
        if title.chars().count() > 46 {
            title = title.chars().take(44).collect::<String>() + "…";
        }
        let mut text = egui::RichText::new(title);
        if let Some(color) = tab.view.connection_id().and_then(|c| self.cx.conns.color_of(c)).and_then(crate::theme::connection_marker) {
            text = text.color(color);
        }
        let mut job = egui::text::LayoutJob::default();
        let font = egui::FontSelection::Default;
        dbiji_icons::ic(tab.view.icon()).append_to(&mut job, &self.style, font.clone(), egui::Align::Center);
        text.append_to(&mut job, &self.style, font, egui::Align::Center);
        job.into()
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        self.cx.tab_active = self.active_tab == Some(tab.id);
        self.cx.current_tab = Some(tab.id);
        tab.view.ui(ui, self.cx);
        self.cx.current_tab = None;
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        // The tab's own text, then its connection and schema@database.
        let location = tab_location(tab.view.as_ref(), self.cx.conns).map(|l| match l.place {
            Some(p) => format!("{} · {p}", l.name),
            None => l.name,
        });
        let tip: Vec<String> = tab.view.tooltip().into_iter().chain(location).collect();
        if !tip.is_empty() {
            response.clone().on_hover_text(tip.join("\n"));
        }
        if response.middle_clicked() {
            self.cx.commands.push(crate::command::AppCommand::CloseTab(tab.id));
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        use crate::command::AppCommand;
        if ui.button(dbiji_core::tr("Close")).clicked() {
            self.cx.commands.push(AppCommand::CloseTab(tab.id));
            ui.close();
        }
        if ui.button(dbiji_core::tr("Close others")).clicked() {
            self.cx.commands.push(AppCommand::CloseOtherTabs(tab.id));
            ui.close();
        }
        if ui.button(dbiji_core::tr("Close all")).clicked() {
            self.cx.commands.push(AppCommand::CloseAllTabs);
            ui.close();
        }
    }

    fn on_close(&mut self, tab: &mut Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        if tab.needs_close_confirm() {
            self.close_confirm.push(tab.id);
            return egui_dock::tab_viewer::OnCloseResponse::Ignore;
        }
        if tab.view.on_close(self.cx) {
            egui_dock::tab_viewer::OnCloseResponse::Close
        } else {
            self.close_vetoed.push(tab.id);
            egui_dock::tab_viewer::OnCloseResponse::Ignore
        }
    }

    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}

/// Implements the `as_any` boilerplate of [`TabView`].
#[macro_export]
macro_rules! impl_tab_any {
    () => {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    };
}

/// A view embedded inside another tab (e.g. the data page of an object editor).
pub trait EmbeddedView {
    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext);
    fn is_dirty(&self) -> bool {
        false
    }
    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        let _ = (action, cx);
        false
    }
    fn focus_context(&self) -> crate::keys::ActionContext {
        crate::keys::ActionContext::Global
    }
    /// The view's own session (its open transaction is listed in Pending transactions).
    fn transaction_session(&self) -> Option<crate::conn::session::SessionHandle> {
        None
    }
}

/// Data editor for a table or view (filled in by the results module).
pub fn make_table_data_view(cx: &mut AppContext, conn: &str, obj: &dbiji_core::ObjectRef) -> Option<Box<dyn EmbeddedView>> {
    let _ = cx;
    Some(Box::new(crate::results::data_tab::TableDataView::new(conn.to_string(), obj.clone(), None)))
}

/// Read-only highlighted code (DDL, plans, logs).
pub fn code(ui: &mut Ui, cx: &mut AppContext, id: impl std::hash::Hash + std::fmt::Debug, text: &str) {
    let _ = cx;
    crate::widgets::code_view(ui, id, text);
}

#[cfg(test)]
mod tests {
    use super::place_label;
    use dbiji_core::SchemaRef;

    fn at(database: Option<&str>, schema: Option<&str>) -> SchemaRef {
        SchemaRef::new(database.map(Into::into), schema.map(Into::into))
    }

    #[test]
    fn place_as_in_dbeaver() {
        let pg = at(Some("zain_iq"), Some("public"));
        // A table of the current database, of another one, an editor without a schema.
        assert_eq!(place_label(at(None, Some("public")), &pg, true).as_deref(), Some("public@zain_iq"));
        assert_eq!(place_label(at(Some("reports"), Some("sales")), &pg, true).as_deref(), Some("sales@reports"));
        assert_eq!(place_label(at(Some("reports"), None), &pg, true).as_deref(), Some("reports"));
        assert_eq!(place_label(SchemaRef::default(), &pg, true).as_deref(), Some("public@zain_iq"));
        // MySQL: the database is the schema; SQLite: main.
        let mysql = at(Some("shop"), Some("shop"));
        assert_eq!(place_label(at(None, Some("crm")), &mysql, false).as_deref(), Some("crm"));
        assert_eq!(place_label(SchemaRef::default(), &mysql, false).as_deref(), Some("shop"));
        assert_eq!(place_label(at(None, Some("main")), &at(None, None), false).as_deref(), Some("main"));
    }
}
