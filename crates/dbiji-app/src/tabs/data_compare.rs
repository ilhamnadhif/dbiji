//! Data compare (FR-M3): the rows of two tables (same or different
//! connections) matched by key columns, the differing rows with per-column
//! highlights, and the INSERT / UPDATE / DELETE script that makes the target
//! equal to the source, generated in the target dialect, opened in a SQL
//! editor or applied in one transaction.
//!
//! Both sides read through their own dedicated session: the merge join keeps
//! two cursors open at the same time, and a long comparison must not block
//! the navigator.
//!
//! Also home of the connection / schema / table pickers and the dedicated
//! session helper shared by the tool tabs.

use crate::backend::{Job, Loadable};
use crate::command::AppCommand;
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::sqllang::SqlLanguage;
use crate::tabs::TabView;
use dbiji_compare::{CompareStrategy, DataCompareJob, DataCompareOptions, DataCompareProgress, DataDiff, RowDiff, RowDiffKind, TableSpec};
use dbiji_core::ddl::TableDef;
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbResult, ErrorKind, Session, Value, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- pickers

/// Selector over all configured connections; choosing a closed connection
/// connects it. `none` adds an entry for "no connection". Returns true when
/// the selection changed.
pub(crate) fn connection_combo(ui: &mut Ui, cx: &mut AppContext, id: impl Hash + std::fmt::Debug, conn: &mut Option<String>, none: Option<&str>) -> bool {
    let names: Vec<(String, String, bool)> = cx.conns.configs().iter().map(|c| (c.id.clone(), c.name.clone(), cx.conns.is_connected(&c.id))).collect();
    let current = conn
        .as_ref()
        .and_then(|c| names.iter().find(|(i, _, _)| i == c))
        .map(|(_, n, _)| n.clone())
        .or_else(|| if conn.is_none() { none.map(str::to_string) } else { None })
        .unwrap_or_else(|| tr("Choose a connection…").to_string());
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(format!("{} {current}", icon::PLUGS)).width(190.0).show_ui(ui, |ui| {
        if let Some(n) = none
            && ui.selectable_label(conn.is_none(), n).clicked()
            && conn.is_some()
        {
            *conn = None;
            changed = true;
        }
        for (cid, name, connected) in &names {
            let st = if *connected { icon::PLUGS_CONNECTED } else { icon::PLUGS };
            if ui.selectable_label(conn.as_deref() == Some(cid), format!("{st} {name}")).clicked() && conn.as_deref() != Some(cid) {
                *conn = Some(cid.clone());
                changed = true;
            }
        }
    });
    if changed
        && let Some(c) = conn.as_ref()
        && !cx.conns.is_connected(c)
    {
        cx.push(AppCommand::Connect(c.clone()));
    }
    changed
}

/// The schema a connection works in by default.
pub(crate) fn default_schema(cx: &AppContext, conn: &str) -> Option<SchemaRef> {
    let live = cx.conns.live(conn)?;
    let name = live.current_schema.clone().or_else(|| live.current_database.clone())?;
    Some(SchemaRef::new(None, Some(name)))
}

/// Schema selector of a connected connection (non-system schemas, or the
/// current schema when the database has none). Selects the current schema
/// when nothing is chosen. Returns true when the selection changed.
pub(crate) fn schema_combo(ui: &mut Ui, cx: &mut AppContext, id: impl Hash + std::fmt::Debug, conn: &str, schema: &mut Option<SchemaRef>) -> bool {
    let backend = cx.backend;
    let fallback = default_schema(cx, conn);
    let Some(live) = cx.conns.live_mut(conn) else {
        ui.weak(tr("Not connected"));
        return false;
    };
    let main = live.main.clone();
    live.meta.request_schemas(backend, &main, None);
    let loading = live.meta.schemas_of(None).is_loading();
    let mut names: Vec<String> = live.meta.schemas_of(None).get().map(|v| v.iter().filter(|s| !s.is_system).map(|s| s.name.clone()).collect()).unwrap_or_default();
    if let Some(f) = fallback.as_ref().and_then(|f| f.schema.clone())
        && !names.contains(&f)
    {
        names.insert(0, f);
    }
    let mut changed = false;
    if schema.is_none()
        && let Some(f) = fallback.clone().or_else(|| names.first().map(|n| SchemaRef::new(None, Some(n.clone()))))
    {
        *schema = Some(f);
        changed = true;
    }
    let label = schema.as_ref().and_then(|s| s.schema.clone()).unwrap_or_else(|| tr("Schema…").to_string());
    egui::ComboBox::from_id_salt(id).selected_text(format!("{} {label}", icon::STACK)).width(150.0).show_ui(ui, |ui| {
        for n in &names {
            let on = schema.as_ref().and_then(|s| s.schema.as_deref()) == Some(n.as_str());
            if ui.selectable_label(on, n).clicked() && !on {
                *schema = Some(SchemaRef::new(None, Some(n.clone())));
                changed = true;
            }
        }
    });
    if loading {
        ui.spinner();
    }
    changed
}

