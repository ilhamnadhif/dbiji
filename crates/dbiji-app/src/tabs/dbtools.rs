//! Database administration tools (FR-A1 / FR-A2): backup and restore with
//! the vendor command line tools, maintenance operations (VACUUM, ANALYZE,
//! OPTIMIZE ...), changing the password of the connection user and the
//! routine debugger. Every tool works on its own database session so that
//! a slow operation never blocks the navigator or an editor.

mod backup;
mod debugger;
mod maintenance;
mod password;

pub use backup::{BackupForm, BackupTab, RestoreForm, ToolOutcome};
pub use debugger::{DebuggerTab, RoutineArg};
pub use maintenance::{MaintenanceTab, StatementResult};
pub use password::ChangePasswordTab;

use crate::backend::{Backend, Job, Loadable};
use crate::command::AppCommand;
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use dbiji_core::driver::ServerInfo;
use dbiji_core::{DbError, DbKind, DbResult, tr, trf};
use egui::Ui;
use egui_phosphor::regular as icon;
use std::time::Duration;

/// Run blocking work (file I/O, store writes) on the blocking pool and poll
/// the result from the UI.
pub(crate) fn spawn_blocking<T, F>(backend: &Backend, f: F) -> Job<DbResult<T>>
where
    T: Send + 'static,
    F: FnOnce() -> DbResult<T> + Send + 'static,
{
    backend.spawn(async move { tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(DbError::internal(e.to_string()))) })
}

/// Command line options differ between the PostgreSQL and the MySQL tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFlavor {
    /// pg_dump / pg_restore / psql.
    Postgres,
    /// mysqldump / mysql (MySQL and MariaDB).
    MySql,
    /// sqlite3 `.dump` / `.backup` / `.restore`.
    Sqlite,
    /// Anything else: every option is shown.
    Other,
}

impl ToolFlavor {
    pub fn of(kind: DbKind) -> ToolFlavor {
        match kind {
            DbKind::Postgres | DbKind::CockroachDb => ToolFlavor::Postgres,
            DbKind::MySql | DbKind::MariaDb => ToolFlavor::MySql,
            DbKind::Sqlite => ToolFlavor::Sqlite,
            _ => ToolFlavor::Other,
        }
    }
}

/// State of a [`ToolSession`].
pub enum SessionState {
    /// Open and usable.
    Ready(SessionHandle),
    /// Being opened.
    Opening,
    /// The connection is not connected.
    Closed,
    /// Opening failed.
    Failed(String),
}

/// A dedicated session of a connection, optionally on another database of
/// the server, opened on first use and closed when dropped.
#[derive(Default)]
pub struct ToolSession {
    session: Loadable<SessionHandle>,
    database: Option<String>,
    runtime: Option<tokio::runtime::Handle>,
}

impl ToolSession {
    /// Session on `database` (`None` = the connection's database).
    pub fn new(database: Option<String>) -> Self {
        ToolSession { session: Loadable::NotLoaded, database, runtime: None }
    }

    /// Poll the session, opening it when needed.
    pub fn state(&mut self, cx: &mut AppContext, conn: &str, label: &str) -> SessionState {
        self.session.poll();
        if let Loadable::Loaded(s) = &self.session {
            if !s.is_closed() {
                return SessionState::Ready(s.clone());
            }
            self.session = Loadable::NotLoaded;
        }
        let Some(live) = cx.conns.live(conn) else { return SessionState::Closed };
        match &self.session {
            Loadable::NotLoaded => {
                self.runtime = Some(cx.backend.handle());
                self.session = Loadable::Loading(live.open_session_job(cx.backend, format!("{} ({label})", live.config.name), self.database.clone()));
                SessionState::Opening
            }
            Loadable::Failed(e) => SessionState::Failed(e.full_text()),
            _ => SessionState::Opening,
        }
    }

    /// The session once open. Otherwise draws a connect button, a spinner
    /// or the error with a retry button.
    pub fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext, conn: &str, label: &str) -> Option<SessionHandle> {
        match self.state(cx, conn, label) {
            SessionState::Ready(s) => Some(s),
            SessionState::Opening => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Opening a session…"));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                None
            }
            SessionState::Closed => {
                ui.horizontal(|ui| {
                    ui.label(tr("The connection is closed."));
                    if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                        cx.push(AppCommand::Connect(conn.to_string()));
                    }
                });
                None
            }
            SessionState::Failed(e) => {
                crate::widgets::error_box(ui, &e);
                if ui.button(format!("{} {}", icon::ARROW_CLOCKWISE, tr("Retry"))).clicked() {
                    self.session = Loadable::NotLoaded;
                }
                None
            }
        }
    }
}

