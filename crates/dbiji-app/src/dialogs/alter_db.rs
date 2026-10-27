//! Edit database (and Rename…): its properties read from the server, the
//! ones the product can change, the exact statements, then execution on a
//! short-lived session in auto-commit mode (PostgreSQL moves a database to
//! another tablespace outside of a transaction) and a navigator refresh.

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use dbiji_core::driver::TransactionMode;
use dbiji_core::{DatabaseProps, DbKind, DbResult, Value, tr, trf};
use egui::RichText;
use egui_phosphor::regular as icon;

pub struct AlterDatabaseDialog {
    conn: String,
    /// As read from the server; `None` while loading.
    original: Option<DatabaseProps>,
    edited: DatabaseProps,
    load: Option<Job<DbResult<DatabaseProps>>>,
    job: Option<Job<DbResult<()>>>,
    error: Option<String>,
    /// Rename…: the name field takes the focus.
    focus_name: bool,
    /// PostgreSQL: end the other sessions on the database first (it is renamed or
    /// moved only when nobody uses it).
    disconnect_others: bool,
}

impl AlterDatabaseDialog {
    /// Database `name` of `conn`; `rename`: opened with Rename… (the name focused).
    pub fn new(conn: String, name: String, rename: bool) -> Self {
        let edited = DatabaseProps { name, ..Default::default() };
        AlterDatabaseDialog { conn, original: None, edited, load: None, job: None, error: None, focus_name: rename, disconnect_others: false }
    }

    /// Statements for the changes made so far.
    pub fn statements(&self, dialect: &dyn dbiji_core::Dialect) -> Vec<String> {
        self.original.as_ref().map(|o| dialect.alter_database_sql(o, &self.edited)).unwrap_or_default()
    }

    /// The properties as read (tests).
    pub fn original(&self) -> Option<&DatabaseProps> {
        self.original.as_ref()
    }

    /// The properties being edited (tests and the dialog).
    pub fn edited_mut(&mut self) -> &mut DatabaseProps {
        &mut self.edited
    }

