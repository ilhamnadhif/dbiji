//! Metadata and data search (FR-N5).
//!
//! * Metadata search: objects whose name (optionally comment) matches a
//!   pattern, across one or all connected connections
//!   ([`dbiji_core::Session::search_metadata`]); results open the object
//!   editor.
//! * Data search: a text in the text columns of chosen tables of a schema,
//!   counted up to a row limit per table on a dedicated, cancellable session;
//!   results open the table data filtered to the matching rows.

use crate::backend::Job;
use crate::command::{AppCommand, ObjectPage};
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::tabs::data_compare::{DedicatedSession, SessionState, connection_combo, schema_combo};
use dbiji_core::ddl::SchemaSnapshot;
use dbiji_core::meta::{MetadataSearch, ObjectInfo, ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbError, DbKind, DbResult, Dialect, ErrorKind, Value, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Object kinds offered by the metadata search.
const KINDS: &[ObjectKind] = &[
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::Type,
    ObjectKind::Schema,
];

/// Default row limit of the data search per table.
pub const DEFAULT_ROW_LIMIT: u64 = 1000;

// ------------------------------------------------------- data search SQL

/// `text` as a `LIKE` pattern matching values that contain it (`\` escapes).
fn like_contains(text: &str, kind: DbKind) -> String {
    let mut s = String::with_capacity(text.len() + 2);
    s.push('%');
    for c in text.chars() {
        let special = matches!(c, '%' | '_' | '\\') || (c == '[' && kind == DbKind::SqlServer);
        if special {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// Condition true when column `column` (unquoted name) contains `text`,
/// written for the dialect: `ILIKE` (PostgreSQL), `LOWER(..) LIKE` (MySQL,
/// SQL Server, Oracle, ANSI), `instr` (SQLite) or `position` (ClickHouse).
pub fn match_predicate(d: &dyn Dialect, column: &str, text: &str, case_sensitive: bool) -> String {
    let kind = d.kind();
    let c = d.quote_ident(column);
    let lit = |s: &str| d.string_literal(s);
    let esc = lit("\\");
    match kind {
        DbKind::Postgres | DbKind::CockroachDb => {
            format!("CAST({c} AS TEXT) {} {} ESCAPE {esc}", if case_sensitive { "LIKE" } else { "ILIKE" }, lit(&like_contains(text, kind)))
        }
        DbKind::MySql | DbKind::MariaDb => {
            let p = lit(&like_contains(text, kind));
            if case_sensitive { format!("CAST({c} AS BINARY) LIKE CAST({p} AS BINARY) ESCAPE {esc}") } else { format!("LOWER({c}) LIKE LOWER({p}) ESCAPE {esc}") }
        }
        DbKind::SqlServer => {
            let p = lit(&like_contains(text, kind));
            if case_sensitive {
                format!("CAST({c} AS NVARCHAR(MAX)) COLLATE Latin1_General_BIN LIKE {p} ESCAPE {esc}")
            } else {
                format!("LOWER(CAST({c} AS NVARCHAR(MAX))) LIKE LOWER({p}) ESCAPE {esc}")
            }
        }
        DbKind::Sqlite => {
            if case_sensitive {
                format!("instr({c}, {}) > 0", lit(text))
            } else {
                format!("instr(lower({c}), lower({})) > 0", lit(text))
            }
        }
        DbKind::ClickHouse => {
            if case_sensitive {
                format!("position(toString({c}), {}) > 0", lit(text))
            } else {
                format!("positionCaseInsensitiveUTF8(toString({c}), {}) > 0", lit(text))
            }
        }
        // Data search is offered for SQL connections only; the others get the
        // portable form.
        DbKind::Oracle | DbKind::Cassandra | DbKind::Redis | DbKind::MongoDb | DbKind::Elasticsearch | DbKind::Neo4j | DbKind::Generic => {
            let p = lit(&like_contains(text, kind));
            if case_sensitive { format!("{c} LIKE {p} ESCAPE {esc}") } else { format!("LOWER({c}) LIKE LOWER({p}) ESCAPE {esc}") }
        }
    }
}

/// `(a matches OR b matches ...)`, the filter of the matching rows.
pub fn data_where_clause(d: &dyn Dialect, columns: &[String], text: &str, case_sensitive: bool) -> String {
    let parts: Vec<String> = columns.iter().map(|c| match_predicate(d, c, text, case_sensitive)).collect();
    format!("({})", parts.join(" OR "))
}

/// Query counting the matching rows of `table`, at most `limit`.
pub fn data_count_sql(d: &dyn Dialect, table: &ObjectRef, columns: &[String], text: &str, case_sensitive: bool, limit: u64) -> String {
    let wc = data_where_clause(d, columns, text, case_sensitive);
    let first = columns.first().cloned().unwrap_or_default();
    let select = d.select_table(table, Some(std::slice::from_ref(&first)), Some(&wc), None, None);
    d.count_query(&d.limit_query(&select, limit.max(1), 0))
}

/// Text columns of a snapshot's tables and views.
fn text_columns(snap: &SchemaSnapshot) -> Vec<(ObjectRef, Vec<String>)> {
    let tables = snap
        .tables
        .iter()
        .map(|t| (ObjectRef::table(snap.schema.clone(), t.name.clone()), t.columns.iter().filter(|c| c.kind.is_textual()).map(|c| c.name.clone()).collect::<Vec<_>>()));
    let views = snap.views.iter().map(|v| {
        let kind = if v.materialized { ObjectKind::MaterializedView } else { ObjectKind::View };
        (ObjectRef::new(kind, snap.schema.clone(), v.name.clone()), v.columns.iter().filter(|c| c.kind.is_textual()).map(|c| c.name.clone()).collect::<Vec<_>>())
    });
    tables.chain(views).filter(|(_, c)| !c.is_empty()).collect()
}

fn count_of(v: &Value) -> u64 {
    match v {
        Value::Int(i) => (*i).max(0) as u64,
        Value::UInt(u) => *u,
        Value::Float(f) => f.max(0.0) as u64,
        other => other.to_text().trim().parse::<u64>().unwrap_or(0),
    }
}

// ------------------------------------------------------------------ state

/// One metadata search result.
#[derive(Debug, Clone)]
pub struct MetaHit {
    pub conn: String,
    pub conn_name: String,
    pub info: ObjectInfo,
}

/// Tables of the data search with matching rows (or an error).
#[derive(Debug, Clone)]
pub struct DataHit {
    pub table: ObjectRef,
    /// Matching rows, counted up to the row limit.
    pub count: u64,
    /// `count` reached the limit: there may be more.
    pub limited: bool,
    pub columns: Vec<String>,
    /// Filter selecting the matching rows (for the data editor).
    pub where_clause: String,
    pub error: Option<String>,
}

#[derive(Default)]
struct DataProgress {
    done: usize,
    total: usize,
    current: Option<String>,
    hits: Vec<DataHit>,
}

struct DataRun {
    job: Job<DbResult<()>>,
    state: Arc<Mutex<DataProgress>>,
    cancel: CancellationToken,
    session: SessionHandle,
    started: Instant,
    conn: String,
}

type MetaResults = Vec<(String, String, DbResult<Vec<ObjectInfo>>)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Metadata,
    Data,
}

pub struct SearchTab {
    uid: u64,
    mode: Mode,
    /// Connection of the metadata search (`None` = all connected) and of the data search.
    conn: Option<String>,
    // metadata search
    pattern: String,
    kinds: BTreeSet<ObjectKind>,
    case_sensitive: bool,
    search_comments: bool,
    /// Schemas chosen for a single connection.
    schemas: BTreeSet<String>,
    /// Comma separated schemas (all connections).
    schema_text: String,
    limit: usize,
    meta_job: Option<Job<MetaResults>>,
    meta_hits: Vec<MetaHit>,
    meta_errors: Vec<String>,
    meta_selected: Option<usize>,
    meta_done: bool,
    // data search
    data_schema: Option<SchemaRef>,
    text: String,
    data_case_sensitive: bool,
    row_limit: u64,
    /// Chosen tables (names) of `data_schema`; `None` = all with text columns.
    chosen: Option<BTreeSet<String>>,
    table_filter: String,
    session: DedicatedSession,
    data_requested: bool,
    data_run: Option<DataRun>,
    data_hits: Vec<DataHit>,
    /// Connection the data hits were found on (the combo may show another one by now).
    hits_conn: Option<String>,
    data_done: Option<(usize, Duration, bool)>,
    notice: Option<String>,
}

impl SearchTab {
    /// Metadata search, or the data search when `data`.
    pub fn new(conn: Option<String>, data: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        SearchTab {
            uid: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            mode: if data { Mode::Data } else { Mode::Metadata },
            conn,
            pattern: String::new(),
            kinds: MetadataSearch::default().kinds.into_iter().collect(),
            case_sensitive: false,
            search_comments: false,
            schemas: BTreeSet::new(),
            schema_text: String::new(),
            limit: 500,
            meta_job: None,
            meta_hits: Vec::new(),
            meta_errors: Vec::new(),
            meta_selected: None,
            meta_done: false,
            data_schema: None,
            text: String::new(),
            data_case_sensitive: false,
            row_limit: DEFAULT_ROW_LIMIT,
            chosen: None,
            table_filter: String::new(),
            session: DedicatedSession::default(),
            data_requested: false,
            data_run: None,
            data_hits: Vec::new(),
            hits_conn: None,
            data_done: None,
            notice: None,
        }
    }

    // --------------------------------------------------- metadata search

    fn start_metadata(&mut self, cx: &mut AppContext) {
        self.notice = None;
        let pattern = self.pattern.trim().to_string();
        if pattern.is_empty() {
            self.notice = Some(tr("Enter a name or a pattern (* and ? are wildcards).").to_string());
            return;
        }
        if self.kinds.is_empty() {
            self.notice = Some(tr("Choose at least one object kind.").to_string());
            return;
        }
        let ids: Vec<String> = match &self.conn {
            Some(c) if cx.conns.is_connected(c) => vec![c.clone()],
            Some(_) => {
                self.notice = Some(tr("The connection is closed.").to_string());
                return;
            }
            None => {
                let mut v = cx.conns.live_ids();
                v.sort();
                v
            }
        };
        let targets: Vec<(String, String, SessionHandle)> = ids.iter().filter_map(|id| cx.conns.live(id).map(|l| (id.clone(), l.config.name.clone(), l.main.clone()))).collect();
        if targets.is_empty() {
            self.notice = Some(tr("No connection is open.").to_string());
            return;
        }
        let schemas: Vec<String> = if self.conn.is_some() {
            self.schemas.iter().cloned().collect()
        } else {
            self.schema_text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        };
        let search = MetadataSearch {
            pattern,
            kinds: self.kinds.iter().copied().collect(),
            schemas,
            case_sensitive: self.case_sensitive,
            search_comments: self.search_comments,
            limit: self.limit.max(1),
        };
        self.meta_hits.clear();
        self.meta_errors.clear();
        self.meta_selected = None;
        self.meta_done = false;
        self.meta_job = Some(cx.backend.spawn(async move {
            let futs = targets.into_iter().map(|(id, name, s)| {
                let search = search.clone();
                async move {
                    let r = s.lock().await.search_metadata(&search).await;
                    (id, name, r)
                }
            });
            futures::future::join_all(futs).await
        }));
    }

    fn poll_metadata(&mut self) {
        let Some(j) = self.meta_job.as_mut() else { return };
        let Some(results) = j.poll() else { return };
        self.meta_job = None;
        self.meta_done = true;
        for (conn, conn_name, r) in results {
            match r {
                Ok(list) => self.meta_hits.extend(list.into_iter().map(|info| MetaHit { conn: conn.clone(), conn_name: conn_name.clone(), info })),
                Err(e) => self.meta_errors.push(format!("{conn_name}: {}", e.full_text())),
            }
        }
    }

    /// Open a result: table children open their table.
    fn open_hit(cx: &mut AppContext, hit: &MetaHit) {
        let obj = &hit.info.obj;
        let target = match obj.parent_table() {
            Some(mut parent) if obj.kind.is_table_child() => {
                let is_view = cx.conns.live(&hit.conn).is_some_and(|l| {
                    [ObjectKind::View, ObjectKind::MaterializedView]
                        .iter()
                        .any(|k| l.meta.objects.get(&(parent.schema_ref(), *k)).and_then(|x| x.get()).is_some_and(|v| v.iter().any(|o| o.obj.name == parent.name)))
                });
                if is_view {
                    parent.kind = ObjectKind::View;
                }
                parent
            }
            _ => obj.clone(),
        };
        cx.push(AppCommand::OpenObject { connection: hit.conn.clone(), object: target, page: ObjectPage::Properties });
    }

    fn metadata_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let mut go = false;
        ui.horizontal(|ui| {
            let r =
                ui.add(egui::TextEdit::singleline(&mut self.pattern).hint_text(tr("Name or pattern, e.g. cust* or *_id")).desired_width(260.0).id_salt(("ms_pattern", self.uid)));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            ui.label(tr("in"));
            let mut conn = self.conn.clone();
            if connection_combo(ui, cx, ("ms_conn", self.uid), &mut conn, Some(tr("All connected connections"))) {
                self.conn = conn;
                self.schemas.clear();
            }
            let busy = self.meta_job.is_some();
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Search"))).strong())).clicked() {
                go = true;
            }
            if busy {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.case_sensitive, tr("Case sensitive"));
            ui.checkbox(&mut self.search_comments, tr("Search comments"));
            ui.label(tr("Limit"));
            ui.add(egui::DragValue::new(&mut self.limit).range(1..=100_000));
            ui.separator();
            self.schema_filter_ui(ui, cx);
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Kinds:"));
            for k in KINDS {
                let mut on = self.kinds.contains(k);
                if ui.checkbox(&mut on, tr(k.plural_label())).changed() {
                    if on {
                        self.kinds.insert(*k);
                    } else {
                        self.kinds.remove(k);
                    }
                }
            }
        });
        if go {
            self.start_metadata(cx);
        }
        if let Some(n) = &self.notice {
            ui.label(RichText::new(n).color(crate::theme::warning_color(ui.visuals().dark_mode)));
        }
        for e in &self.meta_errors {
            crate::widgets::error_box(ui, e);
        }
        ui.separator();
        if self.meta_done {
            if self.meta_hits.is_empty() {
                ui.weak(tr("No objects found."));
                return;
            }
            let n = self.meta_hits.len();
            ui.horizontal(|ui| {
                ui.weak(if n >= self.limit { trf("{} objects (limit reached)", &[&n]) } else { trf("{} objects", &[&n]) });
                if let Some(i) = self.meta_selected
                    && ui.button((dbiji_icons::ic(icon::ARROW_SQUARE_OUT), tr("Open"))).clicked()
                    && let Some(h) = self.meta_hits.get(i).cloned()
                {
                    Self::open_hit(cx, &h);
                }
                ui.weak(tr("Double-click a result to open it."));
            });
        }
        if self.meta_hits.is_empty() {
            return;
        }
        let rows: Vec<Vec<String>> = self
            .meta_hits
            .iter()
            .map(|h| {
                vec![
                    h.conn_name.clone(),
                    format!("{} {}", crate::nav::kind_icon(h.info.obj.kind), tr(h.info.obj.kind.label())),
                    h.info.obj.display_path(),
                    h.info.detail.clone().unwrap_or_default(),
                    h.info.comment.clone().unwrap_or_default(),
                ]
            })
            .collect();
        let r = crate::widgets::simple_table(ui, ("ms_results", self.uid), &[tr("Connection"), tr("Kind"), tr("Object"), tr("Details"), tr("Comment")], &rows, self.meta_selected);
        if let Some(i) = r.clicked {
            self.meta_selected = Some(i);
        }
        if let Some(i) = r.double_clicked
            && let Some(h) = self.meta_hits.get(i).cloned()
        {
            Self::open_hit(cx, &h);
        }
    }

    fn schema_filter_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.label(tr("Schemas:"));
        let Some(conn) = self.conn.clone().filter(|c| cx.conns.is_connected(c)) else {
            ui.add(egui::TextEdit::singleline(&mut self.schema_text).hint_text(tr("all (or a, b, c)")).desired_width(160.0).id_salt(("ms_schemas", self.uid)));
            return;
        };
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&conn) else { return };
        let main = live.main.clone();
        live.meta.request_schemas(backend, &main, None);
        let names: Vec<String> = live.meta.schemas_of(None).get().map(|v| v.iter().filter(|s| !s.is_system).map(|s| s.name.clone()).collect()).unwrap_or_default();
        let label = if self.schemas.is_empty() { tr("All").to_string() } else { self.schemas.iter().cloned().collect::<Vec<_>>().join(", ") };
        ui.menu_button(format!("{} {label}", icon::STACK), |ui| {
            if ui.selectable_label(self.schemas.is_empty(), tr("All")).clicked() {
                self.schemas.clear();
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for n in &names {
                    let mut on = self.schemas.contains(n);
                    if ui.checkbox(&mut on, n).changed() {
                        if on {
                            self.schemas.insert(n.clone());
                        } else {
                            self.schemas.remove(n);
                        }
                    }
                }
            });
        });
    }

    // ------------------------------------------------------- data search

    fn snapshot(&self, cx: &mut AppContext) -> (Option<Arc<SchemaSnapshot>>, Option<String>) {
        let (Some(conn), Some(schema)) = (self.conn.clone(), self.data_schema.clone()) else { return (None, None) };
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&conn) else { return (None, None) };
        let main = live.main.clone();
        live.meta.request_snapshot(backend, &main, &schema);
        let err = live.meta.snapshots.get(&schema).and_then(|l| l.error()).map(|e| e.full_text());
        (live.meta.snapshot(&schema), err)
    }

    fn start_data(&mut self, cx: &mut AppContext) {
        self.notice = None;
        let text = self.text.clone();
        if text.trim().is_empty() {
            self.notice = Some(tr("Enter the text to find.").to_string());
            self.data_requested = false;
            return;
        }
        let Some(conn) = self.conn.clone() else {
            self.notice = Some(tr("Choose a connection.").to_string());
            self.data_requested = false;
            return;
        };
        let (snap, err) = self.snapshot(cx);
        if let Some(e) = err {
            self.notice = Some(e);
            self.data_requested = false;
            return;
        }
        let Some(snap) = snap else { return };
        let db = self.data_schema.as_ref().and_then(|s| s.database.clone());
        let session = match self.session.poll(cx, &conn, db.as_deref(), "search") {
            SessionState::Ready(s) => s,
            SessionState::Opening => return,
            SessionState::Failed(e) => {
                self.notice = Some(e);
                self.session.retry();
                self.data_requested = false;
                return;
            }
            SessionState::NotConnected => {
                self.notice = Some(tr("The connection is closed.").to_string());
                self.data_requested = false;
                return;
            }
        };
        self.data_requested = false;
        let tables: Vec<(ObjectRef, Vec<String>)> =
            text_columns(&snap).into_iter().filter(|(t, _)| self.chosen.as_ref().is_none_or(|c| c.contains(&t.name))).map(|(t, c)| (t.with_database(db.clone()), c)).collect();
        if tables.is_empty() {
            self.notice = Some(tr("Choose at least one table with text columns.").to_string());
            return;
        }
        let state = Arc::new(Mutex::new(DataProgress { total: tables.len(), ..Default::default() }));
        let cancel = CancellationToken::new();
        let (st, token, s) = (state.clone(), cancel.clone(), session.clone());
        let repaint = cx.backend.repainter();
        let cs = self.data_case_sensitive;
        let limit = self.row_limit.max(1);
        let job = cx.backend.spawn(async move {
            let d = s.dialect();
            let total = tables.len();
            for (i, (table, cols)) in tables.into_iter().enumerate() {
                if token.is_cancelled() {
                    return Err(DbError::cancelled());
                }
                {
                    let mut p = st.lock();
                    p.done = i;
                    p.current = Some(table.display_path());
                }
                repaint.request();
                let where_clause = data_where_clause(d.as_ref(), &cols, &text, cs);
                let sql = data_count_sql(d.as_ref(), &table, &cols, &text, cs, limit);
                let hit = match s.query_all(&sql, &[]).await {
                    Ok((_, rows)) => {
                        let n = rows.first().and_then(|r| r.first()).map(count_of).unwrap_or(0);
                        DataHit { table, count: n, limited: n >= limit, columns: cols, where_clause, error: None }
                    }
                    Err(_) if token.is_cancelled() => return Err(DbError::cancelled()),
                    Err(e) => DataHit { table, count: 0, limited: false, columns: cols, where_clause, error: Some(e.full_text()) },
                };
                if hit.count > 0 || hit.error.is_some() {
                    st.lock().hits.push(hit);
                }
            }
            let mut p = st.lock();
            p.done = total;
            p.current = None;
            Ok(())
        });
        self.data_hits.clear();
        self.data_done = None;
        self.data_run = Some(DataRun { job, state, cancel, session, started: Instant::now(), conn });
    }

    fn stop_data(&mut self, cx: &AppContext) {
        if let Some(r) = &self.data_run {
            r.cancel.cancel();
            let s = r.session.clone();
            cx.backend.spawn_detached(async move {
                let _ = s.cancel().await;
            });
        }
    }

    fn poll_data(&mut self) {
        let Some(r) = self.data_run.as_mut() else { return };
        self.data_hits = r.state.lock().hits.clone();
        self.hits_conn = Some(r.conn.clone());
        if let Some(res) = r.job.poll() {
            let p = r.state.lock();
            let cancelled = matches!(&res, Err(e) if e.kind == ErrorKind::Cancelled);
            if let Err(e) = &res
                && !cancelled
            {
                self.notice = Some(e.full_text());
            }
            self.data_done = Some((p.done, r.started.elapsed(), cancelled));
            drop(p);
            self.data_run = None;
        }
    }

    fn data_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let busy = self.data_run.is_some() || self.data_requested;
        ui.horizontal(|ui| {
            let mut conn = self.conn.clone();
            if connection_combo(ui, cx, ("ds_conn", self.uid), &mut conn, None) {
                self.conn = conn;
                self.data_schema = None;
                self.chosen = None;
            }
            if let Some(c) = self.conn.clone().filter(|c| cx.conns.is_connected(c)) {
                let mut schema = self.data_schema.clone();
                if schema_combo(ui, cx, ("ds_schema", self.uid), &c, &mut schema) {
                    self.data_schema = schema;
                    self.chosen = None;
                }
            }
        });
        let mut go = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Find"));
            let r = ui.add(egui::TextEdit::singleline(&mut self.text).hint_text(tr("Text to find in text columns")).desired_width(260.0).id_salt(("ds_text", self.uid)));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            ui.checkbox(&mut self.data_case_sensitive, tr("Case sensitive"));
            ui.label(tr("Rows per table"));
            ui.add(egui::DragValue::new(&mut self.row_limit).range(1..=10_000_000));
            if busy {
                if ui.button(RichText::new(format!("{} {}", icon::STOP, tr("Stop"))).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    self.data_requested = false;
                    self.stop_data(cx);
                }
            } else if ui.button(RichText::new(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Search"))).strong()).clicked() {
                go = true;
            }
        });
        if go && !busy {
            self.data_requested = true;
        }
        if self.data_requested {
            self.start_data(cx);
        }
        if let Some(n) = &self.notice {
            ui.label(RichText::new(n).color(crate::theme::warning_color(ui.visuals().dark_mode)));
        }
        let (snap, err) = self.snapshot(cx);
        if let Some(e) = err {
            crate::widgets::error_box(ui, &e);
        }
        let avail = ui.available_height();
        ui.columns(2, |cols| {
            self.tables_ui(&mut cols[0], snap.as_deref(), avail);
            self.data_results_ui(&mut cols[1], cx, avail);
        });
    }

    fn tables_ui(&mut self, ui: &mut Ui, snap: Option<&SchemaSnapshot>, height: f32) {
        let Some(snap) = snap else {
            if self.data_schema.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Reading the schema…"));
                });
            }
            return;
        };
        let tables = text_columns(snap);
        let chosen_count = tables.iter().filter(|(t, _)| self.chosen.as_ref().is_none_or(|c| c.contains(&t.name))).count();
        ui.horizontal(|ui| {
            ui.strong(trf("Tables ({} of {} with text columns)", &[&chosen_count, &tables.len()]));
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.table_filter).hint_text(tr("Filter…")).desired_width(140.0).id_salt(("ds_filter", self.uid)));
            if ui.small_button(tr("All")).clicked() {
                self.chosen = None;
            }
            if ui.small_button(tr("None")).clicked() {
                self.chosen = Some(BTreeSet::new());
            }
        });
        let filter = self.table_filter.to_lowercase();
        egui::ScrollArea::vertical().id_salt(("ds_tables", self.uid)).max_height((height - 60.0).max(120.0)).auto_shrink([false, true]).show(ui, |ui| {
            for (t, cols) in &tables {
                if !filter.is_empty() && !t.name.to_lowercase().contains(&filter) {
                    continue;
                }
                let mut on = self.chosen.as_ref().is_none_or(|c| c.contains(&t.name));
                let label = format!("{} {}", crate::nav::kind_icon(t.kind), t.name);
                if ui.checkbox(&mut on, label).on_hover_text(cols.join(", ")).changed() {
                    let set = self.chosen.get_or_insert_with(|| tables.iter().map(|(x, _)| x.name.clone()).collect());
                    if on {
                        set.insert(t.name.clone());
                    } else {
                        set.remove(&t.name);
                    }
                }
            }
        });
    }

    fn data_results_ui(&mut self, ui: &mut Ui, cx: &mut AppContext, height: f32) {
        if let Some(r) = &self.data_run {
            let p = r.state.lock();
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(trf("Table {} of {}", &[&(p.done + 1).min(p.total), &p.total]));
                if let Some(c) = &p.current {
                    ui.weak(c);
                }
            });
        } else if self.data_requested {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Opening a session…"));
            });
        }
        if let Some((n, d, cancelled)) = self.data_done {
            let found = self.data_hits.iter().filter(|h| h.error.is_none()).count();
            let text = if cancelled {
                trf("Stopped after {} tables; {} tables with matches", &[&n, &found])
            } else {
                trf("{} tables searched in {}; {} tables with matches", &[&n, &crate::widgets::format_duration(d), &found])
            };
            ui.weak(text);
        }
        let conn = self.data_run.as_ref().map(|r| r.conn.clone()).or_else(|| self.hits_conn.clone()).unwrap_or_default();
        let dark = ui.visuals().dark_mode;
        egui::ScrollArea::vertical().id_salt(("ds_hits", self.uid)).max_height((height - 30.0).max(120.0)).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new(("ds_hits_grid", self.uid)).num_columns(3).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                if !self.data_hits.is_empty() {
                    ui.strong(tr("Table"));
                    ui.strong(tr("Matching rows"));
                    ui.label("");
                    ui.end_row();
                }
                for h in &self.data_hits {
                    ui.label(format!("{} {}", crate::nav::kind_icon(h.table.kind), h.table.display_path())).on_hover_text(trf("Searched columns: {}", &[&h.columns.join(", ")]));
                    match &h.error {
                        Some(e) => {
                            ui.label(RichText::new(format!("{} {}", icon::WARNING, tr("error"))).color(crate::theme::error_color(dark))).on_hover_text(e);
                            ui.label("");
                        }
                        None => {
                            ui.label(if h.limited { trf("{} or more", &[&h.count]) } else { h.count.to_string() });
                            if ui.button((dbiji_icons::ic(icon::TABLE), tr("Show rows"))).clicked() {
                                let tab = crate::results::data_tab::TableDataTab::new(conn.clone(), h.table.clone(), Some(h.where_clause.clone()));
                                cx.push(AppCommand::OpenTab(Box::new(tab)));
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    // ------------------------------------------------- headless test hooks

    #[doc(hidden)]
    pub fn set_pattern(&mut self, pattern: &str) {
        self.pattern = pattern.to_string();
    }

    #[doc(hidden)]
    pub fn search_metadata_now(&mut self, cx: &mut AppContext) {
        self.start_metadata(cx);
    }

    #[doc(hidden)]
    pub fn metadata_hits(&self) -> &[MetaHit] {
        &self.meta_hits
    }

    #[doc(hidden)]
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Prepare a data search in `schema` for `text` (all tables when `tables` is empty).
    #[doc(hidden)]
    pub fn set_data_search(&mut self, schema: SchemaRef, text: &str, tables: &[&str]) {
        self.mode = Mode::Data;
        self.data_schema = Some(schema);
        self.text = text.to_string();
        self.chosen = if tables.is_empty() { None } else { Some(tables.iter().map(|t| t.to_string()).collect()) };
    }

    #[doc(hidden)]
    pub fn request_data_search(&mut self) {
        self.data_requested = true;
    }

    #[doc(hidden)]
    pub fn data_hits(&self) -> &[DataHit] {
        &self.data_hits
    }

    #[doc(hidden)]
    pub fn data_finished(&self) -> bool {
        self.data_done.is_some() && self.data_run.is_none()
    }

    #[doc(hidden)]
    pub fn is_busy(&self) -> bool {
        self.meta_job.is_some() || self.data_run.is_some() || self.data_requested
    }
}

impl TabView for SearchTab {
    fn title(&self) -> String {
        match self.mode {
            Mode::Metadata => tr("Search metadata").to_string(),
            Mode::Data => tr("Search data").to_string(),
        }
    }

    fn icon(&self) -> &'static str {
        match self.mode {
            Mode::Metadata => icon::MAGNIFYING_GLASS,
            Mode::Data => icon::TEXT_AA,
        }
    }

    fn connection_id(&self) -> Option<&str> {
        self.conn.as_deref()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll_metadata();
        self.poll_data();
        ui.horizontal(|ui| {
            let before = self.mode;
            ui.selectable_value(&mut self.mode, Mode::Metadata, format!("{} {}", icon::MAGNIFYING_GLASS, tr("Metadata")));
            ui.selectable_value(&mut self.mode, Mode::Data, format!("{} {}", icon::TEXT_AA, tr("Data")));
            if before != self.mode {
                self.notice = None;
            }
        });
        ui.separator();
        match self.mode {
            Mode::Metadata => self.metadata_ui(ui, cx),
            Mode::Data => self.data_ui(ui, cx),
        }
        if self.is_busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        self.stop_data(cx);
        true
    }

    crate::impl_tab_any!();
}
