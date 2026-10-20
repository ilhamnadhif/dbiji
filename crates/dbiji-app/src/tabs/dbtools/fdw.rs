//! Foreign data wrapper configurator (PostgreSQL): generate the script that
//! makes the tables of another DBiji connection available as foreign tables
//! (CREATE EXTENSION, CREATE SERVER, CREATE USER MAPPING, IMPORT FOREIGN
//! SCHEMA). The password is never written into the script: it is the
//! `:password` parameter, asked when the script runs.

use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::config::ConnectionConfig;
use dbiji_core::{DbKind, SchemaRef, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

/// The source of the foreign tables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FdwSpec {
    pub kind: Option<DbKind>,
    /// Name of the foreign server to create.
    pub server: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    /// Database file (SQLite, DuckDB).
    pub file: Option<String>,
    /// Schema (MySQL: database) to import.
    pub remote_schema: String,
    /// Schema the foreign tables are created in.
    pub local_schema: String,
    pub create_local_schema: bool,
    /// Import only these tables (`LIMIT TO`); empty = all.
    pub tables: Vec<String>,
}

/// The extension that reads from a database kind.
pub fn wrapper_of(kind: DbKind) -> Option<&'static str> {
    match kind {
        DbKind::Postgres | DbKind::CockroachDb => Some("postgres_fdw"),
        DbKind::MySql | DbKind::MariaDb => Some("mysql_fdw"),
        DbKind::Sqlite => Some("sqlite_fdw"),
        DbKind::SqlServer => Some("tds_fdw"),
        DbKind::ClickHouse => Some("clickhouse_fdw"),
        DbKind::DuckDb => Some("duckdb_fdw"),
        DbKind::Generic => None,
    }
}

fn ident(s: &str) -> String {
    let plain = s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_') && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { s.to_string() } else { format!("\"{}\"", s.replace('"', "\"\"")) }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A lowercase identifier made from a connection name.
pub fn identifier_from(name: &str) -> String {
    let mut out: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    match out.chars().next() {
        None => "remote".into(),
        Some(c) if c.is_ascii_digit() => format!("db_{out}"),
        _ => out,
    }
}

/// Defaults for a source connection.
pub fn spec_for(source: &ConnectionConfig) -> FdwSpec {
    let name = identifier_from(&source.name);
    let remote_schema = match source.kind {
        DbKind::MySql | DbKind::MariaDb => source.database.clone().unwrap_or_default(),
        DbKind::ClickHouse => source.database.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "default".into()),
        DbKind::SqlServer => "dbo".into(),
        DbKind::Sqlite | DbKind::DuckDb => "main".into(),
        _ => "public".into(),
    };
    FdwSpec {
        kind: Some(source.kind),
        server: format!("{name}_server"),
        host: source.host.clone(),
        port: source.port.or(source.kind.default_port()),
        database: source.database.clone().filter(|d| !d.is_empty()),
        user: source.user.clone().filter(|u| !u.is_empty()),
        file: source.file_path.clone(),
        remote_schema,
        local_schema: name,
        create_local_schema: true,
        tables: Vec::new(),
    }
}