    /// Draw; returns false when closed.
    pub fn show(&mut self, ctx: &egui::Context, cx: &mut AppContext) -> bool {
        let Some(live) = cx.conns.live(&self.conn) else { return false };
        let dialect = live.dialect.clone();
        let name = self.original.as_ref().map_or_else(|| self.edited.name.clone(), |o| o.name.clone());
        // Read the properties once (on the connection's session: they are the server's).
        if self.original.is_none() && self.load.is_none() && self.error.is_none() {
            match dialect.database_properties_sql(&name) {
                Some(sql) => {
                    let s = live.main.clone();
                    let n = name.clone();
                    self.load = Some(cx.backend.spawn(async move {
                        let (_, rows) = s.query_all(&sql, &[]).await?;
                        let row = rows.into_iter().next().ok_or_else(|| dbiji_core::DbError::query(trf("Database {} not found", &[&n])))?;
                        Ok(DatabaseProps::from_row(&n, &row))
                    }));
                }
                None => {
                    self.original = Some(self.edited.clone());
                }
            }
        }
        if let Some(j) = self.load.as_mut()
            && let Some(r) = j.poll()
        {
            self.load = None;
            match r {
                Ok(p) => {
                    self.edited = p.clone();
                    self.original = Some(p);
                }
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        if let Some(j) = self.job.as_mut()
            && let Some(r) = j.poll()
        {
            self.job = None;
            match r {
                Ok(()) => {
                    cx.toasts.success(trf("Database {} saved", &[&self.edited.name.trim()]));
                    cx.push(AppCommand::RefreshMetadata { connection: self.conn.clone(), object: None, schema: None });
                    return false;
                }
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        let Some(live) = cx.conns.live(&self.conn) else { return false };
        let kind = live.config.kind;
        let read_only = live.config.read_only;
        let conn_name = live.config.name.clone();
        let edits = dialect.database_edits();
        let statements = self.statements(dialect.as_ref());
        let renamed = self.original.as_ref().is_some_and(|o| self.edited.name.trim() != o.name && !self.edited.name.trim().is_empty());
        let moved = self.original.as_ref().is_some_and(|o| o.tablespace != self.edited.tablespace);
        let postgres = kind == DbKind::Postgres;
        // DBiji's own connection uses the database: PostgreSQL renames none while it is connected.
        let in_use = postgres && renamed && live.current_database.as_deref() == Some(name.as_str());
        let busy = self.job.is_some();
        let mut keep = true;
        let mut save = false;
        let error = self.error.clone();
        let can = !statements.is_empty() && !busy && !read_only && !in_use;
        let modal = egui::Modal::new(egui::Id::new(("alter_database", &self.conn))).show(ctx, |ui| {
            ui.set_width(560.0);
            crate::widgets::icon_heading(ui, icon::DATABASE, trf("Database {}", &[&name]));
            ui.weak(trf("On {}", &[&conn_name]));
            ui.add_space(6.0);
            match self.original.clone() {
                Some(original) => self.form(ui, &original, kind, edits, &mut save),
                None if error.is_none() => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(tr("Reading the database's properties…"));
                    });
                }
                None => {}
            }
            if postgres && (renamed || moved) {
                ui.checkbox(&mut self.disconnect_others, tr("Disconnect the other sessions on this database first"))
                    .on_hover_text(tr("PostgreSQL renames or moves a database only while nobody is connected to it."));
            }
            if in_use {
                crate::widgets::error_box(
                    ui,
                    tr("The connection itself uses this database, so PostgreSQL cannot rename it: choose another database in the connection's settings first."),
                );
            }
            if self.original.is_some() {
                ui.add_space(6.0);
                if statements.is_empty() {
                    ui.weak(tr("No changes."));
                } else {
                    egui::ScrollArea::vertical().id_salt(("alter_db_sql", &self.conn)).max_height(140.0).show(ui, |ui| {
                        let text = statements.iter().map(|s| format!("{s};\n")).collect::<String>();
                        let mut t = text.as_str();
                        ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_rows(2).desired_width(f32::INFINITY));
                    });
                }
            }
            if read_only {
                crate::widgets::error_box(ui, tr("The connection is read-only."));
            }
            if let Some(e) = &error {
                crate::widgets::error_box(ui, e);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can, egui::Button::new(RichText::new(format!("{} {}", icon::CHECK, tr("Save"))).strong())).clicked() {
                    save = true;
                }
                if ui.add_enabled(!statements.is_empty(), egui::Button::new(tr("Open in SQL editor"))).clicked() {
                    let text = statements.iter().map(|s| format!("{s};\n")).collect::<String>();
                    cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema: None, text: Some(text) });
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
        if save && can {
            self.start(cx, &name, statements, postgres && self.disconnect_others);
        }
        if modal.should_close() && !busy {
            keep = false;
        }
        keep
    }

    /// The fields: editable where the product can change them, else shown as read.
    fn form(&mut self, ui: &mut egui::Ui, original: &DatabaseProps, kind: DbKind, edits: dbiji_core::DatabaseEdits, save: &mut bool) {
        egui::Grid::new("alter_db_form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Name"));
            if edits.name {
                let r = ui.add(egui::TextEdit::singleline(&mut self.edited.name).desired_width(320.0));
                if std::mem::take(&mut self.focus_name) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    *save = true;
                }
            } else {
                ui.label(&original.name).on_hover_text(tr("This database type cannot rename a database."));
            }
            ui.end_row();
            let field = |ui: &mut egui::Ui, label: &str, v: &mut Option<String>, editable: bool, shown: bool| {
                if !editable && !shown {
                    return;
                }
                ui.label(label);
                if editable {
                    let mut text = v.clone().unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut text).desired_width(320.0)).changed() {
                        *v = Some(text).filter(|t| !t.trim().is_empty());
                    }
                } else {
                    ui.label(v.as_deref().unwrap_or_default());
                }
                ui.end_row();
            };
            field(ui, tr("Owner"), &mut self.edited.owner, edits.owner, original.owner.is_some());
            let encoding = if matches!(kind, DbKind::MySql | DbKind::MariaDb) { tr("Character set") } else { tr("Encoding") };
            field(ui, encoding, &mut self.edited.encoding, edits.encoding, original.encoding.is_some());
            field(ui, tr("Collation"), &mut self.edited.collation, edits.collation, original.collation.is_some());
            field(ui, tr("Character type"), &mut self.edited.ctype, false, original.ctype.is_some());
            field(ui, tr("Tablespace"), &mut self.edited.tablespace, edits.tablespace && original.tablespace.is_some(), original.tablespace.is_some());
            if edits.connection_limit
                && let Some(n) = self.edited.connection_limit.as_mut()
            {
                ui.label(tr("Connection limit"));
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(n).range(-1..=i64::from(i32::MAX)));
                    ui.weak(tr("-1: no limit"));
                });
                ui.end_row();
            }
            if edits.allow_connections
                && let Some(b) = self.edited.allow_connections.as_mut()
            {
                ui.label(tr("Connections"));
                ui.checkbox(b, tr("Allow connections"));
                ui.end_row();
            }
            if edits.is_template
                && let Some(b) = self.edited.is_template.as_mut()
            {
                ui.label(tr("Template"));
                ui.checkbox(b, tr("Others can be created from it (CREATE DATABASE … TEMPLATE)"));
                ui.end_row();
            }
            if edits.comment || original.comment.is_some() {
                ui.label(tr("Comment"));
                let mut text = self.edited.comment.clone().unwrap_or_default();
                let r = ui.add_enabled(edits.comment, egui::TextEdit::multiline(&mut text).desired_rows(2).desired_width(320.0));
                if r.changed() {
                    self.edited.comment = Some(text).filter(|t| !t.trim().is_empty());
                }
                ui.end_row();
            }
            if let Some(b) = original.size_bytes {
                ui.label(tr("Size"));
                ui.label(crate::nav::long_size(b));
                ui.end_row();
            }
        });
    }

    /// Run `statements` on a session of their own (auto-commit), after closing
    /// DBiji's own session on the database (the navigator's).
    fn start(&mut self, cx: &mut AppContext, name: &str, statements: Vec<String>, disconnect_others: bool) {
        let Some(live) = cx.conns.live(&self.conn) else { return };
        let (driver, params, cfg) = (live.driver.clone(), live.params().clone(), live.config.clone());
        let own = live.db_sessions.get(name).and_then(|l| l.get()).cloned();
        let terminate = format!(
            "SELECT pg_catalog.pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity WHERE datname = {} AND pid <> pg_catalog.pg_backend_pid()",
            live.dialect.literal(&Value::text(name), None)
        );
        self.error = None;
        self.job = Some(cx.backend.spawn(async move {
            if let Some(s) = &own {
                if s.has_uncommitted_writes() {
                    return Err(dbiji_core::DbError::config(tr("The database's session has uncommitted changes (another tab, the data editor): commit or roll them back first.")));
                }
                s.close().await;
            }
            let mut s = driver.connect(&params).await?;
            let r = async {
                crate::conn::init_session(s.as_mut(), &cfg, &params).await?;
                s.set_transaction_mode(TransactionMode::Auto).await?;
                if disconnect_others {
                    dbiji_core::driver::query_all(s.as_mut(), &terminate, &[]).await.map_err(|e| e.with_context(terminate.clone()))?;
                }
                for stmt in &statements {
                    dbiji_core::driver::execute_update(s.as_mut(), stmt, &[]).await.map_err(|e| e.with_context(stmt.clone()))?;
                }
                Ok(())
            }
            .await;
            let _ = s.close().await;
            r
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::Dialect;

    fn props(name: &str) -> DatabaseProps {
        DatabaseProps {
            name: name.into(),
            owner: Some("postgres".into()),
            tablespace: Some("pg_default".into()),
            connection_limit: Some(-1),
            is_template: Some(false),
            allow_connections: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn postgres_statements_rename_last() {
        let pg = dbiji_driver_postgres::PgDialect::new();
        let old = props("telenor_mm_gemezz");
        assert!(pg.alter_database_sql(&old, &old).is_empty(), "no change");
        let mut new = old.clone();
        new.name = "Telenor MM".into();
        new.owner = Some("app".into());
        new.connection_limit = Some(20);
        new.allow_connections = Some(false);
        new.is_template = Some(true);
        new.comment = Some("it's live".into());
        new.tablespace = Some("fast".into());
        assert_eq!(
            pg.alter_database_sql(&old, &new),
            [
                "ALTER DATABASE telenor_mm_gemezz OWNER TO app",
                "ALTER DATABASE telenor_mm_gemezz SET TABLESPACE fast",
                "ALTER DATABASE telenor_mm_gemezz WITH CONNECTION LIMIT 20 ALLOW_CONNECTIONS false IS_TEMPLATE true",
                "COMMENT ON DATABASE telenor_mm_gemezz IS 'it''s live'",
                "ALTER DATABASE telenor_mm_gemezz RENAME TO \"Telenor MM\"",
            ]
        );
        // A comment emptied is removed; a blank name is no rename.
        let mut commented = old.clone();
        commented.comment = Some("x".into());
        let mut cleared = commented.clone();
        cleared.comment = None;
        cleared.name = "  ".into();
        assert_eq!(pg.alter_database_sql(&commented, &cleared), ["COMMENT ON DATABASE telenor_mm_gemezz IS NULL"]);
    }

    #[test]
    fn other_databases_change_what_they_can() {
        let old =
            DatabaseProps { name: "shop".into(), owner: Some("sa".into()), encoding: Some("utf8mb3".into()), collation: Some("utf8mb3_general_ci".into()), ..Default::default() };
        let mut new = old.clone();
        new.name = "store".into();
        new.owner = Some("app".into());
        new.encoding = Some("utf8mb4".into());
        new.collation = Some("utf8mb4_unicode_ci".into());
        new.comment = Some("c".into());
        let my = dbiji_driver_mysql::MySqlDialect::new(dbiji_driver_mysql::Flavor::MySql);
        assert_eq!(my.alter_database_sql(&old, &new), ["ALTER DATABASE shop CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"], "MySQL renames no database");
        let mut odd = new.clone();
        odd.collation = Some("x; DROP DATABASE shop".into());
        assert_eq!(my.alter_database_sql(&old, &odd), ["ALTER DATABASE shop CHARACTER SET utf8mb4"], "odd names are left out");
        let ms = dbiji_driver_mssql::MsSqlDialect::new();
        assert_eq!(
            ms.alter_database_sql(&old, &new),
            ["ALTER AUTHORIZATION ON DATABASE::shop TO app", "ALTER DATABASE shop COLLATE utf8mb4_unicode_ci", "ALTER DATABASE shop MODIFY NAME = store"]
        );
        let crdb = dbiji_driver_postgres::PgDialect::cockroach();
        assert_eq!(crdb.alter_database_sql(&old, &new), ["ALTER DATABASE shop OWNER TO app", "COMMENT ON DATABASE shop IS 'c'", "ALTER DATABASE shop RENAME TO store"]);
        let ch = dbiji_driver_clickhouse::ClickHouseDialect::new();
        assert_eq!(ch.alter_database_sql(&old, &new), ["RENAME DATABASE shop TO store"]);
        assert!(ch.database_properties_sql("shop").is_none());
    }
}
