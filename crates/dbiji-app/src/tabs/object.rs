//! Object editor (FR-N4): properties, data, DDL, dependencies and statistics of
//! a database object.

use crate::backend::{Backend, Loadable};
use crate::command::{AppCommand, ObjectPage};
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::widgets;
use dbiji_core::ddl::TableDef;
use dbiji_core::meta::{Dependency, DependencyDirection, ObjectKind, ObjectRef, Property};
use dbiji_core::{DbResult, tr};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::future::Future;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TablePage {
    Columns,
    Constraints,
    ForeignKeys,
    References,
    Indexes,
    Triggers,
    Partitions,
    VirtualModel,
}

pub struct ObjectEditorTab {
    pub conn: String,
    pub obj: ObjectRef,
    pub page: ObjectPage,
    table_page: TablePage,
    props: Loadable<Vec<Property>>,
    table: Loadable<Arc<TableDef>>,
    ddl: Loadable<String>,
    deps: Loadable<Vec<Dependency>>,
    stats: Loadable<Vec<Property>>,
    selected_row: Option<usize>,
    /// Data page, created on first use.
    pub data: Option<Box<dyn crate::tabs::EmbeddedView>>,
    /// Virtual model of the table being edited (virtual key, dictionary, virtual columns).
    vmodel: Option<dbiji_config::VirtualTable>,
    new_vcol: (String, String),
}

impl ObjectEditorTab {
    pub fn new(conn: String, obj: ObjectRef, page: ObjectPage) -> Self {
        ObjectEditorTab {
            conn,
            obj,
            page,
            table_page: TablePage::Columns,
            props: Loadable::NotLoaded,
            table: Loadable::NotLoaded,
            ddl: Loadable::NotLoaded,
            deps: Loadable::NotLoaded,
            stats: Loadable::NotLoaded,
            selected_row: None,
            data: None,
            vmodel: None,
            new_vcol: (String::new(), String::new()),
        }
    }

    pub fn key(conn: &str, obj: &ObjectRef) -> String {
        format!("object:{conn}:{:?}:{}:{}", obj.kind, obj.database.as_deref().unwrap_or(""), obj.display_path())
    }

    fn session(&self, cx: &mut AppContext) -> Option<SessionHandle> {
        let backend = cx.backend;
        cx.conns.live_mut(&self.conn)?.ensure_session(backend, self.obj.database.as_deref())
    }

    fn load<T: Send + 'static, F, Fut>(slot: &mut Loadable<T>, backend: &Backend, session: Option<SessionHandle>, f: F)
    where
        F: FnOnce(SessionHandle) -> Fut + Send + 'static,
        Fut: Future<Output = DbResult<T>> + Send + 'static,
    {
        slot.poll();
        if slot.is_not_loaded()
            && let Some(s) = session {
                *slot = Loadable::Loading(backend.spawn(async move { f(s).await }));
            }
    }

    fn refresh(&mut self) {
        self.props = Loadable::NotLoaded;
        self.table = Loadable::NotLoaded;
        self.ddl = Loadable::NotLoaded;
        self.deps = Loadable::NotLoaded;
        self.stats = Loadable::NotLoaded;
    }

