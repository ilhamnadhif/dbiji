//! Schema compare (FR-M3): compare two schemas (same or different
//! connections), review the differences, generate the migration script in
//! the target dialect, apply it, export an HTML report or a Liquibase
//! changelog.

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_compare::{CompareOptions, DiffItem, DiffStatus, LiquibaseOptions, SchemaDiff, ScriptOptions};
use dbiji_core::ddl::{SchemaSnapshot, SnapshotOptions};
use dbiji_core::meta::SchemaRef;
use dbiji_core::{DbResult, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone, Default, PartialEq)]
struct Side {
    conn: Option<String>,
    schema: Option<SchemaRef>,
}

type Snapshots = (Arc<SchemaSnapshot>, Arc<SchemaSnapshot>);

pub struct SchemaCompareTab {
    source: Side,
    target: Side,
    job: Option<Job<DbResult<Snapshots>>>,
    snapshots: Option<Snapshots>,
    diff: Option<SchemaDiff>,
    /// Keys (`kind:qualified name`) of the objects excluded from the script.
    excluded: BTreeSet<String>,
    show_unchanged: bool,
    drop_removed: bool,
    cascade: bool,
    script: Option<Vec<String>>,
    apply_job: Option<Job<DbResult<usize>>>,
    confirm_apply: bool,
    error: Option<String>,
    selected: Option<String>,
}

fn key(i: &DiffItem) -> String {
    format!("{:?}:{}", i.kind, i.qualified_name())
}

fn status_color(s: DiffStatus, dark: bool) -> egui::Color32 {
    match s {
        DiffStatus::Added => crate::theme::success_color(dark),
        DiffStatus::Removed => crate::theme::error_color(dark),
        DiffStatus::Modified => crate::theme::warning_color(dark),
        DiffStatus::Unchanged => egui::Color32::GRAY,
    }
}

impl SchemaCompareTab {
    pub fn new(conn: Option<String>, schema: Option<SchemaRef>) -> Self {
        SchemaCompareTab {
            source: Side { conn: conn.clone(), schema: schema.clone() },
            target: Side { conn, schema: None },
            job: None,
            snapshots: None,
            diff: None,
            excluded: BTreeSet::new(),
            show_unchanged: false,
            drop_removed: true,
            cascade: false,
            script: None,
            apply_job: None,
            confirm_apply: false,
            error: None,
            selected: None,
        }
    }