/// Tables (and views when `views`) of a schema, from the metadata cache.
pub(crate) fn schema_tables(cx: &mut AppContext, conn: &str, schema: &SchemaRef, views: bool) -> (Vec<ObjectRef>, bool) {
    let backend = cx.backend;
    let Some(live) = cx.conns.live_mut(conn) else { return (Vec::new(), false) };
    let Some(session) = live.ensure_session(backend, schema.database.as_deref()) else { return (Vec::new(), true) };
    let kinds: &[ObjectKind] = if views { &[ObjectKind::Table, ObjectKind::View] } else { &[ObjectKind::Table] };
    let mut out = Vec::new();
    let mut loading = false;
    for k in kinds {
        live.meta.request_objects(backend, &session, schema, *k);
        match live.meta.objects.get(&(schema.clone(), *k)) {
            Some(Loadable::Loaded(v)) => out.extend(v.iter().map(|o| o.obj.clone())),
            Some(Loadable::Loading(_)) | Some(Loadable::NotLoaded) | None => loading = true,
            Some(Loadable::Failed(_)) => {}
        }
    }
    (out, loading)
}

/// Table / view selector of a schema. Returns true when the selection changed.
pub(crate) fn table_combo(ui: &mut Ui, cx: &mut AppContext, id: impl Hash + std::fmt::Debug, conn: &str, schema: &SchemaRef, table: &mut Option<ObjectRef>) -> bool {
    let (items, loading) = schema_tables(cx, conn, schema, true);
    let label = table.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| tr("Table…").to_string());
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(format!("{} {label}", icon::TABLE)).width(200.0).show_ui(ui, |ui| {
        if items.is_empty() {
            ui.weak(if loading { tr("Loading…") } else { tr("No tables") });
        }
        for o in &items {
            let on = table.as_ref().is_some_and(|t| t.name == o.name && t.schema == o.schema);
            if ui.selectable_label(on, format!("{} {}", crate::nav::kind_icon(o.kind), o.name)).clicked() && !on {
                *table = Some(o.clone());
                changed = true;
            }
        }
    });
    if loading {
        ui.spinner();
    }
    changed
}

/// Definition of `table` from the metadata cache (requested when missing);
/// `Err` with the error text when it cannot be read.
pub(crate) fn cached_table_def(cx: &mut AppContext, conn: &str, table: &ObjectRef) -> Result<Option<Arc<TableDef>>, String> {
    let backend = cx.backend;
    let Some(live) = cx.conns.live_mut(conn) else { return Ok(None) };
    let Some(session) = live.ensure_session(backend, table.database.as_deref()) else { return Ok(None) };
    live.meta.request_table_def(backend, &session, table);
    match live.meta.table_defs.get(table) {
        Some(Loadable::Loaded(t)) => Ok(Some(t.clone())),
        Some(Loadable::Failed(e)) => Err(e.full_text()),
        _ => Ok(None),
    }
}

// ------------------------------------------------------ dedicated sessions

/// State of a [`DedicatedSession`].
pub(crate) enum SessionState {
    Ready(SessionHandle),
    Opening,
    Failed(String),
    NotConnected,
}

/// A session of its own for a tool tab, opened on demand for a connection
/// (and database); switching the connection closes the previous session.
#[derive(Default)]
pub(crate) struct DedicatedSession {
    key: Option<(String, Option<String>)>,
    slot: Loadable<SessionHandle>,
    runtime: Option<tokio::runtime::Handle>,
}

impl DedicatedSession {
    /// Poll (and open when needed) the session for `conn` / `database`.
    pub(crate) fn poll(&mut self, cx: &AppContext, conn: &str, database: Option<&str>, label: &str) -> SessionState {
        let key = (conn.to_string(), database.map(str::to_string));
        if self.key.as_ref() != Some(&key) {
            self.close();
            self.key = Some(key);
        }
        self.slot.poll();
        if let Loadable::Loaded(s) = &self.slot {
            if !s.is_closed() {
                return SessionState::Ready(s.clone());
            }
            self.slot = Loadable::NotLoaded;
        }
        let Some(live) = cx.conns.live(conn) else { return SessionState::NotConnected };
        match &self.slot {
            Loadable::NotLoaded => {
                self.runtime = Some(cx.backend.handle());
                self.slot = Loadable::Loading(live.open_session_job(cx.backend, format!("{} ({label})", live.config.name), database.map(str::to_string)));
                SessionState::Opening
            }
            Loadable::Failed(e) => SessionState::Failed(e.full_text()),
            _ => SessionState::Opening,
        }
    }

    /// The open session, if any (no polling).
    pub(crate) fn current(&self) -> Option<SessionHandle> {
        self.slot.get().cloned()
    }

    /// Forget a failed attempt so the next poll tries again.
    pub(crate) fn retry(&mut self) {
        if matches!(self.slot, Loadable::Failed(_)) {
            self.slot = Loadable::NotLoaded;
        }
    }

    pub(crate) fn close(&mut self) {
        if let (Loadable::Loaded(s), Some(rt)) = (std::mem::take(&mut self.slot), &self.runtime) {
            rt.spawn(async move { s.close().await });
        }
    }
}

impl Drop for DedicatedSession {
    fn drop(&mut self) {
        self.close();
    }
}

// ------------------------------------------------------------ data compare

#[derive(Clone, Default, PartialEq, Debug)]
struct Side {
    conn: Option<String>,
    schema: Option<SchemaRef>,
    table: Option<ObjectRef>,
    /// Optional filter (SQL expression without WHERE).
    filter: String,
}

