//! Visual query builder tab (FR-S10): drop tables on a canvas, join them,
//! pick columns, conditions, grouping and ordering; the SQL is generated live.

use crate::backend::Loadable;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::tr;
use dbiji_visual::builder::{QbEvent, QbViewState, QueryModel, show_query_builder};
use egui::Ui;
use egui_phosphor::regular as icon;

pub struct QueryBuilderTab {
    conn: String,
    schema: Option<SchemaRef>,
    model: QueryModel,
    state: Option<QbViewState>,
    picker_open: bool,
    picker_filter: String,
    /// Tables chosen in the picker whose definition is still loading.
    pending: Vec<ObjectRef>,
}

impl QueryBuilderTab {
    pub fn new(conn: String, schema: Option<SchemaRef>) -> Self {
        QueryBuilderTab { conn, schema, model: QueryModel::default(), state: None, picker_open: false, picker_filter: String::new(), pending: Vec::new() }
    }

    fn picker(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&self.conn) else { return };
        let mut schema = self.schema.clone().unwrap_or_else(|| SchemaRef::new(live.current_database.clone(), live.current_schema.clone()));
        // A database chosen without a schema: its default one.
        if schema.schema.is_none() && schema.database.is_some() {
            schema.schema = live.current_schema.clone();
        }
        // The schema's own database: another database of the server has its own session.
        let Some(main) = live.ensure_session(backend, schema.database.as_deref()) else {
            ui.weak(tr("Opening a session…"));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        };
        live.meta.request_objects(backend, &main, &schema, ObjectKind::Table);
        live.meta.request_objects(backend, &main, &schema, ObjectKind::View);
        let mut chosen: Option<ObjectRef> = None;
        let mut open = self.picker_open;
        egui::Window::new(format!("{} {}", icon::TABLE, tr("Add table"))).id(egui::Id::new(("qb_picker", &self.conn))).open(&mut open).default_size([320.0, 400.0]).show(
            ui.ctx(),
            |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.picker_filter).hint_text(tr("Filter…")).desired_width(f32::INFINITY));
                let filter = self.picker_filter.to_lowercase();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for kind in [ObjectKind::Table, ObjectKind::View] {
                        match live.meta.objects.get(&(schema.clone(), kind)) {
                            Some(Loadable::Loaded(list)) => {
                                for o in list.iter().filter(|o| filter.is_empty() || o.obj.name.to_lowercase().contains(&filter)) {
                                    let label = format!("{} {}", crate::nav::kind_icon(kind), o.obj.name);
                                    if ui.selectable_label(false, label).double_clicked() {
                                        chosen = Some(o.obj.clone());
                                    }
                                }
                            }
                            Some(Loadable::Failed(e)) => {
                                ui.label(e.message.clone());
                            }
                            _ => {
                                ui.spinner();
                            }
                        }
                    }
                });
                ui.weak(tr("Double-click a table to add it."));
            },
        );
        self.picker_open = open;
        if let Some(obj) = chosen {
            live.meta.request_table_def(backend, &main, &obj);
            self.pending.push(obj);
        }
    }
}

impl TabView for QueryBuilderTab {
    fn title(&self) -> String {
        tr("Query builder").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::GRAPH
    }
    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let Some(live) = cx.conns.live_mut(&self.conn) else {
            ui.label(tr("The connection is closed."));
            if ui.button(tr("Connect")).clicked() {
                cx.commands.push(AppCommand::Connect(self.conn.clone()));
            }
            return;
        };
        let dialect = live.dialect.clone();
        // Tables whose definition arrived.
        let mut still = Vec::new();
        for obj in std::mem::take(&mut self.pending) {
            match live.meta.table_defs.get(&obj) {
                Some(Loadable::Loaded(def)) => {
                    let def = def.clone();
                    self.model.add_table(&def);
                }
                Some(Loadable::Failed(e)) => cx.toasts.error(tr("Cannot read the table"), Some(e.full_text())),
                _ => still.push(obj),
            }
        }
        self.pending = still;
        if !self.pending.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let state = self.state.get_or_insert_with(|| QbViewState::new(dialect));
        let resp = show_query_builder(ui, &mut self.model, state);
        for ev in resp.events {
            match ev {
                QbEvent::AddTableRequested => self.picker_open = true,
                QbEvent::OpenInEditor(sql) => {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: self.schema.clone(), text: Some(sql) });
                }
                // Run: an editor that executes it as soon as its session is open.
                QbEvent::RunQuery(sql) => {
                    let mut tab = crate::tabs::sql_editor::SqlEditorTab::new(Some(self.conn.clone()), self.schema.clone(), sql, None, false, tr("Query builder").to_string());
                    tab.auto_run = true;
                    cx.push(AppCommand::OpenTab(Box::new(tab)));
                }
                QbEvent::Changed => {}
            }
        }
        if self.picker_open {
            self.picker(ui, cx);
        }
    }

    crate::impl_tab_any!();
}