    fn properties_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let session = self.session(cx);
        let obj = self.obj.clone();
        Self::load(&mut self.props, cx.backend, session.clone(), move |s| async move { s.lock().await.object_properties(&obj).await });
        let is_table = self.obj.kind.is_table_like() || matches!(self.obj.kind, ObjectKind::View | ObjectKind::MaterializedView);
        if is_table {
            let obj = self.obj.clone();
            Self::load(&mut self.table, cx.backend, session, move |s| async move { s.lock().await.table_definition(&obj).await.map(Arc::new) });
        }
        let mut retry = false;
        if !is_table {
            if widgets::load_state(ui, &self.props, &mut retry)
                && let Some(p) = self.props.get() {
                    egui::ScrollArea::vertical().show(ui, |ui| widgets::property_grid(ui, "obj_props", p));
                }
            if retry {
                self.props = Loadable::NotLoaded;
            }
            return;
        }
        // Tables: general header + sub pages
        if let Some(t) = self.table.get().cloned() {
            ui.horizontal_wrapped(|ui| {
                if let Some(c) = &t.comment {
                    ui.label(RichText::new(c).italics());
                    ui.separator();
                }
                if let Some(r) = t.row_estimate.filter(|r| *r >= 0) {
                    ui.weak(format!("{} ~{r}", tr("Rows:")));
                }
                if let Some(s) = t.size_bytes {
                    ui.weak(format!("{} {}", tr("Size:"), widgets::format_bytes(s)));
                }
                for (k, v) in &t.options {
                    ui.weak(format!("{k}: {v}"));
                }
            });
        }
        ui.horizontal(|ui| {
            for (p, label) in [
                (TablePage::Columns, tr("Columns")),
                (TablePage::Constraints, tr("Constraints")),
                (TablePage::ForeignKeys, tr("Foreign keys")),
                (TablePage::References, tr("References")),
                (TablePage::Indexes, tr("Indexes")),
                (TablePage::Triggers, tr("Triggers")),
                (TablePage::Partitions, tr("Partitions")),
                (TablePage::VirtualModel, tr("Virtual model")),
            ] {
                ui.selectable_value(&mut self.table_page, p, label);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.obj.kind.is_table_like() && ui.button(format!("{} {}", icon::PENCIL, tr("Edit table"))).clicked() {
                    cx.push(AppCommand::EditTable { connection: self.conn.clone(), table: self.obj.clone() });
                }
            });
        });
        ui.separator();
        if !widgets::load_state(ui, &self.table, &mut retry) {
            if retry {
                self.table = Loadable::NotLoaded;
            }
            return;
        }
        let Some(t) = self.table.get().cloned() else { return };
        let fmt_cols = |c: &[String]| c.join(", ");
        match self.table_page {
            TablePage::Columns => {
                let rows: Vec<Vec<String>> = t
                    .columns
                    .iter()
                    .map(|c| {
                        vec![
                            c.ordinal.to_string(),
                            format!("{}{}", if t.is_pk_column(&c.name) { "🔑 " } else { "" }, c.name),
                            c.data_type.clone(),
                            if c.nullable { String::new() } else { "NOT NULL".into() },
                            c.generated.as_ref().map(|g| format!("AS ({g})")).or_else(|| c.default.clone()).unwrap_or_default(),
                            if c.auto_increment { "✓".into() } else { String::new() },
                            c.collation.clone().unwrap_or_default(),
                            c.comment.clone().unwrap_or_default(),
                        ]
                    })
                    .collect();
                let r = widgets::simple_table(
                    ui,
                    "obj_cols",
                    &["#", tr("Name"), tr("Type"), tr("Null"), tr("Default"), tr("Auto"), tr("Collation"), tr("Comment")],
                    &rows,
                    self.selected_row,
                );
                if let Some(i) = r.clicked {
                    self.selected_row = Some(i);
                }
            }
            TablePage::Constraints => {
                let mut rows = Vec::new();
                if let Some(pk) = &t.primary_key {
                    rows.push(vec![pk.name.clone().unwrap_or_default(), "PRIMARY KEY".into(), fmt_cols(&pk.columns)]);
                }
                for u in &t.unique_keys {
                    rows.push(vec![u.name.clone().unwrap_or_default(), "UNIQUE".into(), fmt_cols(&u.columns)]);
                }
                for c in &t.checks {
                    rows.push(vec![c.name.clone().unwrap_or_default(), "CHECK".into(), c.expression.clone()]);
                }
                widgets::simple_table(ui, "obj_cons", &[tr("Name"), tr("Type"), tr("Definition")], &rows, None);
            }
            TablePage::ForeignKeys | TablePage::References => {
                let list = if self.table_page == TablePage::ForeignKeys { &t.foreign_keys } else { &t.referenced_by };
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|fk| {
                        vec![
                            fk.name.clone().unwrap_or_default(),
                            if self.table_page == TablePage::References { format!("{}.{}", fk.schema.clone().unwrap_or_default(), fk.table) } else { String::new() },
                            fmt_cols(&fk.columns),
                            format!("{}{}", fk.ref_schema.as_ref().map(|s| format!("{s}.")).unwrap_or_default(), fk.ref_table),
                            fmt_cols(&fk.ref_columns),
                            fk.on_update.sql().to_string(),
                            fk.on_delete.sql().to_string(),
                        ]
                    })
                    .collect();
                let r = widgets::simple_table(
                    ui,
                    "obj_fks",
                    &[tr("Name"), tr("Table"), tr("Columns"), tr("References"), tr("Referenced columns"), tr("On update"), tr("On delete")],
                    &rows,
                    None,
                );
                if let Some(i) = r.double_clicked
                    && let Some(fk) = list.get(i) {
                        let (schema, table) = if self.table_page == TablePage::References {
                            (fk.schema.clone(), fk.table.clone())
                        } else {
                            (fk.ref_schema.clone().or(self.obj.schema.clone()), fk.ref_table.clone())
                        };
                        let target = ObjectRef::table(schema, table).with_database(self.obj.database.clone());
                        cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: target, page: ObjectPage::Properties });
                    }
            }
            TablePage::Indexes => {
                let rows: Vec<Vec<String>> = t
                    .indexes
                    .iter()
                    .map(|i| {
                        vec![
                            i.name.clone(),
                            if i.primary { "PRIMARY".into() } else if i.unique { "UNIQUE".into() } else { String::new() },
                            i.method.clone().unwrap_or_default(),
                            i.columns.iter().map(|c| format!("{}{}", c.name, if c.descending { " DESC" } else { "" })).collect::<Vec<_>>().join(", "),
                            i.predicate.clone().unwrap_or_default(),
                            i.definition.clone().unwrap_or_default(),
                        ]
                    })
                    .collect();
                widgets::simple_table(ui, "obj_idx", &[tr("Name"), tr("Kind"), tr("Method"), tr("Columns"), tr("Predicate"), tr("Definition")], &rows, None);
            }
            TablePage::Triggers => {
                let rows: Vec<Vec<String>> = t
                    .triggers
                    .iter()
                    .map(|tr_| {
                        vec![
                            tr_.name.clone(),
                            tr_.timing.clone(),
                            tr_.events.join(" OR "),
                            if tr_.for_each_row { "ROW".into() } else { "STATEMENT".into() },
                            if tr_.enabled { "✓".into() } else { String::new() },
                            tr_.definition.clone(),
                        ]
                    })
                    .collect();
                widgets::simple_table(ui, "obj_trg", &[tr("Name"), tr("Timing"), tr("Events"), tr("Level"), tr("Enabled"), tr("Definition")], &rows, None);
            }
            TablePage::VirtualModel => {
                let key = dbiji_config::project::VirtualTableKey::of(&self.obj);
                let projects = cx.services.projects.as_ref();
                if self.vmodel.is_none() {
                    let vm = projects.and_then(|p| p.virtual_model(&self.conn).ok()).unwrap_or_default();
                    self.vmodel = Some(vm.get(&key).cloned().unwrap_or_default());
                }
                let Some(vt) = self.vmodel.as_mut() else { return };
                ui.label(tr("Client-side additions used by the data editor; stored in the project, not in the database."));
                ui.add_space(4.0);
                ui.strong(tr("Virtual key"));
                ui.weak(tr("Columns that identify a row when the table has no primary or unique key."));
                ui.horizontal_wrapped(|ui| {
                    let mut keys = vt.virtual_key.clone().unwrap_or_default();
                    for c in &t.columns {
                        let mut on = keys.contains(&c.name);
                        if ui.checkbox(&mut on, &c.name).changed() {
                            if on {
                                keys.push(c.name.clone());
                            } else {
                                keys.retain(|k| k != &c.name);
                            }
                        }
                    }
                    vt.virtual_key = if keys.is_empty() { None } else { Some(keys) };
                });
                ui.add_space(6.0);
                ui.strong(tr("Dictionary"));
                ui.weak(tr("When other tables reference this one, show these columns next to the key."));
                ui.horizontal_wrapped(|ui| {
                    let mut cols = vt.dictionary.clone().map(|d| d.description_columns).unwrap_or_default();
                    for c in &t.columns {
                        let mut on = cols.contains(&c.name);
                        if ui.checkbox(&mut on, &c.name).changed() {
                            if on {
                                cols.push(c.name.clone());
                            } else {
                                cols.retain(|k| k != &c.name);
                            }
                        }
                    }
                    vt.dictionary = if cols.is_empty() { None } else { Some(dbiji_config::project::DictionaryConfig { description_columns: cols }) };
                });
                ui.add_space(6.0);
                ui.strong(tr("Virtual columns"));
                let mut remove = None;
                egui::Grid::new("vcols").num_columns(3).spacing([10.0, 4.0]).show(ui, |ui| {
                    for (i, c) in vt.virtual_columns.iter_mut().enumerate() {
                        ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(140.0));
                        ui.add(egui::TextEdit::singleline(&mut c.expression).desired_width(320.0).code_editor());
                        if ui.small_button(icon::TRASH).clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                    ui.add(egui::TextEdit::singleline(&mut self.new_vcol.0).hint_text(tr("name")).desired_width(140.0));
                    ui.add(egui::TextEdit::singleline(&mut self.new_vcol.1).hint_text(tr("expression, e.g. price * quantity")).desired_width(320.0).code_editor());
                    if ui.add_enabled(!self.new_vcol.0.trim().is_empty() && !self.new_vcol.1.trim().is_empty(), egui::Button::new(icon::PLUS)).clicked() {
                        vt.virtual_columns.push(dbiji_config::project::VirtualColumn {
                            name: self.new_vcol.0.trim().to_string(),
                            expression: self.new_vcol.1.trim().to_string(),
                            ..Default::default()
                        });
                        self.new_vcol = (String::new(), String::new());
                    }
                    ui.end_row();
                });
                if let Some(i) = remove {
                    vt.virtual_columns.remove(i);
                }
                ui.add_space(6.0);
                if ui.button(RichText::new(format!("{} {}", icon::FLOPPY_DISK, tr("Save virtual model"))).strong()).clicked()
                    && let Some(p) = projects {
                        let mut vm = p.virtual_model(&self.conn).unwrap_or_default();
                        if vt.is_empty() {
                            vm.remove(&key);
                        } else {
                            vm.set(key.clone(), vt.clone());
                        }
                        match p.save_virtual_model(&self.conn, &vm) {
                            Ok(()) => cx.toasts.success(tr("Virtual model saved")),
                            Err(e) => cx.toasts.error(tr("Cannot save the virtual model"), Some(e.to_string())),
                        }
                    }
            }
            TablePage::Partitions => {
                if let Some(p) = &t.partitioning {
                    ui.label(format!("{} {} ({})", tr("Partitioned by"), p.strategy, p.key));
                }
                let rows: Vec<Vec<String>> = t
                    .partitions
                    .iter()
                    .map(|p| vec![p.name.clone(), p.bound.clone(), p.row_estimate.map(|r| r.to_string()).unwrap_or_default()])
                    .collect();
                widgets::simple_table(ui, "obj_parts", &[tr("Name"), tr("Bound"), tr("Rows")], &rows, None);
            }
        }
        if retry {
            self.table = Loadable::NotLoaded;
        }
    }

    fn ddl_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let session = self.session(cx);
        let obj = self.obj.clone();
        Self::load(&mut self.ddl, cx.backend, session, move |s| async move { s.lock().await.object_ddl(&obj).await });
        let mut retry = false;
        if widgets::load_state(ui, &self.ddl, &mut retry) {
            let text = self.ddl.get().cloned().unwrap_or_default();
            ui.horizontal(|ui| {
                if ui.button(format!("{} {}", icon::FILE_SQL, tr("Open in SQL editor"))).clicked() {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: Some(self.obj.schema_ref()), text: Some(text.clone()) });
                }
            });
            crate::tabs::code(ui, cx, ("ddl", &self.obj), &text);
        }
        if retry {
            self.ddl = Loadable::NotLoaded;
        }
    }

    fn deps_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let session = self.session(cx);
        let obj = self.obj.clone();
        Self::load(&mut self.deps, cx.backend, session, move |s| async move { s.lock().await.object_dependencies(&obj).await });
        let mut retry = false;
        if widgets::load_state(ui, &self.deps, &mut retry) {
            let deps = self.deps.get().cloned().unwrap_or_default();
            if deps.is_empty() {
                ui.weak(tr("No dependencies."));
            }
            let rows: Vec<Vec<String>> = deps
                .iter()
                .map(|d| {
                    vec![
                        match d.direction {
                            DependencyDirection::DependsOn => tr("depends on").to_string(),
                            DependencyDirection::UsedBy => tr("used by").to_string(),
                        },
                        tr(d.target.kind.label()).to_string(),
                        d.target.display_path(),
                        d.kind.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            let r = widgets::simple_table(ui, "obj_deps", &[tr("Direction"), tr("Kind"), tr("Object"), tr("Dependency")], &rows, None);
            if let Some(i) = r.double_clicked
                && let Some(d) = deps.get(i) {
                    cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: d.target.clone(), page: ObjectPage::Properties });
                }
        }
        if retry {
            self.deps = Loadable::NotLoaded;
        }
    }

    fn stats_page(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let session = self.session(cx);
        let obj = self.obj.clone();
        Self::load(&mut self.stats, cx.backend, session, move |s| async move { s.lock().await.object_statistics(&obj).await });
        let mut retry = false;
        if widgets::load_state(ui, &self.stats, &mut retry) {
            let stats = self.stats.get().cloned().unwrap_or_default();
            if stats.is_empty() {
                ui.weak(tr("No statistics available."));
            }
            egui::ScrollArea::vertical().show(ui, |ui| widgets::property_grid(ui, "obj_stats", &stats));
        }
        if retry {
            self.stats = Loadable::NotLoaded;
        }
    }
}

