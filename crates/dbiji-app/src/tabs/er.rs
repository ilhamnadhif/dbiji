//! ER diagram tab (FR-M2): diagram of a schema or of selected tables, saved
//! diagrams in the project, SVG / PNG export and an edit mode whose changes
//! are turned into DDL for review in a SQL editor.

use crate::command::{AppCommand, ObjectPage};
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::ddl::{ForeignKeyDef, SchemaSnapshot, TableDef};
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{tr, trf};
use dbiji_visual::er::{ErDiagram, ErEvent, ErModel, ErViewState, export_png, export_svg, show_er};
use egui::Ui;
use egui_phosphor::regular as icon;
use std::sync::Arc;

pub struct ErTab {
    conn: String,
    schema: SchemaRef,
    /// Tables to show (empty = whole schema).
    tables: Vec<String>,
    diagram: Option<ErDiagram>,
    state: ErViewState,
    /// Name under which the diagram is saved in the project.
    saved_name: Option<String>,
    name_edit: String,
    dirty: bool,
    snapshot: Option<Arc<SchemaSnapshot>>,
    refresh_pending: bool,
}

impl ErTab {
    pub fn new(conn: String, schema: SchemaRef, tables: Vec<String>) -> Self {
        let name = match tables.as_slice() {
            [] => schema.schema.clone().or(schema.database.clone()).unwrap_or_else(|| tr("Diagram").to_string()),
            [t] => t.clone(),
            _ => format!("{} (+{})", tables[0], tables.len() - 1),
        };
        ErTab { conn, schema, tables, diagram: None, state: ErViewState::default(), saved_name: None, name_edit: name, dirty: false, snapshot: None, refresh_pending: false }
    }

    /// Open a diagram saved in the project.
    pub fn from_saved(conn: String, schema: SchemaRef, name: String, diagram: ErDiagram) -> Self {
        let mut t = Self::new(conn, schema, Vec::new());
        t.name_edit = name.clone();
        t.saved_name = Some(name);
        t.diagram = Some(diagram);
        t
    }

    pub fn key(conn: &str, schema: &SchemaRef, tables: &[String]) -> String {
        format!("er:{conn}:{}:{}:{}", schema.database.as_deref().unwrap_or(""), schema.schema.as_deref().unwrap_or(""), tables.join(","))
    }

    fn object_of(&self, key: &str) -> ObjectRef {
        let (schema, name) = match key.split_once('.') {
            Some((s, n)) if self.schema.schema.as_deref() == Some(s) || self.snapshot.as_ref().is_some_and(|sn| sn.schema.as_deref() == Some(s)) => {
                (Some(s.to_string()), n.to_string())
            }
            _ => (self.schema.schema.clone(), key.to_string()),
        };
        let is_view = self.snapshot.as_ref().is_some_and(|s| s.views.iter().any(|v| v.name == name));
        let kind = if is_view { ObjectKind::View } else { ObjectKind::Table };
        ObjectRef::new(kind, schema, name).with_database(self.schema.database.clone())
    }

    fn table_def(&self, key: &str) -> Option<TableDef> {
        let obj = self.object_of(key);
        self.snapshot.as_ref()?.tables.iter().find(|t| t.name == obj.name).cloned()
    }

    fn build_model(&self, snap: &SchemaSnapshot) -> ErModel {
        if self.tables.is_empty() {
            ErModel::from_snapshot(snap, None)
        } else {
            let keys: Vec<String> = self.tables.iter().map(|t| dbiji_visual::er::table_key(snap.schema.as_deref(), t)).collect();
            ErModel::from_snapshot(snap, Some(&keys))
        }
    }

    fn save(&mut self, cx: &mut AppContext) {
        let Some(d) = self.diagram.as_mut() else { return };
        let Some(projects) = cx.services.projects.as_ref() else { return };
        let name = self.name_edit.trim().to_string();
        if name.is_empty() {
            return;
        }
        d.name = name.clone();
        let json = match d.to_json().and_then(|j| serde_json::from_str::<serde_json::Value>(&j).map_err(dbiji_core::DbError::from)) {
            Ok(j) => serde_json::json!({ "connection": self.conn, "database": self.schema.database, "schema": self.schema.schema, "diagram": j }),
            Err(e) => {
                cx.toasts.error(tr("Cannot save the diagram"), Some(e.to_string()));
                return;
            }
        };
        match projects.default_project().and_then(|p| p.save_diagram(&name, &json)) {
            Ok(_) => {
                self.saved_name = Some(name);
                self.dirty = false;
                cx.toasts.success(tr("Diagram saved"));
            }
            Err(e) => cx.toasts.error(tr("Cannot save the diagram"), Some(e.to_string())),
        }
    }