/// Columns of the compared rows: both queries list the key columns first,
/// then the compared columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompareLayout {
    pub keys: Vec<String>,
    pub columns: Vec<String>,
    pub target_keys: Vec<String>,
    pub target_columns: Vec<String>,
    /// Selected source columns the target table does not have.
    pub missing_in_target: Vec<String>,
}

fn find_column<'a>(t: &'a TableDef, name: &str) -> Option<&'a str> {
    t.columns
        .iter()
        .find(|c| c.name == name)
        .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
        .map(|c| c.name.as_str())
}

/// The comparison of `source` with `target`: rows matched by `keys` (source
/// column names, matched case-insensitively in the target), the other source
/// columns except `excluded` compared with the target columns of the same
/// name. Both specs list their columns explicitly so the rows of the result
/// follow [`CompareLayout`].
#[allow(clippy::too_many_arguments)]
pub fn build_compare_job(
    src_obj: &ObjectRef,
    src: &TableDef,
    dst_obj: &ObjectRef,
    dst: &TableDef,
    keys: &[String],
    excluded: &BTreeSet<String>,
    filters: (&str, &str),
    options: DataCompareOptions,
) -> Result<(DataCompareJob, CompareLayout), String> {
    // keys in table order
    let keys: Vec<String> = src.columns.iter().filter(|c| keys.contains(&c.name)).map(|c| c.name.clone()).collect();
    if keys.is_empty() {
        return Err(tr("Choose at least one key column.").to_string());
    }
    let mut target_keys = Vec::new();
    for k in &keys {
        match find_column(dst, k) {
            Some(t) => target_keys.push(t.to_string()),
            None => return Err(trf("The target table has no key column {}.", &[k])),
        }
    }
    let mut layout = CompareLayout { keys: keys.clone(), target_keys: target_keys.clone(), ..Default::default() };
    for c in &src.columns {
        if keys.contains(&c.name) || excluded.contains(&c.name) {
            continue;
        }
        match find_column(dst, &c.name) {
            Some(t) if !target_keys.iter().any(|k| k == t) => {
                layout.columns.push(c.name.clone());
                layout.target_columns.push(t.to_string());
            }
            _ => layout.missing_in_target.push(c.name.clone()),
        }
    }
    // Only keys: compare the first key with itself (rows present on one side only).
    let (src_cols, dst_cols) = if layout.columns.is_empty() {
        (vec![keys[0].clone()], vec![target_keys[0].clone()])
    } else {
        (layout.columns.clone(), layout.target_columns.clone())
    };
    let filter = |f: &str| Some(f.trim().to_string()).filter(|f| !f.is_empty());
    let job = DataCompareJob {
        source: TableSpec { obj: src_obj.clone(), key_columns: keys, columns: src_cols, where_clause: filter(filters.0) },
        target: TableSpec { obj: dst_obj.clone(), key_columns: target_keys, columns: dst_cols, where_clause: filter(filters.1) },
        options,
    };
    Ok((job, layout))
}

/// The sync statements as one script.
pub fn sync_script_text(statements: &[String]) -> String {
    let mut s = String::with_capacity(statements.iter().map(|x| x.len() + 2).sum());
    for st in statements {
        s.push_str(st);
        s.push_str(";\n");
    }
    s
}

struct Running {
    job: Job<DbResult<DataDiff>>,
    progress: Arc<Mutex<DataCompareProgress>>,
    cancel: CancellationToken,
    sessions: (SessionHandle, SessionHandle),
    layout: CompareLayout,
    started: Instant,
}

struct Outcome {
    diff: DataDiff,
    layout: CompareLayout,
    elapsed: Duration,
    target_conn: String,
    target_schema: Option<SchemaRef>,
    target_name: String,
    script: String,
    lang: SqlLanguage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultView {
    Rows,
    Script,
}

pub struct DataCompareTab {
    uid: u64,
    source: Side,
    target: Side,
    /// Source table the key / column choice was made for.
    chosen_for: Option<ObjectRef>,
    keys: Vec<String>,
    /// Source columns left out of the comparison.
    excluded: BTreeSet<String>,
    options: DataCompareOptions,
    src_session: DedicatedSession,
    dst_session: DedicatedSession,
    start_requested: bool,
    run: Option<Running>,
    result: Option<Outcome>,
    error: Option<String>,
    view: ResultView,
    filter: Option<RowDiffKind>,
    confirm_apply: bool,
    apply_job: Option<Job<DbResult<u64>>>,
}

fn kind_label(k: RowDiffKind) -> &'static str {
    match k {
        RowDiffKind::Different => tr("Different"),
        RowDiffKind::OnlyInSource => tr("Only in source"),
        RowDiffKind::OnlyInTarget => tr("Only in target"),
    }
}

fn kind_color(k: RowDiffKind, dark: bool) -> egui::Color32 {
    match k {
        RowDiffKind::Different => crate::theme::warning_color(dark),
        RowDiffKind::OnlyInSource => crate::theme::success_color(dark),
        RowDiffKind::OnlyInTarget => crate::theme::error_color(dark),
    }
}

fn strategy_label(s: CompareStrategy) -> &'static str {
    match s {
        CompareStrategy::Auto => tr("Automatic"),
        CompareStrategy::MergeJoin => tr("Merge join (sorted keys)"),
        CompareStrategy::HashJoin => tr("Hash join (in memory)"),
    }
}