impl TabView for ObjectEditorTab {
    fn title(&self) -> String {
        self.obj.name.clone()
    }

    fn icon(&self) -> &'static str {
        crate::nav::kind_icon(self.obj.kind)
    }

    fn tooltip(&self) -> Option<String> {
        Some(format!("{}: {}", tr(self.obj.kind.label()), self.obj.display_path()))
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(Self::key(&self.conn, &self.obj))
    }

    fn is_dirty(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.is_dirty())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !cx.conns.is_connected(&self.conn) {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(tr("The connection is closed."));
                if ui.button(tr("Connect")).clicked() {
                    cx.push(AppCommand::Connect(self.conn.clone()));
                }
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", crate::nav::kind_icon(self.obj.kind), self.obj.display_path())).strong());
            ui.separator();
            let mut pages = vec![(ObjectPage::Properties, tr("Properties"))];
            if self.obj.kind.has_data() {
                pages.push((ObjectPage::Data, tr("Data")));
            }
            pages.push((ObjectPage::Ddl, tr("DDL")));
            pages.push((ObjectPage::Dependencies, tr("Dependencies")));
            if self.obj.kind.has_data() {
                pages.push((ObjectPage::Statistics, tr("Statistics")));
            }
            if self.obj.kind.is_table_like() {
                pages.push((ObjectPage::Diagram, tr("Diagram")));
            }
            for (p, label) in pages {
                ui.selectable_value(&mut self.page, p, label);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(icon::ARROW_CLOCKWISE).on_hover_text(tr("Refresh")).clicked() {
                    self.refresh();
                    cx.push(AppCommand::RefreshMetadata { connection: self.conn.clone(), object: Some(self.obj.clone()), schema: None });
                }
                ui.menu_button(icon::DOTS_THREE, |ui| crate::nav::object_menu(ui, cx, &self.conn.clone(), &self.obj.clone()));
            });
        });
        ui.separator();
        match self.page {
            ObjectPage::Properties => self.properties_page(ui, cx),
            ObjectPage::Data => {
                if self.data.is_none() {
                    self.data = crate::tabs::make_table_data_view(cx, &self.conn, &self.obj);
                }
                match &mut self.data {
                    Some(d) => d.ui(ui, cx),
                    None => {
                        ui.weak(tr("The data editor is not available."));
                    }
                }
            }
            ObjectPage::Ddl => self.ddl_page(ui, cx),
            ObjectPage::Dependencies => self.deps_page(ui, cx),
            ObjectPage::Statistics => self.stats_page(ui, cx),
            ObjectPage::Diagram => {
                cx.push(AppCommand::OpenErDiagram { connection: self.conn.clone(), schema: self.obj.schema_ref(), tables: vec![self.obj.name.clone()] });
                self.page = ObjectPage::Properties;
            }
        }
    }

    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        match (&mut self.data, self.page) {
            (Some(d), ObjectPage::Data) => d.on_action(action, cx),
            _ => false,
        }
    }

    fn session_state(&self) -> Option<serde_json::Value> {
        let tab = dbiji_config::SessionTab {
            kind: if self.page == ObjectPage::Data { dbiji_config::project::TabKind::DataEditor } else { dbiji_config::project::TabKind::ObjectEditor },
            title: Some(self.obj.name.clone()),
            connection_id: Some(self.conn.clone()),
            object: Some(self.obj.clone()),
            ..Default::default()
        };
        serde_json::to_value(tab).ok()
    }

    fn focus_context(&self) -> crate::keys::ActionContext {
        match (&self.data, self.page) {
            (Some(d), ObjectPage::Data) => d.focus_context(),
            _ => crate::keys::ActionContext::Global,
        }
    }

    crate::impl_tab_any!();
}