    fn export(&mut self, cx: &mut AppContext) {
        let Some(d) = &self.diagram else { return };
        let Some(path) = rfd::FileDialog::new().set_file_name(format!("{}.svg", d.name)).add_filter("SVG", &["svg"]).add_filter("PNG", &["png"]).save_file() else {
            return;
        };
        let png = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
        let res = if png { export_png(d, 2.0).and_then(|b| std::fs::write(&path, b).map_err(Into::into)) } else { std::fs::write(&path, export_svg(d)).map_err(Into::into) };
        match res {
            Ok(()) => cx.toasts.success(trf("Exported to {}", &[&path.display()])),
            Err(e) => cx.toasts.error(tr("Export failed"), Some(e.to_string())),
        }
    }

    fn handle(&mut self, ev: ErEvent, cx: &mut AppContext) {
        let dialect = cx.conns.live(&self.conn).map(|l| l.dialect.clone());
        match ev {
            ErEvent::OpenTable(k) | ErEvent::EditTable(k) => {
                cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: self.object_of(&k), page: ObjectPage::Properties });
            }
            ErEvent::ViewData(k) => cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: self.object_of(&k), page: ObjectPage::Data }),
            ErEvent::GenerateDdl(keys) => {
                let Some(d) = dialect else { return };
                let mut sql = String::new();
                for k in keys {
                    if let Some(t) = self.table_def(&k) {
                        for s in d.ddl().create_table(&t, true) {
                            sql.push_str(&s);
                            sql.push_str(";\n");
                        }
                        sql.push('\n');
                    }
                }
                cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: Some(self.schema.clone()), text: Some(sql) });
            }
            ErEvent::RemoveFromDiagram(keys) => {
                if let Some(d) = self.diagram.as_mut() {
                    d.remove_tables(&keys.into_iter().collect());
                    self.dirty = true;
                }
            }
            ErEvent::AddRelatedTables(k) => {
                let (Some(snap), Some(d)) = (self.snapshot.clone(), self.diagram.as_mut()) else { return };
                let obj_name = k.rsplit('.').next().unwrap_or(&k).to_string();
                let mut related: Vec<String> = Vec::new();
                for t in &snap.tables {
                    if t.name == obj_name {
                        related.extend(t.foreign_keys.iter().map(|f| f.ref_table.clone()));
                        related.extend(t.referenced_by.iter().map(|f| f.table.clone()));
                    } else if t.foreign_keys.iter().any(|f| f.ref_table == obj_name) {
                        related.push(t.name.clone());
                    }
                }
                related.push(obj_name);
                related.sort();
                related.dedup();
                let keys: Vec<String> = related.iter().map(|n| dbiji_visual::er::table_key(snap.schema.as_deref(), n)).collect();
                let model = ErModel::from_snapshot(&snap, Some(&keys));
                let added = d.add_model(model);
                if !added.is_empty() {
                    self.dirty = true;
                }
            }
            ErEvent::TableMoved(_) | ErEvent::SettingsChanged | ErEvent::NotesChanged => self.dirty = true,
            ErEvent::SelectionChanged(_) => {}
            ErEvent::ExportRequested => self.export(cx),
            ErEvent::CreateTable { .. } => cx.push(AppCommand::CreateTable { connection: self.conn.clone(), schema: self.schema.clone() }),
            ErEvent::AddColumn(k) => cx.push(AppCommand::EditTable { connection: self.conn.clone(), table: self.object_of(&k) }),
            ErEvent::DeleteTables(keys) => {
                let objects: Vec<ObjectRef> = keys.iter().map(|k| self.object_of(k)).collect();
                cx.push(AppCommand::DropObjects { connection: self.conn.clone(), objects });
            }
            ErEvent::CreateRelation { from_table, from_columns, to_table, to_columns } => {
                let (Some(d), Some(t)) = (dialect, self.table_def(&from_table)) else { return };
                let to = self.object_of(&to_table);
                let fk = ForeignKeyDef {
                    name: Some(format!("fk_{}_{}", t.name, to.name)),
                    schema: t.schema.clone(),
                    table: t.name.clone(),
                    columns: from_columns,
                    ref_schema: to.schema.clone(),
                    ref_table: to.name.clone(),
                    ref_columns: to_columns,
                    ..Default::default()
                };
                let sql = d.ddl().add_foreign_key(&t, &fk) + ";\n";
                cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: Some(self.schema.clone()), text: Some(sql) });
            }
            ErEvent::DeleteRelation(rel) => {
                let (Some(d), Some(t)) = (dialect, self.table_def(&rel.from_table)) else { return };
                let Some(fk) = t.foreign_keys.iter().find(|f| f.name.as_deref() == Some(rel.name.as_str())).cloned() else { return };
                let sql = d.ddl().drop_foreign_key(&t, &fk) + ";\n";
                cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: Some(self.schema.clone()), text: Some(sql) });
            }
            ErEvent::EditRelation(rel) => {
                let parent = self.object_of(&rel.from_table);
                let fk = ObjectRef::child(ObjectKind::ForeignKey, &parent, rel.name);
                cx.push(AppCommand::OpenObject { connection: self.conn.clone(), object: fk, page: ObjectPage::Properties });
            }
        }
    }
}