impl DataCompareTab {
    pub fn new(conn: Option<String>, table: Option<ObjectRef>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let source = Side { conn: conn.clone(), schema: table.as_ref().map(|t| t.schema_ref()), table: table.clone(), filter: String::new() };
        let target = Side { conn, ..Default::default() };
        DataCompareTab {
            uid: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            source,
            target,
            chosen_for: None,
            keys: Vec::new(),
            excluded: BTreeSet::new(),
            options: DataCompareOptions { max_samples: 1000, ..Default::default() },
            src_session: DedicatedSession::default(),
            dst_session: DedicatedSession::default(),
            start_requested: false,
            run: None,
            result: None,
            error: None,
            view: ResultView::Rows,
            filter: None,
            confirm_apply: false,
            apply_job: None,
        }
    }

    fn def_of(cx: &mut AppContext, side: &Side) -> Result<Option<Arc<TableDef>>, String> {
        match (&side.conn, &side.table) {
            (Some(c), Some(t)) => cached_table_def(cx, c, t),
            _ => Ok(None),
        }
    }

    /// Default keys (primary key, else the best unique key) and all columns.
    fn init_choice(&mut self, src: &TableDef) {
        if self.chosen_for.as_ref() == self.source.table.as_ref() {
            return;
        }
        self.chosen_for = self.source.table.clone();
        self.keys = src.row_key().map(|k| k.columns.clone()).unwrap_or_default();
        self.excluded.clear();
    }