impl Drop for ToolSession {
    fn drop(&mut self) {
        if let (Loadable::Loaded(s), Some(rt)) = (&self.session, &self.runtime) {
            let s = s.clone();
            rt.spawn(async move { s.close().await });
        }
    }
}

/// Split a command line into arguments: whitespace separates, single and
/// double quotes group (inside double quotes `\"` and `\\` are escapes).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' && matches!(chars.peek(), Some('"') | Some('\\')) {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    in_arg = true;
                }
                c if c.is_whitespace() => {
                    if in_arg {
                        out.push(std::mem::take(&mut cur));
                        in_arg = false;
                    }
                }
                c => {
                    cur.push(c);
                    in_arg = true;
                }
            },
        }
    }
    if in_arg {
        out.push(cur);
    }
    out
}

/// Inverse of [`split_args`].
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Comma or newline separated list (schema and table names).
pub fn split_list(s: &str) -> Vec<String> {
    s.split([',', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// Inverse of [`split_list`].
pub fn join_list(items: &[String]) -> String {
    items.join(", ")
}

/// First line printed by `<program> --version` (`None` when the tool cannot
/// be started).
pub async fn tool_version(program: String) -> Option<String> {
    let run = tokio::process::Command::new(&program).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(10), run).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// Major version in a `--version` line (`pg_dump (PostgreSQL) 17.2`,
/// `mysqldump  Ver 8.0.36 ...`, `mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB`).
pub fn version_major(line: &str) -> Option<u32> {
    let s = match line.find("Distrib ") {
        Some(i) => &line[i + "Distrib ".len()..],
        None => line,
    };
    let (major, _, _) = dbiji_core::driver::parse_version_number(s);
    (major > 0).then_some(major)
}

/// Warning shown when the client tool is newer than the server: its dumps
/// may contain settings the server does not know.
pub fn newer_tool_warning(tool: &str, tool_line: &str, server: &ServerInfo) -> Option<String> {
    let tool_major = version_major(tool_line)?;
    let server_major = server.version_number.0;
    if server_major == 0 {
        return None;
    }
    let maria = |s: &str| s.to_ascii_lowercase().contains("mariadb");
    if maria(tool_line) != (maria(&server.product) || maria(&server.version)) {
        // MySQL tools against MariaDB (or the reverse): versions are not comparable
        return None;
    }
    (tool_major > server_major).then(|| {
        trf(
            "{} {} is newer than the server ({}). Dumps written by it can contain settings the server does not know; restoring them reports errors about unknown settings, which are skipped.",
            &[&tool, &tool_major, &server_major],
        )
    })
}

/// Lines of a tool's output that report errors.
pub fn error_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| {
            let u = l.to_ascii_lowercase();
            (u.contains("error:") || u.starts_with("error ")) && !u.contains("errors ignored on restore")
        })
        .cloned()
        .collect()
}

/// The error is about a setting the server does not know (a dump written
/// by a newer tool version).
pub fn is_unknown_setting_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("unrecognized configuration parameter") || l.contains("unknown system variable")
}

/// File dialog starting in the directory of `path` (when it exists).
pub(crate) fn dialog_near(path: &str) -> rfd::FileDialog {
    let mut d = rfd::FileDialog::new();
    let p = std::path::Path::new(path.trim());
    if let Some(dir) = p.parent().filter(|d| d.is_dir()) {
        d = d.set_directory(dir);
    }
    if let Some(name) = p.file_name() {
        d = d.set_file_name(name.to_string_lossy());
    }
    d
}

/// Home directory of the user (default location of backups).
pub(crate) fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Name usable as a file name.
pub(crate) fn file_safe(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    let out = out.trim_matches('_').to_string();
    if out.is_empty() { "dbiji".to_string() } else { out }
}