/// The configuration script.
pub fn fdw_script(spec: &FdwSpec) -> Result<String, String> {
    let kind = spec.kind.ok_or_else(|| tr("Choose the source connection").to_string())?;
    let wrapper = wrapper_of(kind).ok_or_else(|| tr("No foreign data wrapper reads this database").to_string())?;
    if spec.server.trim().is_empty() || spec.local_schema.trim().is_empty() {
        return Err(tr("Enter the server name and the local schema").to_string());
    }
    if spec.remote_schema.trim().is_empty() {
        return Err(tr("Enter the remote schema").to_string());
    }
    let server = ident(spec.server.trim());
    let local = ident(spec.local_schema.trim());
    let port = spec.port.map(|p| p.to_string()).unwrap_or_default();
    let db = spec.database.as_deref().unwrap_or("");
    let file = spec.file.as_deref().unwrap_or("");
    let mut options: Vec<(&str, String)> = match kind {
        DbKind::Postgres | DbKind::CockroachDb => vec![("host", spec.host.clone()), ("port", port), ("dbname", db.to_string())],
        DbKind::MySql | DbKind::MariaDb => vec![("host", spec.host.clone()), ("port", port)],
        DbKind::SqlServer => vec![("servername", spec.host.clone()), ("port", port), ("database", db.to_string())],
        DbKind::ClickHouse => vec![("host", spec.host.clone()), ("port", port), ("dbname", db.to_string()), ("driver", "http".into())],
        DbKind::Sqlite | DbKind::DuckDb => vec![("database", file.to_string())],
        DbKind::Generic => Vec::new(),
    };
    options.retain(|(_, v)| !v.is_empty());
    let options = options.iter().map(|(k, v)| format!("{k} {}", lit(v))).collect::<Vec<_>>().join(", ");
    let mut out = format!("-- {}\n", trf("Foreign tables of {} through {}", &[&kind.display_name(), &wrapper]));
    if !matches!(kind, DbKind::Postgres | DbKind::CockroachDb) {
        out.push_str(&format!("-- {}\n", trf("The {} extension must be installed on the server", &[&wrapper])));
    }
    out.push_str(&format!("-- {}\n\n", tr("The database server connects to the source itself: SSH tunnels and proxies of the connection are not used")));
    out.push_str(&format!("CREATE EXTENSION IF NOT EXISTS {wrapper};\n\n"));
    out.push_str(&format!("CREATE SERVER IF NOT EXISTS {server}\n    FOREIGN DATA WRAPPER {wrapper}"));
    if !options.is_empty() {
        out.push_str(&format!("\n    OPTIONS ({options})"));
    }
    out.push_str(";\n\n");
    if !matches!(kind, DbKind::Sqlite | DbKind::DuckDb) {
        let user_key = match kind {
            DbKind::MySql | DbKind::MariaDb | DbKind::SqlServer => "username",
            _ => "user",
        };
        let user = spec.user.as_deref().unwrap_or("");
        out.push_str(&format!("-- {}\n", tr(":password is asked when the script runs; enter it as a string, e.g. 'secret'")));
        out.push_str(&format!("CREATE USER MAPPING IF NOT EXISTS FOR CURRENT_USER\n    SERVER {server}\n    OPTIONS ({user_key} {}, password :password);\n\n", lit(user)));
    }
    if spec.create_local_schema {
        out.push_str(&format!("CREATE SCHEMA IF NOT EXISTS {local};\n\n"));
    }
    let tables: Vec<String> = spec.tables.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).map(ident).collect();
    out.push_str(&format!("IMPORT FOREIGN SCHEMA {}", ident(spec.remote_schema.trim())));
    if !tables.is_empty() {
        out.push_str(&format!("\n    LIMIT TO ({})", tables.join(", ")));
    }
    out.push_str(&format!("\n    FROM SERVER {server}\n    INTO {local};\n"));
    Ok(out)
}

/// The configurator tab of a PostgreSQL connection.
pub struct FdwConfigTab {
    conn: String,
    source: Option<String>,
    spec: FdwSpec,
    tables: String,
}

impl FdwConfigTab {
    pub fn new(conn: String) -> Self {
        FdwConfigTab { conn, source: None, spec: FdwSpec::default(), tables: String::new() }
    }

    /// Choose the source connection (fills the defaults).
    pub fn set_source(&mut self, source: &ConnectionConfig) {
        self.source = Some(source.id.clone());
        self.spec = spec_for(source);
        self.tables.clear();
    }

    pub fn script(&self) -> Result<String, String> {
        fdw_script(&FdwSpec { tables: self.tables.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(), ..self.spec.clone() })
    }
}

impl TabView for FdwConfigTab {
    fn popup_size(&self) -> Option<egui::Vec2> {
        Some(egui::vec2(720.0, 560.0))
    }