impl TabView for ErTab {
    fn title(&self) -> String {
        self.name_edit.clone()
    }

    fn icon(&self) -> &'static str {
        icon::GRAPH
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(match &self.saved_name {
            Some(n) => format!("er-saved:{n}"),
            None => Self::key(&self.conn, &self.schema, &self.tables),
        })
    }

    fn is_dirty(&self) -> bool {
        self.dirty && self.saved_name.is_some()
    }

    fn session_state(&self) -> Option<serde_json::Value> {
        let path = self.saved_name.as_ref().map(|n| std::path::PathBuf::from(format!("{n}.json")));
        let tab = dbiji_config::SessionTab {
            kind: dbiji_config::project::TabKind::ErDiagram,
            title: Some(self.name_edit.clone()),
            path,
            connection_id: Some(self.conn.clone()),
            database: self.schema.database.clone(),
            schema: self.schema.schema.clone(),
            content: (!self.tables.is_empty()).then(|| self.tables.join("\n")),
            ..Default::default()
        };
        serde_json::to_value(tab).ok()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&self.conn) else {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(tr("The connection is closed."));
                if ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked() {
                    cx.commands.push(AppCommand::Connect(self.conn.clone()));
                }
            });
            return;
        };
        let session = live.ensure_session(backend, self.schema.database.as_deref());
        if let Some(s) = &session {
            live.meta.request_snapshot(backend, s, &self.schema);
        }
        let entry = live.meta.snapshots.get(&self.schema);
        match entry {
            Some(crate::backend::Loadable::Loaded(snap)) => {
                let snap = snap.clone();
                if self.diagram.is_none() {
                    let model = self.build_model(&snap);
                    self.diagram = Some(ErDiagram::new(self.name_edit.clone(), model));
                } else if self.refresh_pending {
                    let model = self.build_model(&snap);
                    if let Some(d) = self.diagram.as_mut() {
                        d.refresh_model(model);
                    }
                    self.refresh_pending = false;
                }
                self.snapshot = Some(snap);
            }
            Some(crate::backend::Loadable::Failed(e)) => {
                let text = e.full_text();
                crate::widgets::error_box(ui, &text);
                if ui.button(tr("Retry")).clicked() {
                    live.meta.clear_schema(&self.schema);
                }
                return;
            }
            _ => {
                if self.diagram.is_none() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(tr("Reading the schema…"));
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.name_edit).desired_width(200.0));
            if ui.button((dbiji_icons::ic(icon::FLOPPY_DISK), tr("Save"))).on_hover_text(tr("Save the diagram in the project")).clicked() {
                self.save(cx);
            }
            if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                if let Some(live) = cx.conns.live_mut(&self.conn) {
                    live.meta.clear_schema(&self.schema);
                }
                self.refresh_pending = true;
            }
            if ui.button((dbiji_icons::ic(icon::TREE_STRUCTURE), tr("Layout"))).clicked()
                && let Some(d) = self.diagram.as_mut()
            {
                d.relayout();
                self.dirty = true;
            }
            if ui.button((dbiji_icons::ic(icon::EXPORT), tr("Export…"))).clicked() {
                self.export(cx);
            }
            ui.toggle_value(&mut self.state.edit_mode, (dbiji_icons::ic(icon::PENCIL_SIMPLE), tr("Edit mode")))
                .on_hover_text(tr("Draw relations and create tables; changes open as SQL for review"));
        });
        ui.separator();
        let events = match self.diagram.as_mut() {
            Some(d) => show_er(ui, d, &mut self.state).events,
            None => Vec::new(),
        };
        for ev in events {
            self.handle(ev, cx);
        }
    }

    crate::impl_tab_any!();
}