/// Connection picker over the configured connections.
pub(crate) fn connection_combo(ui: &mut Ui, cx: &AppContext, id: impl std::hash::Hash + std::fmt::Debug, value: &mut String, width: f32) -> bool {
    let current =
        cx.conns.config(value).map(|c| c.name.clone()).unwrap_or_else(|| if value.is_empty() { tr("Choose…").to_string() } else { tr("(missing connection)").to_string() });
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(current).width(width).show_ui(ui, |ui| {
        for c in cx.conns.configs() {
            let label = format!("{} {}", crate::nav::db_icon(c.kind), c.name);
            if ui.selectable_label(*value == c.id, label).clicked() && *value != c.id {
                *value = c.id.clone();
                changed = true;
            }
        }
    });
    changed
}

/// Schema name input with a menu of the schemas of a connected connection.
pub(crate) fn schema_field(ui: &mut Ui, cx: &mut AppContext, id: impl std::hash::Hash + std::fmt::Debug + Copy, conn: &str, value: &mut String) -> bool {
    let mut changed = ui.add(egui::TextEdit::singleline(value).desired_width(160.0).hint_text(tr("(default)"))).changed();
    if cx.conns.is_connected(conn) {
        let backend = cx.backend;
        if let Some(live) = cx.conns.live_mut(conn) {
            let r = ui.menu_button(icon::CARET_DOWN, |ui| {
                let main = live.main.clone();
                live.meta.request_schemas(backend, &main, None);
                match live.meta.schemas_of(None).get() {
                    Some(list) => {
                        egui::ScrollArea::vertical().id_salt((id, "schemas")).max_height(280.0).show(ui, |ui| {
                            for s in list.iter().filter(|s| !s.is_system) {
                                if ui.selectable_label(*value == s.name, &s.name).clicked() {
                                    *value = s.name.clone();
                                    changed = true;
                                    ui.close();
                                }
                            }
                        });
                    }
                    None => {
                        ui.spinner();
                    }
                }
            });
            r.response.on_hover_text(tr("Choose a schema"));
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_roundtrip() {
        assert_eq!(split_args(r#"--a "b c" 'd e' f\g "x\"y""#), vec!["--a", "b c", "d e", "f\\g", "x\"y"]);
        let args = vec!["--file=C:\\dumps\\a b.sql".to_string(), "plain".into(), "".into(), "it's".into()];
        assert_eq!(split_args(&join_args(&args)), args);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_list("public, sales\n  hr ,"), vec!["public", "sales", "hr"]);
    }

    #[test]
    fn versions_and_errors() {
        assert_eq!(version_major("pg_dump (PostgreSQL) 17.2 (Homebrew)"), Some(17));
        assert_eq!(version_major("mysqldump  Ver 8.0.36 for Linux on x86_64"), Some(8));
        assert_eq!(version_major("mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for Linux"), Some(10));
        let server = ServerInfo { product: "PostgreSQL".into(), version: "16.4".into(), version_number: (16, 4, 0), session_id: None };
        assert!(newer_tool_warning("pg_dump", "pg_dump (PostgreSQL) 17.2", &server).is_some());
        assert!(newer_tool_warning("pg_dump", "pg_dump (PostgreSQL) 16.1", &server).is_none());
        let maria = ServerInfo { product: "MariaDB".into(), version: "11.4.2-MariaDB".into(), version_number: (11, 4, 2), session_id: None };
        assert!(newer_tool_warning("mysqldump", "mysqldump  Ver 8.0.36 for Linux", &maria).is_none());
        let lines: Vec<String> = [
            "pg_restore: creating TABLE \"public.t\"",
            "pg_restore: error: could not execute query: ERROR:  unrecognized configuration parameter \"transaction_timeout\"",
            "Command was: SET transaction_timeout = 0;",
            "pg_restore: warning: errors ignored on restore: 1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let errors = error_lines(&lines);
        assert_eq!(errors.len(), 1);
        assert!(errors.iter().all(|l| is_unknown_setting_error(l)));
        assert!(is_unknown_setting_error("ERROR 1193 (HY000) at line 17: Unknown system variable 'foo'"));
        assert!(!is_unknown_setting_error("psql:/tmp/x.sql:3: ERROR:  relation \"t\" already exists"));
        assert_eq!(file_safe("my db/prod"), "my_db_prod");
    }
}
