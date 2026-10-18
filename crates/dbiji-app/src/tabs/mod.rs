//! Central tab area: SQL editors, object editors, diagrams, admin views ...
//! Tabs live in an `egui_dock` tree so they can be moved, split and detached.

pub mod admin;
pub mod er;
pub mod history;
pub mod object;
pub mod settings;
pub mod welcome;

use crate::cx::AppContext;
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

    /// Stable key used to focus an existing tab instead of opening a duplicate
    /// (e.g. "object:<conn>:<schema>.<table>").
    fn dedupe_key(&self) -> Option<String> {
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
}

impl Tab {
    pub fn new(id: u64, view: Box<dyn TabView>) -> Self {
        Tab { id, view }
    }
}

/// egui_dock adapter that renders tabs with access to the application context.
pub struct DockViewer<'a, 'b> {
    pub cx: &'a mut AppContext<'b>,
    /// Tab ids that the user asked to close but which vetoed (dirty).
    pub close_vetoed: Vec<u64>,
    pub active_tab: Option<u64>,
}

impl egui_dock::TabViewer for DockViewer<'_, '_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> Id {
        Id::new(("dbiji_tab", tab.id))
    }

    fn title(&mut self, tab: &mut Tab) -> WidgetText {
        let dirty = if tab.view.is_dirty() { "● " } else { "" };
        let mut title = format!("{} {dirty}{}", tab.view.icon(), tab.view.title());
        if title.chars().count() > 48 {
            title = title.chars().take(46).collect::<String>() + "…";
        }
        let text = egui::RichText::new(title);
        match tab.view.connection_id().and_then(|c| self.cx.conns.color_of(c)).and_then(crate::theme::connection_marker) {
            Some(color) => text.color(color).into(),
            None => text.into(),
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        self.cx.tab_active = self.active_tab == Some(tab.id);
        tab.view.ui(ui, self.cx);
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        if let Some(t) = tab.view.tooltip() {
            response.clone().on_hover_text(t);
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