    fn side_ui(ui: &mut Ui, cx: &mut AppContext, id: &str, side: &mut Side) {
        let names: Vec<(String, String)> = cx.conns.configs().iter().map(|c| (c.id.clone(), c.name.clone())).collect();
        let current = side.conn.as_ref().and_then(|c| names.iter().find(|(i, _)| i == c)).map(|(_, n)| n.clone()).unwrap_or_else(|| tr("Choose…").to_string());
        egui::ComboBox::from_id_salt((id, "conn")).selected_text(current).width(180.0).show_ui(ui, |ui| {
            for (cid, name) in &names {
                if ui.selectable_label(side.conn.as_deref() == Some(cid), name).clicked() {
                    side.conn = Some(cid.clone());
                    side.schema = None;
                }
            }
        });
        let Some(conn) = side.conn.clone() else { return };
        if !cx.conns.is_connected(&conn) {
            if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                cx.push(AppCommand::Connect(conn));
            }
            return;
        }
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&conn) else { return };
        let main = live.main.clone();
        live.meta.request_schemas(backend, &main, None);
        let schemas: Vec<String> = live.meta.schemas_of(None).get().map(|v| v.iter().map(|s| s.name.clone()).collect()).unwrap_or_default();
        let db = live.current_database.clone();
        let label = side.schema.as_ref().and_then(|s| s.schema.clone()).unwrap_or_else(|| tr("Schema…").to_string());
        egui::ComboBox::from_id_salt((id, "schema")).selected_text(label).width(160.0).show_ui(ui, |ui| {
            if schemas.is_empty() {
                // Single-schema databases (SQLite, MySQL database = schema).
                let cur = live.current_schema.clone().or(db.clone());
                if ui.selectable_label(false, cur.clone().unwrap_or_else(|| tr("(default)").to_string())).clicked() {
                    side.schema = Some(SchemaRef::new(None, cur));
                }
            }
            for s in &schemas {
                if ui.selectable_label(side.schema.as_ref().and_then(|x| x.schema.as_deref()) == Some(s.as_str()), s).clicked() {
                    side.schema = Some(SchemaRef::new(None, Some(s.clone())));
                }
            }
        });
    }

    fn start(&mut self, cx: &mut AppContext) {
        let (Some(sc), Some(ss), Some(tc), Some(ts)) = (self.source.conn.clone(), self.source.schema.clone(), self.target.conn.clone(), self.target.schema.clone()) else {
            self.error = Some(tr("Choose the source and target schemas.").to_string());
            return;
        };
        let (Some(a), Some(b)) = (cx.conns.live(&sc).map(|l| l.main.clone()), cx.conns.live(&tc).map(|l| l.main.clone())) else { return };
        self.error = None;
        self.diff = None;
        self.script = None;
        self.excluded.clear();
        self.job = Some(cx.backend.spawn(async move {
            let opts = SnapshotOptions::full();
            let sa = a.lock().await.schema_snapshot(&ss, &opts).await?;
            let sb = b.lock().await.schema_snapshot(&ts, &opts).await?;
            Ok((Arc::new(sa), Arc::new(sb)))
        }));
    }

    fn selected_diff(&self) -> Option<SchemaDiff> {
        let mut d = self.diff.clone()?;
        d.retain(|i| !self.excluded.contains(&key(i)));
        Some(d)
    }

    fn script_options(&self) -> ScriptOptions {
        ScriptOptions { drop_removed: self.drop_removed, cascade: self.cascade, include_comments: true, ..Default::default() }
    }

    fn build_script(&mut self, cx: &AppContext) {
        let (Some(d), Some((a, b)), Some(tc)) = (self.selected_diff(), self.snapshots.clone(), self.target.conn.clone()) else { return };
        let Some(dialect) = cx.conns.live(&tc).map(|l| l.dialect.clone()) else { return };
        self.script = Some(dbiji_compare::migration_script(&d, &a, &b, dialect.as_ref(), &self.script_options()));
    }

    fn item_row(&mut self, ui: &mut Ui, item: &DiffItem, depth: usize) {
        if !self.show_unchanged && !item.has_changes() {
            return;
        }
        let k = key(item);
        let dark = ui.visuals().dark_mode;
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 16.0);
            if depth == 0 {
                let mut on = !self.excluded.contains(&k);
                if ui.checkbox(&mut on, "").changed() {
                    if on {
                        self.excluded.remove(&k);
                    } else {
                        self.excluded.insert(k.clone());
                    }
                    self.script = None;
                }
            }
            ui.label(RichText::new(tr(item.status.label())).color(status_color(item.status, dark)).small());
            ui.label(crate::nav::kind_icon(item.kind));
            if ui.selectable_label(self.selected.as_deref() == Some(k.as_str()), item.display_name()).clicked() {
                self.selected = Some(k.clone());
            }
        });
        if self.selected.as_deref() == Some(k.as_str()) && !item.changes.is_empty() {
            egui::Grid::new(("chg", &k)).num_columns(3).striped(true).show(ui, |ui| {
                ui.strong(tr("Property"));
                ui.strong(tr("Source"));
                ui.strong(tr("Target"));
                ui.end_row();
                for c in &item.changes {
                    ui.label(&c.property);
                    ui.monospace(c.source.clone().unwrap_or_else(|| "—".into()));
                    ui.monospace(c.target.clone().unwrap_or_else(|| "—".into()));
                    ui.end_row();
                }
            });
        }
        for c in &item.children {
            self.item_row(ui, c, depth + 1);
        }
    }
}