    fn title(&self) -> String {
        tr("Foreign data wrapper").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::PLUGS_CONNECTED
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("fdw:{}", self.conn))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        crate::widgets::icon_heading(ui, icon::PLUGS_CONNECTED, tr("Foreign data wrapper"));
        ui.weak(tr("Make the tables of another connection available in this database as foreign tables."));
        ui.add_space(6.0);
        let sources: Vec<ConnectionConfig> = cx.conns.configs().iter().filter(|c| c.id != self.conn && wrapper_of(c.kind).is_some()).cloned().collect();
        let selected = self.source.as_ref().and_then(|id| sources.iter().find(|c| &c.id == id)).map(|c| format!("{} ({})", c.name, c.kind.display_name()));
        egui::Grid::new(("fdw_form", &self.conn)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Source connection"));
            egui::ComboBox::from_id_salt(("fdw_source", &self.conn)).selected_text(selected.unwrap_or_else(|| tr("(choose)").to_string())).width(260.0).show_ui(ui, |ui| {
                for c in &sources {
                    if ui.selectable_label(self.source.as_deref() == Some(&c.id), format!("{} ({})", c.name, c.kind.display_name())).clicked() {
                        self.set_source(c);
                    }
                }
            });
            ui.end_row();
            ui.label(tr("Server name"));
            ui.add(egui::TextEdit::singleline(&mut self.spec.server).desired_width(260.0));
            ui.end_row();
            ui.label(tr("Remote schema"));
            ui.add(egui::TextEdit::singleline(&mut self.spec.remote_schema).desired_width(260.0));
            ui.end_row();
            ui.label(tr("Local schema"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.spec.local_schema).desired_width(260.0));
                ui.checkbox(&mut self.spec.create_local_schema, tr("Create it"));
            });
            ui.end_row();
            ui.label(tr("Tables"));
            ui.add(egui::TextEdit::singleline(&mut self.tables).desired_width(340.0).hint_text(tr("comma separated; empty = all")));
            ui.end_row();
        });
        ui.add_space(8.0);
        match self.script() {
            Ok(script) => {
                ui.horizontal(|ui| {
                    if ui.button((dbiji_icons::ic(icon::FILE_SQL), tr("Open in SQL editor"))).clicked() {
                        let schema = cx.conns.config(&self.conn).and_then(|c| c.database.clone()).map(|d| SchemaRef { database: Some(d), schema: None });
                        cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema, text: Some(script.clone()) });
                    }
                    if ui.button((dbiji_icons::ic(icon::COPY), tr("Copy"))).clicked() {
                        ui.ctx().copy_text(script.clone());
                    }
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| crate::tabs::code(ui, cx, ("fdw_script", &self.conn), &script));
            }
            Err(e) => {
                ui.label(RichText::new(e).weak());
            }
        }
    }

    crate::impl_tab_any!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts() {
        let mut src = ConnectionConfig::new(DbKind::Postgres, "Sales DB (prod)");
        src.host = "db.internal".into();
        src.database = Some("sales".into());
        src.user = Some("reader".into());
        let spec = spec_for(&src);
        assert_eq!((spec.server.as_str(), spec.local_schema.as_str(), spec.remote_schema.as_str(), spec.port), ("sales_db_prod_server", "sales_db_prod", "public", Some(5432)));
        let s = fdw_script(&FdwSpec { tables: vec!["orders".into(), "Line Items".into()], ..spec }).unwrap();
        assert!(s.contains("CREATE EXTENSION IF NOT EXISTS postgres_fdw;"), "{s}");
        assert!(s.contains("OPTIONS (host 'db.internal', port '5432', dbname 'sales')"), "{s}");
        assert!(s.contains("OPTIONS (user 'reader', password :password);"), "{s}");
        assert!(s.contains("IMPORT FOREIGN SCHEMA public\n    LIMIT TO (orders, \"Line Items\")\n    FROM SERVER sales_db_prod_server\n    INTO sales_db_prod;"), "{s}");

        let mut my = ConnectionConfig::new(DbKind::MySql, "shop");
        my.host = "10.0.0.5".into();
        my.database = Some("Shop".into());
        let s = fdw_script(&spec_for(&my)).unwrap();
        assert!(
            s.contains("mysql_fdw") && s.contains("OPTIONS (host '10.0.0.5', port '3306')") && s.contains("OPTIONS (username '") && s.contains("IMPORT FOREIGN SCHEMA \"Shop\""),
            "{s}"
        );

        let mut lite = ConnectionConfig::new(DbKind::Sqlite, "1 local");
        lite.file_path = Some("/data/o'k.db".into());
        let s = fdw_script(&spec_for(&lite)).unwrap();
        assert!(s.contains("OPTIONS (database '/data/o''k.db')") && !s.contains("USER MAPPING") && s.contains("INTO db_1_local;"), "{s}");

        assert!(fdw_script(&FdwSpec::default()).is_err());
        assert_eq!(identifier_from("--"), "remote");
    }
}
