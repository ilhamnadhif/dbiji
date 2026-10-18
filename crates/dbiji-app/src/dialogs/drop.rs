//! Drop objects: confirmation with the exact DDL, optional CASCADE, then
//! execution on the connection's session and a metadata refresh.

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use dbiji_core::ddl::{ForeignKeyDef, IndexDef, KeyDef, CheckDef, RoutineDef, SequenceDef, TableDef, TriggerDef, ViewDef};
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::{DbResult, Dialect, tr, trf};
use egui::RichText;
use egui_phosphor::regular as icon;

pub struct DropDialog {
    conn: String,
    objects: Vec<ObjectRef>,
    cascade: bool,
    job: Option<Job<DbResult<usize>>>,
    error: Option<String>,
}

/// `DROP` statement for one object in the connection's dialect.
pub fn drop_sql(d: &dyn Dialect, o: &ObjectRef, cascade: bool) -> String {
    let ddl = d.ddl();
    let parent = || {
        let mut t = TableDef::new(o.schema.clone(), o.table.clone().unwrap_or_default());
        t.schema = o.schema.clone();
        t
    };
    let cascade_sfx = if cascade { " CASCADE" } else { "" };
    match o.kind {
        ObjectKind::Table | ObjectKind::ForeignTable | ObjectKind::Partition => ddl.drop_table(o.schema.as_deref(), &o.name, cascade),
        ObjectKind::View | ObjectKind::MaterializedView => {
            let v = ViewDef { schema: o.schema.clone(), name: o.name.clone(), materialized: o.kind == ObjectKind::MaterializedView, ..Default::default() };
            ddl.drop_view(&v) + cascade_sfx
        }
        ObjectKind::Sequence => ddl.drop_sequence(&SequenceDef { schema: o.schema.clone(), name: o.name.clone(), ..Default::default() }) + cascade_sfx,
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
            let r = RoutineDef {
                schema: o.schema.clone(),
                name: o.name.clone(),
                kind: Some(o.kind),
                arguments: o.signature.clone().map(|s| s.trim_start_matches('(').trim_end_matches(')').to_string()).unwrap_or_default(),
                ..Default::default()
            };
            ddl.drop_routine(&r)
        }
        ObjectKind::Index => ddl.drop_index(&parent(), &IndexDef { name: o.name.clone(), ..Default::default() }),
        ObjectKind::Trigger => ddl.drop_trigger(&parent(), &TriggerDef { name: o.name.clone(), ..Default::default() }),
        ObjectKind::ForeignKey => ddl.drop_foreign_key(&parent(), &ForeignKeyDef { name: Some(o.name.clone()), ..Default::default() }),
        ObjectKind::PrimaryKey => ddl.drop_primary_key(&parent(), &KeyDef { name: Some(o.name.clone()), ..Default::default() }),
        ObjectKind::UniqueKey => ddl.drop_unique_key(&parent(), &KeyDef { name: Some(o.name.clone()), ..Default::default() }),
        ObjectKind::CheckConstraint => ddl.drop_check(&parent(), &CheckDef { name: Some(o.name.clone()), ..Default::default() }),
        ObjectKind::Column => ddl.drop_column(&parent(), &o.name),
        ObjectKind::Schema => ddl.drop_schema(&o.name, cascade),
        ObjectKind::Database => format!("DROP DATABASE {}", d.quote_ident(&o.name)),
        other => format!("DROP {} {}{cascade_sfx}", other.label().to_uppercase(), d.qualified(o.schema.as_deref(), &o.name)),
    }
}

impl DropDialog {
    pub fn new(conn: String, objects: Vec<ObjectRef>) -> Self {
        DropDialog { conn, objects, cascade: false, job: None, error: None }
    }

    /// Draw; returns false when closed.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext) -> bool {
        if let Some(j) = self.job.as_mut() {
            if let Some(r) = j.poll() {
                self.job = None;
                match r {
                    Ok(n) => {
                        cx.toasts.success(trf("{} object(s) dropped", &[&n]));
                        let mut schemas: Vec<_> = self.objects.iter().map(|o| o.schema_ref()).collect();
                        schemas.dedup();
                        for s in schemas {
                            cx.push(AppCommand::RefreshMetadata { connection: self.conn.clone(), object: None, schema: Some(s) });
                        }
                        return false;
                    }
                    Err(e) => self.error = Some(e.full_text()),
                }
            }
        }
        let Some(live) = cx.conns.live(&self.conn) else { return false };
        let dialect = live.dialect.clone();
        let session = live.main.clone();
        let read_only = live.config.read_only;
        let statements: Vec<String> = self.objects.iter().map(|o| drop_sql(dialect.as_ref(), o, self.cascade)).collect();
        let mut keep = true;
        let busy = self.job.is_some();
        let modal = egui::Modal::new(egui::Id::new(("drop_objects", &self.conn))).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(format!("{} {}", icon::TRASH, tr("Drop objects")));
            ui.label(trf("Drop {} object(s)? This cannot be undone.", &[&self.objects.len()]));
            ui.checkbox(&mut self.cascade, tr("Cascade (also drop dependent objects)"));
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                let text = statements.iter().map(|s| format!("{s};\n")).collect::<String>();
                let mut t = text.as_str();
                ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
            });
            if read_only {
                crate::widgets::error_box(ui, tr("The connection is read-only."));
            }
            if let Some(e) = &self.error {
                crate::widgets::error_box(ui, e);
            }
            ui.horizontal(|ui| {
                let danger = crate::theme::error_color(ui.visuals().dark_mode);
                if ui.add_enabled(!busy && !read_only, egui::Button::new(RichText::new(tr("Drop")).color(danger))).clicked() {
                    let stmts = statements.clone();
                    let s = session.clone();
                    self.error = None;
                    self.job = Some(cx.backend.spawn(async move {
                        let mut n = 0;
                        for sql in &stmts {
                            s.execute_update(sql, &[]).await.map_err(|e| e.with_context(sql.clone()))?;
                            n += 1;
                        }
                        Ok(n)
                    }));
                }
                if ui.button(tr("Open in SQL editor")).clicked() {
                    let text = statements.iter().map(|s| format!("{s};\n")).collect::<String>();
                    cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: self.objects.first().map(|o| o.schema_ref()), text: Some(text) });
                    keep = false;
                }
                if ui.add_enabled(!busy, egui::Button::new(tr("Cancel"))).clicked() {
                    keep = false;
                }
                if busy {
                    ui.spinner();
                }
            });
        });
        if modal.should_close() && !busy {
            keep = false;
        }
        keep
    }
}