    fn side_ui(&mut self, ui: &mut Ui, cx: &mut AppContext, target: bool) {
        let uid = self.uid;
        let side = if target { &mut self.target } else { &mut self.source };
        let before = side.clone();
        let tag = if target { "dst" } else { "src" };
        let mut conn = side.conn.clone();
        if connection_combo(ui, cx, ("dc_conn", uid, tag), &mut conn, None) {
            side.conn = conn;
            side.schema = None;
            side.table = None;
        }
        if let Some(c) = side.conn.clone() {
            if cx.conns.is_connected(&c) {
                let mut schema = side.schema.clone();
                if schema_combo(ui, cx, ("dc_schema", uid, tag), &c, &mut schema) {
                    side.schema = schema;
                    side.table = None;
                }
                if let Some(s) = side.schema.clone() {
                    let mut table = side.table.clone();
                    if table_combo(ui, cx, ("dc_table", uid, tag), &c, &s, &mut table) {
                        side.table = table;
                    }
                }
            } else if cx.conns.is_connecting() {
                ui.spinner();
            } else if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                cx.push(AppCommand::Connect(c));
            }
        }
        ui.add(egui::TextEdit::singleline(&mut side.filter).hint_text(tr("WHERE filter (optional)")).desired_width(220.0).id_salt(("dc_filter", uid, tag)));
        if *side != before && before.table != side.table {
            self.result = None;
            self.error = None;
        }
    }

    /// Pick the source table's name in the target schema when nothing is chosen.
    fn auto_target_table(&mut self, cx: &mut AppContext) {
        let (Some(src), Some(conn), Some(schema), None) = (self.source.table.clone(), self.target.conn.clone(), self.target.schema.clone(), self.target.table.as_ref()) else { return };
        if !cx.conns.is_connected(&conn) || (self.source.conn.as_deref() == Some(conn.as_str()) && Some(&schema) == self.source.schema.as_ref()) {
            return;
        }
        let (items, _) = schema_tables(cx, &conn, &schema, true);
        if let Some(t) = items.iter().find(|t| t.name == src.name).or_else(|| items.iter().find(|t| t.name.eq_ignore_ascii_case(&src.name))) {
            self.target.table = Some(t.clone());
        }
    }

    fn columns_ui(&mut self, ui: &mut Ui, src: &TableDef, dst: Option<&TableDef>) {
        egui::CollapsingHeader::new(trf("Keys and columns ({} key, {} compared)", &[&self.keys.len(), &src.columns.iter().filter(|c| !self.keys.contains(&c.name) && !self.excluded.contains(&c.name)).count()]))
            .id_salt(("dc_cols", self.uid))
            .default_open(self.keys.is_empty())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button(tr("Primary key")).clicked() {
                        self.keys = src.row_key().map(|k| k.columns.clone()).unwrap_or_default();
                    }
                    if ui.small_button(tr("Compare all")).clicked() {
                        self.excluded.clear();
                    }
                    if ui.small_button(tr("Compare none")).clicked() {
                        self.excluded = src.columns.iter().map(|c| c.name.clone()).collect();
                    }
                });
                egui::ScrollArea::vertical().id_salt(("dc_cols_scroll", self.uid)).max_height(220.0).show(ui, |ui| {
                    egui::Grid::new(("dc_cols_grid", self.uid)).num_columns(5).striped(true).spacing([14.0, 2.0]).show(ui, |ui| {
                        ui.strong(tr("Column"));
                        ui.strong(tr("Type"));
                        ui.strong(tr("Key"));
                        ui.strong(tr("Compare"));
                        ui.strong(tr("In target"));
                        ui.end_row();
                        for c in &src.columns {
                            ui.label(&c.name);
                            ui.weak(&c.data_type);
                            let mut key = self.keys.contains(&c.name);
                            if ui.checkbox(&mut key, "").changed() {
                                if key {
                                    self.keys.push(c.name.clone());
                                } else {
                                    self.keys.retain(|k| k != &c.name);
                                }
                            }
                            let mut cmp = !self.excluded.contains(&c.name);
                            ui.add_enabled_ui(!key, |ui| {
                                if ui.checkbox(&mut cmp, "").changed() {
                                    if cmp {
                                        self.excluded.remove(&c.name);
                                    } else {
                                        self.excluded.insert(c.name.clone());
                                    }
                                }
                            });
                            match dst.map(|d| find_column(d, &c.name)) {
                                Some(Some(t)) if t == c.name => {
                                    ui.label(RichText::new(icon::CHECK).color(crate::theme::success_color(ui.visuals().dark_mode)));
                                }
                                Some(Some(t)) => {
                                    ui.weak(t);
                                }
                                Some(None) => {
                                    ui.label(RichText::new(tr("missing")).color(crate::theme::warning_color(ui.visuals().dark_mode)));
                                }
                                None => {
                                    ui.weak("—");
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
            });
    }

    fn options_ui(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new(tr("Options")).id_salt(("dc_opts", self.uid)).show(ui, |ui| {
            let o = &mut self.options;
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut o.ignore_case, tr("Ignore case"));
                ui.checkbox(&mut o.trim_text, tr("Ignore trailing spaces"));
                ui.label(tr("Numeric tolerance"));
                ui.add(egui::DragValue::new(&mut o.numeric_tolerance).range(0.0..=1e12).speed(0.001));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(tr("Row matching"));
                egui::ComboBox::from_id_salt(("dc_strategy", self.uid)).selected_text(strategy_label(o.strategy)).show_ui(ui, |ui| {
                    for s in [CompareStrategy::Auto, CompareStrategy::MergeJoin, CompareStrategy::HashJoin] {
                        ui.selectable_value(&mut o.strategy, s, strategy_label(s));
                    }
                });
                ui.label(tr("Differing rows to show"));
                ui.add(egui::DragValue::new(&mut o.max_samples).range(1..=1_000_000));
                ui.checkbox(&mut o.generate_sync, tr("Generate the sync script"));
            });
        });
    }

    fn try_start(&mut self, cx: &mut AppContext, src: Option<&Arc<TableDef>>, dst: Option<&Arc<TableDef>>) {
        let (Some(sc), Some(st), Some(dc), Some(dt)) = (self.source.conn.clone(), self.source.table.clone(), self.target.conn.clone(), self.target.table.clone()) else {
            self.error = Some(tr("Choose the source and the target table.").to_string());
            self.start_requested = false;
            return;
        };
        if sc == dc && st == dt && self.source.filter.trim() == self.target.filter.trim() {
            self.error = Some(tr("The source and the target are the same table.").to_string());
            self.start_requested = false;
            return;
        }
        let (Some(src), Some(dst)) = (src, dst) else { return };
        let a = self.src_session.poll(cx, &sc, st.database.as_deref(), "data compare");
        let b = self.dst_session.poll(cx, &dc, dt.database.as_deref(), "data compare");
        let (a, b) = match (a, b) {
            (SessionState::Ready(a), SessionState::Ready(b)) => (a, b),
            (SessionState::Failed(e), _) | (_, SessionState::Failed(e)) => {
                self.error = Some(e);
                self.start_requested = false;
                self.src_session.retry();
                self.dst_session.retry();
                return;
            }
            (SessionState::NotConnected, _) | (_, SessionState::NotConnected) => {
                self.error = Some(tr("Connect both connections first.").to_string());
                self.start_requested = false;
                return;
            }
            _ => return,
        };
        self.start_requested = false;
        let (job, layout) = match build_compare_job(&st, src, &dt, dst, &self.keys, &self.excluded, (&self.source.filter, &self.target.filter), self.options.clone()) {
            Ok(j) => j,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.error = None;
        self.result = None;
        let progress = Arc::new(Mutex::new(DataCompareProgress::default()));
        let cancel = CancellationToken::new();
        let (p, token) = (progress.clone(), cancel.clone());
        let repaint = cx.backend.repainter();
        let (s1, s2) = (a.clone(), b.clone());
        let handle = cx.backend.spawn(async move {
            let (sd, dd) = (s1.dialect(), s2.dialect());
            let mut ga = s1.lock().await;
            let mut gb = s2.lock().await;
            let mut sp: &mut dyn Session = ga.as_mut();
            let mut tp: &mut dyn Session = gb.as_mut();
            let mut on_progress = move |pr: &DataCompareProgress| {
                *p.lock() = *pr;
                repaint.request();
            };
            dbiji_compare::run_data_compare(&job, &mut sp, &mut tp, sd.as_ref(), dd.as_ref(), &mut on_progress, &token).await
        });
        self.run = Some(Running { job: handle, progress, cancel, sessions: (a, b), layout, started: Instant::now() });
    }

    fn stop(&mut self, cx: &AppContext) {
        if let Some(r) = &self.run {
            r.cancel.cancel();
            let (a, b) = r.sessions.clone();
            cx.backend.spawn_detached(async move {
                let _ = a.cancel().await;
                let _ = b.cancel().await;
            });
        }
    }

    fn poll(&mut self, cx: &mut AppContext) {
        if let Some(r) = self.run.as_mut()
            && let Some(res) = r.job.poll()
        {
            let r = self.run.take().expect("running");
            match res {
                Ok(diff) => {
                    let dialect = r.sessions.1.dialect();
                    let script = sync_script_text(&diff.sync_statements);
                    self.result = Some(Outcome {
                        diff,
                        layout: r.layout,
                        elapsed: r.started.elapsed(),
                        target_conn: self.target.conn.clone().unwrap_or_default(),
                        target_schema: self.target.schema.clone(),
                        target_name: self.target.table.as_ref().map(|t| t.display_path()).unwrap_or_default(),
                        script,
                        lang: SqlLanguage::new(dialect),
                    });
                }
                Err(e) if e.kind == ErrorKind::Cancelled => self.error = Some(tr("The comparison was cancelled.").to_string()),
                Err(e) => self.error = Some(e.full_text()),
            }
        }
        if let Some(j) = self.apply_job.as_mut()
            && let Some(res) = j.poll()
        {
            self.apply_job = None;
            match res {
                Ok(n) => {
                    cx.toasts.success(trf("{} statement(s) applied to the target", &[&n]));
                    if let (Some(c), Some(t)) = (&self.target.conn, &self.target.table) {
                        cx.push(AppCommand::RefreshMetadata { connection: c.clone(), object: Some(t.clone()), schema: None });
                    }
                    // Compare again: the tables should now be equal.
                    self.start_requested = true;
                }
                Err(e) => {
                    self.error = Some(e.full_text());
                    cx.toasts.error(tr("The sync script failed; nothing was changed"), Some(e.full_text()));
                }
            }
        }
    }

    /// Run the sync script on the target in one transaction.
    fn apply(&mut self, cx: &mut AppContext) {
        let Some(o) = &self.result else { return };
        let Some(session) = self.dst_session.current() else {
            self.error = Some(tr("The target session is closed; compare again first.").to_string());
            return;
        };
        let stmts: Vec<(String, Vec<Value>)> = o.diff.sync_statements.iter().map(|s| (s.clone(), Vec::new())).collect();
        let n = stmts.len() as u64;
        self.apply_job = Some(cx.backend.spawn(async move {
            let (_, committed) = session.apply_changes(&stmts).await?;
            if !committed {
                session.commit().await?;
            }
            Ok(n)
        }));
    }

    fn progress_ui(&mut self, ui: &mut Ui, cx: &AppContext) {
        let Some(r) = &self.run else { return };
        let p = *r.progress.lock();
        let elapsed = r.started.elapsed();
        let mut stop = false;
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(trf("{} source rows, {} target rows read; {} differences so far", &[&p.source_rows, &p.target_rows, &(p.different + p.only_in_source + p.only_in_target)]));
            ui.weak(crate::widgets::format_duration(elapsed));
            if ui.button(RichText::new(format!("{} {}", icon::STOP, tr("Stop"))).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                stop = true;
            }
        });
        if stop {
            self.stop(cx);
        }
    }

    fn summary_ui(ui: &mut Ui, o: &Outcome) {
        let d = &o.diff;
        let dark = ui.visuals().dark_mode;
        ui.horizontal_wrapped(|ui| {
            if d.is_identical() {
                ui.label(RichText::new(format!("{} {}", icon::CHECK_CIRCLE, tr("The data is identical."))).color(crate::theme::success_color(dark)).strong());
            }
            ui.label(trf("{} equal", &[&d.identical]));
            ui.label(RichText::new(trf("{} different", &[&d.different])).color(kind_color(RowDiffKind::Different, dark)));
            ui.label(RichText::new(trf("{} only in source", &[&d.only_in_source])).color(kind_color(RowDiffKind::OnlyInSource, dark)));
            ui.label(RichText::new(trf("{} only in target", &[&d.only_in_target])).color(kind_color(RowDiffKind::OnlyInTarget, dark)));
            ui.weak(trf("({} source rows, {} target rows, {})", &[&d.source_rows, &d.target_rows, &crate::widgets::format_duration(o.elapsed)]));
            ui.weak(strategy_label(d.strategy));
        });
        for w in &d.warnings {
            ui.label(RichText::new(format!("{} {w}", icon::WARNING)).color(crate::theme::warning_color(dark)));
        }
        if d.skipped_null_keys > 0 {
            ui.label(RichText::new(trf("{} rows with a NULL key were skipped", &[&d.skipped_null_keys])).color(crate::theme::warning_color(dark)));
        }
        if !o.layout.missing_in_target.is_empty() {
            ui.label(RichText::new(trf("Not compared (missing in the target): {}", &[&o.layout.missing_in_target.join(", ")])).color(crate::theme::warning_color(dark)));
        }
        let shown = d.samples.len() as u64;
        let total = d.different + d.only_in_source + d.only_in_target;
        if shown < total {
            ui.weak(trf("Showing the first {} of {} differing rows.", &[&shown, &total]));
        }
    }

    fn rows_ui(&mut self, ui: &mut Ui, cx: &AppContext) {
        let Some(o) = &self.result else { return };
        let fmt = cx.settings.results.format.clone();
        let dark = ui.visuals().dark_mode;
        ui.horizontal(|ui| {
            ui.label(tr("Show:"));
            ui.selectable_value(&mut self.filter, None, tr("All"));
            for k in [RowDiffKind::Different, RowDiffKind::OnlyInSource, RowDiffKind::OnlyInTarget] {
                ui.selectable_value(&mut self.filter, Some(k), kind_label(k));
            }
        });
        let rows: Vec<&RowDiff> = o.diff.samples.iter().filter(|s| self.filter.is_none_or(|f| f == s.kind)).collect();
        if rows.is_empty() {
            ui.weak(tr("No differing rows."));
            return;
        }
        let nk = o.layout.keys.len();
        let text_of = |v: Option<&Value>| -> String {
            match v {
                Some(v) => v.display(&fmt).chars().take(200).collect(),
                None => String::new(),
            }
        };
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let highlight = crate::theme::tint(crate::theme::warning_color(dark), dark);
        let mut tb = egui_extras::TableBuilder::new(ui)
            .id_salt(("dc_rows", self.uid))
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::auto().at_least(90.0));
        let total_cols = nk + o.layout.columns.len();
        for i in 0..total_cols {
            tb = if i + 1 == total_cols {
                tb.column(egui_extras::Column::remainder().at_least(80.0).clip(true))
            } else {
                tb.column(egui_extras::Column::auto().at_least(60.0).at_most(360.0).clip(true).resizable(true))
            };
        }
        tb.header(row_h, |mut h| {
            h.col(|ui| {
                ui.strong(tr("Difference"));
            });
            for k in &o.layout.keys {
                h.col(|ui| {
                    ui.strong(format!("{} {k}", icon::KEY));
                });
            }
            for (c, t) in o.layout.columns.iter().zip(&o.layout.target_columns) {
                h.col(|ui| {
                    if c == t {
                        ui.strong(c);
                    } else {
                        ui.strong(format!("{c} → {t}"));
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let Some(s) = rows.get(row.index()) else { return };
                row.col(|ui| {
                    ui.label(RichText::new(kind_label(s.kind)).color(kind_color(s.kind, dark)).small());
                });
                for i in 0..nk {
                    row.col(|ui| {
                        ui.label(text_of(s.key.get(i)));
                    });
                }
                for (j, name) in o.layout.columns.iter().enumerate() {
                    let src = s.source.as_ref().and_then(|r| r.get(nk + j));
                    let tgt = s.target.as_ref().and_then(|r| r.get(nk + j));
                    row.col(|ui| match s.kind {
                        RowDiffKind::Different if s.columns.iter().any(|c| c == name) => {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, highlight);
                            let (a, b) = (text_of(src), text_of(tgt));
                            ui.label(RichText::new(format!("{a} → {b}")).strong()).on_hover_text(format!("{}: {a}\n{}: {b}", tr("Source"), tr("Target")));
                        }
                        RowDiffKind::OnlyInTarget => {
                            ui.label(text_of(tgt));
                        }
                        _ => {
                            ui.label(text_of(src));
                        }
                    });
                }
            });
        });
    }

    fn script_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let Some(o) = &self.result else { return };
        let dark = ui.visuals().dark_mode;
        let n = o.diff.sync_statements.len();
        ui.horizontal(|ui| {
            if ui.add_enabled(n > 0, egui::Button::new(format!("{} {}", icon::FILE_SQL, tr("Open in SQL editor")))).clicked() {
                cx.push(AppCommand::NewSqlEditor { connection: Some(o.target_conn.clone()), schema: o.target_schema.clone(), text: Some(o.script.clone()) });
            }
            if ui.add_enabled(n > 0 && self.apply_job.is_none() && self.run.is_none(), egui::Button::new(format!("{} {}", icon::PLAY, tr("Apply to target…")))).clicked() {
                self.confirm_apply = true;
            }
            if ui.add_enabled(n > 0, egui::Button::new(format!("{} {}", icon::COPY, tr("Copy")))).clicked() {
                ui.ctx().copy_text(o.script.clone());
            }
            if self.apply_job.is_some() {
                ui.spinner();
                ui.weak(tr("Applying…"));
            }
            ui.weak(trf("{} statement(s)", &[&n]));
        });
        if o.diff.sync_truncated {
            ui.label(RichText::new(trf("The script was cut off after {} statements; compare again after applying it.", &[&n])).color(crate::theme::warning_color(dark)));
        }
        if !self.options.generate_sync {
            ui.weak(tr("Generating the sync script is turned off in the options."));
        } else if n == 0 {
            ui.weak(tr("Nothing to synchronize."));
            return;
        }
        let theme = dbiji_editor::EditorTheme::from_visuals(ui.visuals());
        dbiji_editor::CodeView::new(&o.script, &o.lang, &theme).id_salt(("dc_script", self.uid)).show(ui);
    }

    fn apply_modal(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !self.confirm_apply {
            return;
        }
        let Some(o) = &self.result else {
            self.confirm_apply = false;
            return;
        };
        let target_conn = cx.conns.config(&o.target_conn).map(|c| c.name.clone()).unwrap_or_default();
        let (n, name) = (o.diff.sync_statements.len(), o.target_name.clone());
        let mut close = false;
        let mut run = false;
        let modal = egui::Modal::new(egui::Id::new(("dc_apply", self.uid))).show(ui.ctx(), |ui| {
            ui.heading(tr("Apply the sync script"));
            ui.label(trf("Run {} statement(s) on {} (\"{}\") in one transaction?", &[&n, &name, &target_conn]));
            ui.weak(tr("Rows of the target are inserted, updated and deleted to match the source."));
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Apply")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    run = true;
                    close = true;
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if run {
            self.apply(cx);
        }
        if close || modal.should_close() {
            self.confirm_apply = false;
        }
    }

    // ------------------------------------------------- headless test hooks

    #[doc(hidden)]
    pub fn set_source(&mut self, conn: &str, table: ObjectRef) {
        self.source = Side { conn: Some(conn.to_string()), schema: Some(table.schema_ref()), table: Some(table), filter: String::new() };
    }

    #[doc(hidden)]
    pub fn set_target(&mut self, conn: &str, table: ObjectRef) {
        self.target = Side { conn: Some(conn.to_string()), schema: Some(table.schema_ref()), table: Some(table), filter: String::new() };
    }

    /// Start comparing (as the Compare button).
    #[doc(hidden)]
    pub fn request_compare(&mut self) {
        self.start_requested = true;
    }

    #[doc(hidden)]
    pub fn is_busy(&self) -> bool {
        self.start_requested || self.run.is_some() || self.apply_job.is_some()
    }

    #[doc(hidden)]
    pub fn key_columns(&self) -> &[String] {
        &self.keys
    }

    #[doc(hidden)]
    pub fn diff(&self) -> Option<&DataDiff> {
        self.result.as_ref().map(|o| &o.diff)
    }

    #[doc(hidden)]
    pub fn layout(&self) -> Option<&CompareLayout> {
        self.result.as_ref().map(|o| &o.layout)
    }

    #[doc(hidden)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Apply the sync script without the confirmation dialog.
    #[doc(hidden)]
    pub fn apply_now(&mut self, cx: &mut AppContext) {
        self.apply(cx);
    }
}

impl TabView for DataCompareTab {
    fn title(&self) -> String {
        match &self.source.table {
            Some(t) => format!("{} {}", tr("Data compare"), t.name),
            None => tr("Data compare").to_string(),
        }
    }

    fn icon(&self) -> &'static str {
        icon::GIT_DIFF
    }

    fn tooltip(&self) -> Option<String> {
        let path = |s: &Side| s.table.as_ref().map(|t| t.display_path()).unwrap_or_else(|| "—".into());
        Some(format!("{} ⇄ {}", path(&self.source), path(&self.target)))
    }

    fn connection_id(&self) -> Option<&str> {
        self.target.conn.as_deref().or(self.source.conn.as_deref())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll(cx);
        egui::Grid::new(("dc_sides", self.uid)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.strong(tr("Source"));
            ui.horizontal(|ui| self.side_ui(ui, cx, false));
            ui.end_row();
            ui.strong(tr("Target"));
            ui.horizontal(|ui| self.side_ui(ui, cx, true));
            ui.end_row();
        });
        self.auto_target_table(cx);
        let src = Self::def_of(cx, &self.source.clone());
        let dst = Self::def_of(cx, &self.target.clone());
        for e in [src.as_ref().err(), dst.as_ref().err()].into_iter().flatten() {
            crate::widgets::error_box(ui, e);
        }
        let src = src.ok().flatten();
        let dst = dst.ok().flatten();
        if let Some(s) = &src {
            self.init_choice(s);
            self.columns_ui(ui, s, dst.as_deref());
        } else if self.source.table.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Reading the table definition…"));
            });
        }
        self.options_ui(ui);
        let busy = self.run.is_some() || self.apply_job.is_some();
        ui.horizontal(|ui| {
            let ready = src.is_some() && dst.is_some() && !self.keys.is_empty();
            if ui.add_enabled(ready && !busy && !self.start_requested, egui::Button::new(RichText::new(format!("{} {}", icon::GIT_DIFF, tr("Compare"))).strong())).clicked() {
                self.start_requested = true;
            }
            if ui.add_enabled(!busy, egui::Button::new(format!("{} {}", icon::ARROWS_LEFT_RIGHT, tr("Swap")))).clicked() {
                std::mem::swap(&mut self.source, &mut self.target);
                self.result = None;
                self.chosen_for = None;
            }
            if self.keys.is_empty() && src.is_some() {
                ui.label(RichText::new(tr("The source table has no primary key: choose the key columns.")).color(crate::theme::warning_color(ui.visuals().dark_mode)));
            }
            if self.start_requested {
                ui.spinner();
                ui.weak(tr("Opening the sessions…"));
            }
        });
        if self.start_requested {
            self.try_start(cx, src.as_ref(), dst.as_ref());
        }
        self.progress_ui(ui, cx);
        if let Some(e) = &self.error {
            crate::widgets::error_box(ui, e);
        }
        if let Some(o) = &self.result {
            ui.separator();
            Self::summary_ui(ui, o);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, ResultView::Rows, format!("{} {}", icon::ROWS, tr("Differences")));
                ui.selectable_value(&mut self.view, ResultView::Script, format!("{} {}", icon::FILE_SQL, tr("Sync script")));
            });
            match self.view {
                ResultView::Rows => self.rows_ui(ui, cx),
                ResultView::Script => self.script_ui(ui, cx),
            }
        }
        self.apply_modal(ui, cx);
        if busy || self.start_requested {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        self.stop(cx);
        true
    }

    crate::impl_tab_any!();
}