impl TabView for SchemaCompareTab {
    fn title(&self) -> String {
        tr("Schema compare").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::GIT_DIFF
    }
    fn connection_id(&self) -> Option<&str> {
        self.target.conn.as_deref()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if let Some(j) = self.job.as_mut()
            && let Some(r) = j.poll()
        {
            self.job = None;
            match r {
                Ok((a, b)) => {
                    let diff = dbiji_compare::schema_diff(&a, &b, &CompareOptions::default());
                    self.snapshots = Some((a, b));
                    self.diff = Some(diff);
                }
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        if let Some(j) = self.apply_job.as_mut()
            && let Some(r) = j.poll()
        {
            self.apply_job = None;
            match r {
                Ok(n) => {
                    cx.toasts.success(trf("{} statement(s) executed on the target", &[&n]));
                    if let (Some(tc), Some(ts)) = (&self.target.conn, &self.target.schema)
                        && let Some(l) = cx.conns.live_mut(tc)
                    {
                        l.meta.clear_schema(ts);
                    }
                    self.start(cx);
                }
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        egui::Grid::new("cmp_sides").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.strong(tr("Source"));
            ui.horizontal(|ui| Self::side_ui(ui, cx, "src", &mut self.source));
            ui.end_row();
            ui.strong(tr("Target"));
            ui.horizontal(|ui| Self::side_ui(ui, cx, "dst", &mut self.target));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            let busy = self.job.is_some();
            if ui.add_enabled(!busy, egui::Button::new(format!("{} {}", icon::GIT_DIFF, tr("Compare")))).clicked() {
                self.start(cx);
            }
            if ui.button(format!("{} {}", icon::ARROWS_LEFT_RIGHT, tr("Swap"))).clicked() {
                std::mem::swap(&mut self.source, &mut self.target);
                self.diff = None;
                self.script = None;
            }
            if busy {
                ui.spinner();
                ui.weak(tr("Reading both schemas…"));
            }
            ui.separator();
            ui.checkbox(&mut self.show_unchanged, tr("Show unchanged"));
            if ui.checkbox(&mut self.drop_removed, tr("Drop objects missing in the source")).changed() {
                self.script = None;
            }
            if ui.checkbox(&mut self.cascade, tr("Cascade")).changed() {
                self.script = None;
            }
        });
        if let Some(e) = &self.error {
            crate::widgets::error_box(ui, e);
        }
        let Some(diff) = self.diff.clone() else { return };
        let t = diff.summary.total;
        ui.label(trf("{} added, {} removed, {} modified, {} unchanged", &[&t.added, &t.removed, &t.modified, &t.unchanged]));
        if !diff.has_differences() {
            ui.label(RichText::new(format!("{} {}", icon::CHECK_CIRCLE, tr("The schemas are identical."))).color(crate::theme::success_color(ui.visuals().dark_mode)));
        }
        if self.script.is_none() {
            self.build_script(cx);
        }
        let script = self.script.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            let target = self.target.conn.clone();
            if ui.button(format!("{} {}", icon::FILE_SQL, tr("Open script in SQL editor"))).clicked() {
                let text = script.iter().map(|s| format!("{s};\n")).collect::<String>();
                cx.push(AppCommand::NewSqlEditor { connection: target.clone(), schema: self.target.schema.clone(), text: Some(text) });
            }
            if ui.add_enabled(!script.is_empty() && self.apply_job.is_none(), egui::Button::new(format!("{} {}", icon::PLAY, tr("Apply to target…")))).clicked() {
                self.confirm_apply = true;
            }
            if ui.button(format!("{} {}", icon::GRAPH, tr("Diff diagram"))).on_hover_text(tr("ER diagram of the target with the differences highlighted")).clicked()
                && let (Some((a, b)), Some(tc)) = (self.snapshots.clone(), self.target.conn.clone())
            {
                let old = dbiji_visual::er::ErModel::from_snapshot(&b, None);
                let new = dbiji_visual::er::ErModel::from_snapshot(&a, None);
                let model = dbiji_visual::er::ErModel::diff(&old, &new);
                let name = trf("Diff {}", &[&self.target.schema.as_ref().and_then(|s| s.schema.clone()).unwrap_or_default()]);
                let diagram = dbiji_visual::er::ErDiagram::new(name.clone(), model);
                let schema = self.target.schema.clone().unwrap_or_else(|| SchemaRef::new(None, None));
                cx.push(AppCommand::OpenTab(Box::new(crate::tabs::er::ErTab::from_saved(tc, schema, name, diagram))));
            }
            if ui.button(format!("{} {}", icon::FILE_HTML, tr("HTML report…"))).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("schema-compare.html").save_file()
            {
                let src = self.source.schema.as_ref().and_then(|s| s.schema.clone()).unwrap_or_default();
                let dst = self.target.schema.as_ref().and_then(|s| s.schema.clone()).unwrap_or_default();
                let html = dbiji_compare::html_report(&diff, &src, &dst);
                if let Err(e) = std::fs::write(&path, html) {
                    cx.toasts.error(tr("Cannot write the file"), Some(e.to_string()));
                }
            }
            if ui.button(format!("{} {}", icon::FILE_CODE, tr("Liquibase changelog…"))).clicked()
                && let (Some(d), Some((a, b))) = (self.selected_diff(), self.snapshots.clone())
                && let Some(path) = rfd::FileDialog::new().set_file_name("changelog.xml").save_file()
            {
                let opts =
                    LiquibaseOptions { script: self.script_options(), dialect: target.as_ref().and_then(|t| cx.conns.live(t)).map(|l| l.dialect.clone()), ..Default::default() };
                let xml = dbiji_compare::liquibase_changelog(&d, &a, &b, &opts);
                if let Err(e) = std::fs::write(&path, xml) {
                    cx.toasts.error(tr("Cannot write the file"), Some(e.to_string()));
                }
            }
            if self.apply_job.is_some() {
                ui.spinner();
            }
        });
        let avail = ui.available_height();
        ui.columns(2, |cols| {
            egui::ScrollArea::vertical().id_salt("cmp_items").max_height(avail).show(&mut cols[0], |ui| {
                for item in &diff.items {
                    self.item_row(ui, item, 0);
                }
            });
            egui::ScrollArea::both().id_salt("cmp_script").max_height(avail).show(&mut cols[1], |ui| {
                let text = script.iter().map(|s| format!("{s};\n")).collect::<String>();
                let mut t = text.as_str();
                ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
            });
        });
        if self.confirm_apply {
            let mut close = false;
            let target_name = self.target.conn.as_ref().and_then(|c| cx.conns.config(c)).map(|c| c.name.clone()).unwrap_or_default();
            let modal = egui::Modal::new(egui::Id::new("cmp_apply")).show(ui.ctx(), |ui| {
                ui.heading(tr("Apply the migration"));
                ui.label(trf("Run {} statement(s) on \"{}\"?", &[&script.len(), &target_name]));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Run")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        if let Some(live) = self.target.conn.as_ref().and_then(|c| cx.conns.live(c)) {
                            let s = live.main.clone();
                            let previous = live.current_schema.clone().or(live.current_database.clone());
                            let target_schema = self.target.schema.as_ref().and_then(|t| t.schema.clone());
                            let stmts = script.clone();
                            self.apply_job = Some(cx.backend.spawn(async move {
                                // Unqualified names in view bodies and foreign keys
                                // resolve in the active schema: make it the target.
                                if let Some(t) = &target_schema {
                                    s.lock().await.set_current_schema(t).await?;
                                }
                                let mut n = 0;
                                let mut result = Ok(());
                                for sql in &stmts {
                                    if let Err(e) = s.execute_update(sql, &[]).await {
                                        result = Err(e.with_context(sql.chars().take(200).collect::<String>()));
                                        break;
                                    }
                                    n += 1;
                                }
                                if let (Some(p), Some(_)) = (&previous, &target_schema) {
                                    let _ = s.lock().await.set_current_schema(p).await;
                                }
                                result.map(|_| n)
                            }));
                        }
                        close = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.confirm_apply = false;
            }
        }
        if self.job.is_some() || self.apply_job.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    crate::impl_tab_any!();
}
