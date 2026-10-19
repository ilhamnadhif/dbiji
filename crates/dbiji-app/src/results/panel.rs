//! One result set on screen (FR-D1..D8): the data grid with its toolbar and
//! panels, incremental fetching, server-side filter / sort, saving edits in
//! one transaction, foreign key navigation, references, charts and the
//! spatial view.

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::results::source::ResultSource;
use dbiji_core::ddl::TableDef;
use dbiji_core::meta::{ObjectKind, ObjectRef};
use dbiji_core::{ColumnMeta, DbResult, Dialect, Row, Value, tr, trf};
use dbiji_grid::model::ForeignKeyLink;
use dbiji_grid::panels::{ReferencesPanelState, Relation, RelationDirection};
use dbiji_grid::{EditTarget, GridAction, GridOptions, ReadOnlyReason, ResultModel, ResultViewState, ResultViewer, determine_editability};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Grid,
    Chart,
    Spatial,
}

struct ChartView {
    config: dbiji_visual::chart::ChartConfig,
    data: dbiji_visual::chart::ChartData,
    state: dbiji_visual::chart::ChartState,
    generation: u64,
}

struct SpatialView {
    layers: Vec<dbiji_visual::spatial::SpatialLayer>,
    state: dbiji_visual::spatial::SpatialState,
    column: usize,
}

/// Previous location of a table data view (FK navigation history).
#[derive(Clone)]
struct Location {
    table: ObjectRef,
    where_clause: Option<String>,
}

pub struct ResultPanel {
    pub conn: String,
    pub title: String,
    pub src: ResultSource,
    pub model: ResultModel,
    pub view: ResultViewState,
    /// SQL before server-side filters were applied.
    base_sql: String,
    /// Set for table data views (object editor data page, FK navigation).
    pub table: Option<ObjectRef>,
    where_clause: Option<String>,
    order_by: Option<String>,
    editability: Option<Result<EditTarget, ReadOnlyReason>>,
    foreign_keys: Vec<ForeignKeyLink>,
    relations: Vec<Relation>,
    save_preview: Option<String>,
    save_job: Option<Job<DbResult<(u64, bool)>>>,
    refs_job: Option<(usize, Vec<Value>, Job<DbResult<(Vec<ColumnMeta>, Vec<Row>)>>)>,
    virtual_key_dialog: Option<(ObjectRef, Vec<bool>)>,
    mode: Mode,
    chart: Option<ChartView>,
    spatial: Option<SpatialView>,
    back: Vec<Location>,
    forward: Vec<Location>,
    /// Last status message (`true` = error).
    pub status: Option<(String, bool)>,
    fetch_op: Option<u64>,
    executed: Instant,
    /// Dictionary lookups being loaded: (result column, job).
    dict_jobs: Vec<(usize, Job<DbResult<HashMap<String, String>>>)>,
    settings_applied: bool,
    confirm_fetch_all: bool,
}

/// Collect the rows of a store (bounded) for charts / maps.
fn rows_of(model: &ResultModel, limit: usize) -> Vec<Row> {
    let mut s = model.store().lock();
    let n = s.len().min(limit);
    (0..n).filter_map(|i| s.get(i).cloned()).collect()
}

/// Virtual model entry of a table; the schema may be missing on either side
/// (single-schema databases, unqualified result sources).
pub(crate) fn vm_lookup<'a>(vm: &'a dbiji_config::VirtualModel, obj: &ObjectRef) -> Option<&'a dbiji_config::VirtualTable> {
    vm.get_for(obj).or_else(|| {
        vm.tables.iter().find(|e| e.key.table.eq_ignore_ascii_case(&obj.name) && (e.key.schema.is_none() || obj.schema.is_none() || e.key.schema == obj.schema)).map(|e| &e.table)
    })
}

fn same_table(a: &ObjectRef, b_schema: Option<&str>, b_name: &str) -> bool {
    a.name.eq_ignore_ascii_case(b_name) && (a.schema.is_none() || b_schema.is_none() || a.schema.as_deref().map(str::to_lowercase) == b_schema.map(str::to_lowercase))
}

impl ResultPanel {
    pub fn new(conn: String, title: String, mut src: ResultSource, table: Option<ObjectRef>) -> Self {
        if let Some(t) = &table {
            // `SELECT * FROM table`: every column comes from the table.
            for c in &mut src.columns {
                if c.source.is_none() {
                    c.source = Some(dbiji_core::value::ColumnSource { database: t.database.clone(), schema: t.schema.clone(), table: t.name.clone(), column: c.name.clone() });
                }
            }
        }
        let mut model = ResultModel::new(src.columns.clone(), src.store.clone());
        model.fetch.has_more = src.has_more();
        model.fetch.fetching = src.is_fetching();
        model.fetch.elapsed = Some(src.exec_duration);
        let view = ResultViewState::new(&model);
        ResultPanel {
            conn,
            title,
            base_sql: src.sql.clone(),
            src,
            model,
            view,
            table,
            where_clause: None,
            order_by: None,
            editability: None,
            foreign_keys: Vec::new(),
            relations: Vec::new(),
            save_preview: None,
            save_job: None,
            refs_job: None,
            virtual_key_dialog: None,
            mode: Mode::Grid,
            chart: None,
            spatial: None,
            back: Vec::new(),
            forward: Vec::new(),
            status: None,
            fetch_op: None,
            executed: Instant::now(),
            dict_jobs: Vec::new(),
            settings_applied: false,
            confirm_fetch_all: false,
        }
    }

    pub fn has_changes(&self) -> bool {
        self.model.has_changes()
    }

    pub fn sql(&self) -> &str {
        &self.src.sql
    }

    fn dialect(&self, cx: &AppContext) -> Option<Arc<dyn Dialect>> {
        cx.conns.live(&self.conn).map(|l| l.dialect.clone())
    }

    fn fetch_size(cx: &AppContext) -> usize {
        cx.settings.results.effective_fetch_size() as usize
    }

    /// Keep the grid model in sync with the fetch job.
    fn poll(&mut self, cx: &mut AppContext) {
        if let Some((op, res)) = self.src.poll() {
            let state = match res {
                Ok(n) => crate::ops::OpState::Succeeded(trf("{} rows", &[&n])),
                Err(e) => crate::ops::OpState::Failed(e),
            };
            cx.ops.finish(op, state);
            if self.fetch_op == Some(op) {
                self.fetch_op = None;
            }
        }
        if !Arc::ptr_eq(self.model.store(), &self.src.store) {
            self.model.reset(self.src.columns.clone(), self.src.store.clone());
            self.model.set_foreign_keys(self.foreign_keys.clone());
            if let Some(e) = &self.editability {
                self.model.set_editability(e.clone());
            }
            self.chart = None;
            self.spatial = None;
        }
        self.model.fetch.fetching = self.src.is_fetching();
        self.model.fetch.has_more = self.src.has_more();
        self.model.fetch.error = self.src.error.as_ref().map(|e| e.full_text());
        match &self.src.total_count {
            crate::backend::Loadable::Loaded(n) => {
                self.model.fetch.total_count = Some(*n);
                self.model.fetch.counting = false;
            }
            crate::backend::Loadable::Loading(_) => self.model.fetch.counting = true,
            crate::backend::Loadable::Failed(e) => {
                self.model.fetch.counting = false;
                self.status = Some((e.full_text(), true));
            }
            crate::backend::Loadable::NotLoaded => self.model.fetch.counting = false,
        }
        if let Some(j) = self.save_job.as_mut()
            && let Some(r) = j.poll()
        {
            self.save_job = None;
            match r {
                Ok((n, committed)) => {
                    self.model.apply_saved();
                    if committed {
                        cx.toasts.success(trf("Saved: {} row(s) changed", &[&n]));
                    } else {
                        cx.toasts.info(tr("Changes applied inside the open transaction — commit to keep them."));
                    }
                }
                Err(e) => cx.toasts.error(tr("Cannot save the changes"), Some(e.full_text())),
            }
        }
        let mut done = Vec::new();
        for (i, (col, job)) in self.dict_jobs.iter_mut().enumerate() {
            if let Some(r) = job.poll() {
                match r {
                    Ok(map) => self.model.set_dictionary(*col, map),
                    Err(e) => tracing::warn!("dictionary lookup failed: {e}"),
                }
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            self.dict_jobs.remove(i);
        }
        if let Some((rel, key, job)) = self.refs_job.as_mut()
            && let Some(r) = job.poll()
        {
            let (rel, key) = (*rel, key.clone());
            self.refs_job = None;
            match r {
                Ok((cols, rows)) => self.view.references.set_related_rows(rel, key, cols, rows),
                Err(e) => self.status = Some((e.full_text(), true)),
            }
        }
    }

    /// Work out which table the result can be edited in, its keys and foreign keys.
    fn resolve_editability(&mut self, cx: &mut AppContext) {
        if self.editability.is_some() {
            return;
        }
        let read_only = cx.conns.config(&self.conn).is_some_and(|c| c.read_only);
        let mut tables: Vec<ObjectRef> = Vec::new();
        for c in self.model.columns() {
            if let Some(s) = &c.source
                && !tables.iter().any(|t| same_table(t, s.schema.as_deref(), &s.table))
            {
                tables.push(ObjectRef::table(s.schema.clone(), s.table.clone()).with_database(s.database.clone()));
            }
        }
        if tables.is_empty() {
            let e = Err(ReadOnlyReason::NoSourceTable);
            self.model.set_editability(e.clone());
            self.editability = Some(e);
            return;
        }
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&self.conn) else { return };
        let main = live.main.clone();
        let mut defs: HashMap<(Option<String>, String), Arc<TableDef>> = HashMap::new();
        let mut waiting = false;
        for t in tables.iter().take(4) {
            live.meta.request_table_def(backend, &main, t);
            match live.meta.table_defs.get(t) {
                Some(crate::backend::Loadable::Loaded(d)) => {
                    defs.insert((t.schema.clone(), t.name.to_lowercase()), d.clone());
                }
                Some(crate::backend::Loadable::Failed(_)) => {}
                _ => waiting = true,
            }
        }
        if waiting {
            return;
        }
        let virtual_model = cx.services.projects.as_ref().and_then(|p| p.virtual_model(&self.conn).ok());
        let find = |obj: &ObjectRef| -> Option<Arc<TableDef>> {
            defs.iter()
                .find(|((s, n), _)| n.eq_ignore_ascii_case(&obj.name) && (obj.schema.is_none() || s.is_none() || s.as_deref() == obj.schema.as_deref()))
                .map(|(_, d)| d.clone())
        };
        let key_lookup = |obj: &ObjectRef| -> Option<Vec<String>> {
            if let Some(vk) = virtual_model.as_ref().and_then(|m| vm_lookup(m, obj)).and_then(|v| v.virtual_key.clone()).filter(|k| !k.is_empty()) {
                return Some(vk);
            }
            let d = find(obj)?;
            if let Some(pk) = &d.primary_key {
                return Some(pk.columns.clone());
            }
            d.unique_keys.first().map(|k| k.columns.clone())
        };
        let e = determine_editability(self.model.columns(), key_lookup, read_only);
        // Foreign keys of the (first) source table, mapped to result columns.
        let col_index =
            |name: &str| -> Option<usize> { self.model.columns().iter().position(|c| c.source.as_ref().map(|s| s.column.as_str()).unwrap_or(&c.name).eq_ignore_ascii_case(name)) };
        let base = match &e {
            Ok(t) => Some(t.table.clone()),
            Err(_) => tables.first().cloned().filter(|_| tables.len() == 1),
        };
        let mut links = Vec::new();
        let mut relations = Vec::new();
        if let Some(def) = base.as_ref().and_then(find) {
            for fk in &def.foreign_keys {
                let cols: Option<Vec<usize>> = fk.columns.iter().map(|c| col_index(c)).collect();
                if let Some(cols) = cols {
                    links.push(ForeignKeyLink {
                        name: fk.name.clone(),
                        columns: cols,
                        ref_table: ObjectRef::table(fk.ref_schema.clone().or(def.schema.clone()), fk.ref_table.clone()),
                        ref_columns: fk.ref_columns.clone(),
                    });
                }
            }
            relations = ReferencesPanelState::relations_from_foreign_keys(&links);
            for fk in &def.referenced_by {
                let cols: Option<Vec<usize>> = fk.ref_columns.iter().map(|c| col_index(c)).collect();
                if let Some(key_columns) = cols {
                    relations.push(Relation {
                        name: fk.name.clone(),
                        direction: RelationDirection::ReferencedBy,
                        table: ObjectRef::table(fk.schema.clone().or(def.schema.clone()), fk.table.clone()),
                        columns: fk.columns.clone(),
                        key_columns,
                    });
                }
            }
        }
        // Virtual model: virtual columns of the base table and dictionaries of referenced tables.
        if let Some(vm) = &virtual_model {
            if let Some(vt) = base.as_ref().and_then(|b| vm_lookup(vm, b)) {
                for vc in &vt.virtual_columns {
                    if let Err(err) = self.model.add_virtual_column(&vc.name, &vc.expression) {
                        tracing::warn!("virtual column {}: {err:?}", vc.name);
                    }
                }
            }
            for link in links.iter().filter(|l| l.columns.len() == 1) {
                let Some(dict) = vm_lookup(vm, &link.ref_table).and_then(|v| v.dictionary.clone()).filter(|d| !d.description_columns.is_empty()) else { continue };
                let d = live.dialect.clone();
                let key_col = link.ref_columns[0].clone();
                let mut cols = vec![d.quote_ident(&key_col)];
                cols.extend(dict.description_columns.iter().map(|c| d.quote_ident(c)));
                let sql = d.limit_query(&format!("SELECT {} FROM {}", cols.join(", "), d.qualified_ref(&link.ref_table)), 20_000, 0);
                let s = live.main.clone();
                let col = link.columns[0];
                self.dict_jobs.push((
                    col,
                    backend.spawn(async move {
                        let (_, rows) = s.query_all(&sql, &[]).await?;
                        Ok(rows
                            .into_iter()
                            .filter_map(|r| {
                                let k = r.first()?.to_text().into_owned();
                                let v = r.iter().skip(1).map(|v| v.to_text().into_owned()).collect::<Vec<_>>().join(" ");
                                Some((k, v))
                            })
                            .collect())
                    }),
                ));
            }
        }
        self.model.set_editability(e.clone());
        self.model.set_foreign_keys(links.clone());
        self.view.references.set_relations(relations.clone());
        self.foreign_keys = links;
        self.relations = relations;
        self.editability = Some(e);
    }

    fn where_for(dialect: &dyn Dialect, columns: &[String], values: &[Value]) -> String {
        columns
            .iter()
            .zip(values)
            .map(|(c, v)| if v.is_null() { format!("{} IS NULL", dialect.quote_ident(c)) } else { format!("{} = {}", dialect.quote_ident(c), dialect.literal(v, None)) })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    fn current_sql(&self, dialect: &dyn Dialect) -> String {
        match &self.table {
            Some(t) => dialect.select_table(t, None, self.where_clause.as_deref(), self.order_by.as_deref(), None),
            None => {
                let base = self.base_sql.trim().trim_end_matches(';');
                if self.where_clause.is_none() && self.order_by.is_none() {
                    return self.base_sql.clone();
                }
                let mut sql = format!("SELECT * FROM ({base}) dbiji_q");
                if let Some(w) = &self.where_clause {
                    sql.push_str(" WHERE ");
                    sql.push_str(w);
                }
                if let Some(o) = &self.order_by {
                    sql.push_str(" ORDER BY ");
                    sql.push_str(o);
                }
                sql
            }
        }
    }

    fn requery(&mut self, cx: &mut AppContext) {
        let Some(d) = self.dialect(cx) else { return };
        let sql = self.current_sql(d.as_ref());
        self.src.requery(cx.backend, sql, Self::fetch_size(cx));
        self.executed = Instant::now();
    }

    /// Show another table (FK navigation inside a table data view).
    fn navigate(&mut self, cx: &mut AppContext, to: Location, record: bool) {
        if record && let Some(t) = &self.table {
            self.back.push(Location { table: t.clone(), where_clause: self.where_clause.clone() });
            self.forward.clear();
        }
        self.title = to.table.name.clone();
        self.table = Some(to.table);
        self.where_clause = to.where_clause.clone();
        self.order_by = None;
        self.view.toolbar.filter_text = to.where_clause.unwrap_or_default();
        self.view.toolbar.order_text.clear();
        self.editability = None;
        self.foreign_keys.clear();
        self.requery(cx);
    }

    fn handle(&mut self, action: GridAction, cx: &mut AppContext) {
        let backend = cx.backend;
        match action {
            GridAction::DefaultFilter { where_clause, order_by } => {
                let Some(table) = self.table.clone() else { return };
                let Some(p) = cx.services.projects.as_ref() else { return };
                let mut vm = p.virtual_model(&self.conn).unwrap_or_default();
                let key = dbiji_config::project::VirtualTableKey::of(&table);
                let mut vt = vm.get(&key).cloned().unwrap_or_default();
                let reset = where_clause.is_none() && order_by.is_none();
                vt.default_filter = where_clause;
                vt.default_order = order_by;
                vm.set(key, vt);
                match p.save_virtual_model(&self.conn, &vm) {
                    Ok(()) => cx.toasts.success(if reset { tr("Default filter removed") } else { tr("Default filter saved") }),
                    Err(e) => cx.toasts.error(tr("Cannot save the default filter"), Some(e.to_string())),
                }
            }
            GridAction::FetchMore => self.src.fetch_more(backend, Self::fetch_size(cx), None),
            GridAction::FetchAll => {
                let limit = cx.settings.results.confirm_fetch_all_above;
                let known_small = self.model.fetch.total_count.is_some_and(|t| t <= limit);
                if limit > 0 && !known_small && !self.confirm_fetch_all {
                    self.confirm_fetch_all = true;
                } else {
                    self.fetch_all(cx);
                }
            }
            GridAction::CountTotal => {
                if let Some(d) = self.dialect(cx) {
                    self.src.count_total(backend, d);
                }
            }
            GridAction::Refresh => {
                self.src.refresh(backend, Self::fetch_size(cx));
                self.executed = Instant::now();
            }
            GridAction::SaveChanges => {
                let Some(d) = self.dialect(cx) else { return };
                let confirm = cx.conns.type_of(&self.conn).confirm_data_change;
                if confirm {
                    self.save_preview = Some(self.model.change_preview_sql(d.as_ref()));
                } else {
                    self.start_save(cx);
                }
            }
            GridAction::ChangesDiscarded | GridAction::SelectionChanged | GridAction::Copied { .. } | GridAction::SwitchView(_) | GridAction::TogglePanel(_) => {}
            GridAction::OpenValuePanel { .. } | GridAction::AutoRefreshChanged(_) => {}
            GridAction::ServerQueryChanged { where_clause, order_by } => {
                if let Some(w) = where_clause {
                    self.where_clause = Some(w).filter(|w| !w.trim().is_empty());
                }
                if let Some(o) = order_by {
                    self.order_by = Some(o).filter(|o| !o.trim().is_empty());
                }
                self.requery(cx);
            }
            GridAction::NavigateForeignKey { link, values } => {
                let Some(d) = self.dialect(cx) else { return };
                let w = Self::where_for(d.as_ref(), &link.ref_columns, &values);
                let target = Location { table: link.ref_table.clone(), where_clause: Some(w) };
                if self.table.is_some() {
                    self.navigate(cx, target, true);
                } else {
                    cx.push(AppCommand::OpenTab(Box::new(crate::results::data_tab::TableDataTab::new(self.conn.clone(), target.table, target.where_clause))));
                }
            }
            GridAction::RequestReferences { relation, values } => {
                let (Some(rel), Some(d), Some(live)) = (self.relations.get(relation).cloned(), self.dialect(cx), cx.conns.live(&self.conn)) else { return };
                let w = Self::where_for(d.as_ref(), &rel.columns, &values);
                let sql = d.select_table(&rel.table, None, Some(&w), None, Some(200));
                let s = live.main.clone();
                self.refs_job = Some((relation, values, backend.spawn(async move { s.query_all(&sql, &[]).await })));
            }
            GridAction::PasteRequested => match read_clipboard() {
                Some(text) => {
                    if let Err(e) = dbiji_grid::DataGrid::paste_text(&mut self.model, &mut self.view.grid, &text) {
                        self.status = Some((e, true));
                    }
                }
                None => self.status = Some((tr("Use ⌘V to paste into the grid.").to_string(), false)),
            },
            GridAction::Export { rows, columns } => {
                let rows = rows.map(|r| crate::tabs::transfer::grid_store_rows(&self.model, &r));
                let cols: Vec<usize> = columns.into_iter().filter(|c| *c < self.model.columns().len()).collect();
                let tab = crate::tabs::transfer::ExportTab::from_result(
                    self.conn.clone(),
                    self.title.clone(),
                    self.src.store.clone(),
                    self.model.columns().to_vec(),
                    rows,
                    cols,
                    Some(self.src.sql.clone()),
                );
                cx.push(AppCommand::OpenTab(Box::new(tab)));
            }
            GridAction::GeneratedSql { sql, .. } => self.status = Some((trf("{} characters of SQL generated", &[&sql.len()]), false)),
            GridAction::DefineVirtualKey { table } => {
                let t = table.or(self.table.clone());
                if let Some(t) = t {
                    self.virtual_key_dialog = Some((t, vec![false; self.model.columns().len()]));
                } else {
                    self.status = Some((tr("The result does not come from a single table.").to_string(), true));
                }
            }
            GridAction::ShowChart => self.mode = Mode::Chart,
            GridAction::ShowSpatial => {
                let cols = dbiji_visual::spatial::geometry_columns(self.model.columns());
                if cols.is_empty() {
                    self.status = Some((tr("The result has no geometry column.").to_string(), true));
                } else {
                    self.mode = Mode::Spatial;
                }
            }
            GridAction::Notify { message, error } => self.status = Some((message, error)),
            GridAction::SaveValueToFile { row, col, extension } => {
                let Some(v) = self.model.cell(row, col) else { return };
                let bytes: Vec<u8> = match &v {
                    Value::Bytes(b) => b.to_vec(),
                    other => other.to_text().into_owned().into_bytes(),
                };
                if let Some(path) = rfd::FileDialog::new().set_file_name(format!("value.{extension}")).save_file() {
                    match std::fs::write(&path, bytes) {
                        Ok(()) => cx.toasts.success(trf("Saved to {}", &[&path.display()])),
                        Err(e) => cx.toasts.error(tr("Cannot write the file"), Some(e.to_string())),
                    }
                }
            }
        }
    }

    fn fetch_all(&mut self, cx: &mut AppContext) {
        let op = cx.ops.start(trf("Fetching all rows: {}", &[&self.title]));
        self.fetch_op = Some(op.id);
        self.src.fetch_more(cx.backend, usize::MAX, Some(op));
    }

    fn start_save(&mut self, cx: &mut AppContext) {
        let Some(d) = self.dialect(cx) else { return };
        let stmts = self.model.change_statements(d.as_ref());
        if stmts.is_empty() {
            return;
        }
        let s = self.src.session.clone();
        self.save_job = Some(cx.backend.spawn(async move { s.apply_changes(&stmts).await }));
    }

    /// Save pending edits (shortcut ⌘S in the grid).
    pub fn save(&mut self, cx: &mut AppContext) {
        if self.model.has_changes() {
            self.handle(GridAction::SaveChanges, cx);
        }
    }

    pub fn discard(&mut self) {
        self.model.discard_changes();
    }

    fn chart_ui(&mut self, ui: &mut Ui) {
        let generation = self.src.generation;
        let cv = self.chart.get_or_insert_with(|| {
            let config = dbiji_visual::chart::ChartConfig::suggest(self.model.columns());
            ChartView { data: dbiji_visual::chart::ChartData::default(), config, state: Default::default(), generation: u64::MAX }
        });
        ui.horizontal(|ui| {
            if ui.button(format!("{} {}", icon::TABLE, tr("Back to grid"))).clicked() {
                self.mode = Mode::Grid;
            }
            if ui.button(format!("{} {}", icon::EXPORT, tr("Export chart…"))).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("chart.png").add_filter("PNG", &["png"]).add_filter("SVG", &["svg"]).save_file()
            {
                let size = egui::vec2(1200.0, 700.0);
                let res = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
                    std::fs::write(&path, dbiji_visual::chart::export_chart_svg(&cv.data, &cv.config, size)).map_err(|e| e.to_string())
                } else {
                    dbiji_visual::chart::export_chart_png(&cv.data, &cv.config, size, 2.0)
                        .map_err(|e| e.to_string())
                        .and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string()))
                };
                if let Err(e) = res {
                    self.status = Some((e, true));
                }
            }
        });
        let mut rebuild = cv.generation != generation;
        egui::Panel::left("chart_cfg").resizable(true).default_size(220.0).show(ui, |ui| {
            rebuild |= dbiji_visual::chart::chart_config_ui(ui, self.model.columns(), &mut cv.config);
        });
        if rebuild {
            let rows = rows_of(&self.model, 100_000);
            cv.data = dbiji_visual::chart::ChartData::build(self.model.columns(), &rows, &cv.config);
            cv.generation = generation;
            cv.state.reset_view = true;
        }
        egui::CentralPanel::default().show(ui, |ui| {
            dbiji_visual::chart::show_chart(ui, &cv.data, &cv.config, &mut cv.state);
        });
    }

    fn spatial_ui(&mut self, ui: &mut Ui) {
        let cols = dbiji_visual::spatial::geometry_columns(self.model.columns());
        let Some(&first) = cols.first() else {
            self.mode = Mode::Grid;
            return;
        };
        let generation = self.src.generation;
        let need = self.spatial.as_ref().is_none_or(|s| !cols.contains(&s.column));
        if need {
            self.spatial = Some(SpatialView { layers: Vec::new(), state: Default::default(), column: first });
        }
        let names: Vec<String> = self.model.columns().iter().map(|c| c.name.clone()).collect();
        let sv = self.spatial.as_mut().expect("set above");
        let mut rebuild = need || sv.layers.is_empty();
        ui.horizontal(|ui| {
            if ui.button(format!("{} {}", icon::TABLE, tr("Back to grid"))).clicked() {
                self.mode = Mode::Grid;
            }
            egui::ComboBox::from_id_salt("spatial_col").selected_text(names.get(sv.column).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                for c in &cols {
                    if ui.selectable_value(&mut sv.column, *c, names.get(*c).cloned().unwrap_or_default()).changed() {
                        rebuild = true;
                    }
                }
            });
            if ui.button(icon::ARROW_CLOCKWISE).clicked() {
                rebuild = true;
            }
        });
        let _ = generation;
        if rebuild {
            let rows = rows_of(&self.model, 200_000);
            sv.layers = vec![dbiji_visual::spatial::SpatialLayer::from_column(names.get(sv.column).cloned().unwrap_or_default(), &rows, sv.column, None)];
        }
        let r = dbiji_visual::spatial::show_spatial(ui, &sv.layers, &mut sv.state);
        if let Some(row) = r.clicked_row {
            self.status = Some((trf("Row {}", &[&(row + 1)]), false));
        }
    }

    fn dialogs(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.confirm_fetch_all {
            let mut decided: Option<bool> = None;
            let limit = cx.settings.results.confirm_fetch_all_above;
            let modal = egui::Modal::new(egui::Id::new(("fetch_all", &self.conn, &self.title))).show(ui.ctx(), |ui| {
                ui.heading(tr("Fetch all rows?"));
                match self.model.fetch.total_count {
                    Some(t) => ui.label(trf("The result has {} rows; fetching them may take a while.", &[&t])),
                    None => ui.label(trf("The result may have more than {} rows; fetching them may take a while.", &[&limit])),
                };
                ui.weak(tr("Rows beyond the memory limit are kept in a temporary file."));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Fetch all")).strong()).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        decided = Some(false);
                    }
                });
            });
            if let Some(go) = decided {
                self.confirm_fetch_all = false;
                if go {
                    self.fetch_all(cx);
                }
            } else if modal.should_close() {
                self.confirm_fetch_all = false;
            }
        }
        if let Some(sql) = self.save_preview.clone() {
            let mut close = false;
            let mut save = false;
            let modal = egui::Modal::new(egui::Id::new(("save_preview", &self.conn, &self.title))).show(ui.ctx(), |ui| {
                ui.set_width(640.0);
                ui.heading(tr("Save changes"));
                ui.label(tr("These statements will run in one transaction:"));
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    let mut t = sql.as_str();
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Save")).strong()).clicked() {
                        save = true;
                    }
                    if ui.button(tr("Copy SQL")).clicked() {
                        ui.ctx().copy_text(sql.clone());
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if save {
                self.start_save(cx);
                close = true;
            }
            if close || modal.should_close() {
                self.save_preview = None;
            }
        }
        if let Some((table, selected)) = self.virtual_key_dialog.as_mut() {
            let mut close = false;
            let mut apply = false;
            let names: Vec<String> = self.model.columns().iter().map(|c| c.name.clone()).collect();
            let modal = egui::Modal::new(egui::Id::new(("vkey", &self.conn))).show(ui.ctx(), |ui| {
                ui.heading(tr("Define a virtual key"));
                ui.label(trf("Columns that identify a row of {}:", &[&table.name]));
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for (i, n) in names.iter().enumerate() {
                        if let Some(s) = selected.get_mut(i) {
                            ui.checkbox(s, n);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(selected.iter().any(|s| *s), egui::Button::new(tr("Save"))).clicked() {
                        apply = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if apply {
                let cols: Vec<String> = names.iter().zip(selected.iter()).filter(|(_, s)| **s).map(|(n, _)| n.clone()).collect();
                if let Some(p) = cx.services.projects.as_ref() {
                    let mut vm = p.virtual_model(&self.conn).unwrap_or_default();
                    let key = dbiji_config::project::VirtualTableKey::of(table);
                    let mut vt = vm.get(&key).cloned().unwrap_or_default();
                    vt.virtual_key = Some(cols);
                    vm.set(key, vt);
                    match p.save_virtual_model(&self.conn, &vm) {
                        Ok(()) => {
                            self.editability = None;
                            cx.toasts.success(tr("Virtual key saved"));
                        }
                        Err(e) => cx.toasts.error(tr("Cannot save the virtual key"), Some(e.to_string())),
                    }
                }
                close = true;
            }
            if close || modal.should_close() {
                self.virtual_key_dialog = None;
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !self.settings_applied {
            self.settings_applied = true;
            let r = &cx.settings.results;
            self.view.grid.mode = match r.sort_filter_mode {
                dbiji_config::settings::SortFilterMode::Server => dbiji_grid::DataMode::Server,
                dbiji_config::settings::SortFilterMode::Client => dbiji_grid::DataMode::Client,
            };
            if r.auto_refresh_secs > 0 && self.view.toolbar.auto_refresh_secs.is_none() {
                self.view.toolbar.auto_refresh_secs = Some(r.auto_refresh_secs as u64);
            }
        }
        self.poll(cx);
        self.resolve_editability(cx);
        if self.table.is_some() && (!self.back.is_empty() || !self.forward.is_empty()) {
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.back.is_empty(), egui::Button::new(icon::ARROW_LEFT)).on_hover_text(tr("Back")).clicked()
                    && let Some(loc) = self.back.pop()
                {
                    if let Some(t) = &self.table {
                        self.forward.push(Location { table: t.clone(), where_clause: self.where_clause.clone() });
                    }
                    self.navigate(cx, loc, false);
                }
                if ui.add_enabled(!self.forward.is_empty(), egui::Button::new(icon::ARROW_RIGHT)).on_hover_text(tr("Forward")).clicked()
                    && let Some(loc) = self.forward.pop()
                {
                    if let Some(t) = &self.table {
                        self.back.push(Location { table: t.clone(), where_clause: self.where_clause.clone() });
                    }
                    self.navigate(cx, loc, false);
                }
                if let Some(t) = &self.table {
                    ui.strong(t.display_path());
                }
                if let Some(w) = &self.where_clause {
                    ui.weak(w);
                }
            });
        }
        match self.mode {
            Mode::Chart => self.chart_ui(ui),
            Mode::Spatial => self.spatial_ui(ui),
            Mode::Grid => {
                let dialect = self.dialect(cx);
                let options = GridOptions {
                    format: cx.settings.results.format.clone(),
                    show_row_numbers: cx.settings.results.show_row_numbers,
                    fetch_more_threshold: Self::fetch_size(cx),
                    sql_table_name: self.table.as_ref().map(|t| t.name.clone()),
                    dialect,
                    allow_editing: !cx.conns.config(&self.conn).is_some_and(|c| c.read_only),
                    ..Default::default()
                };
                let resp = ResultViewer::new(&mut self.model, &mut self.view, &options).id_salt(("result", &self.conn, &self.title)).show(ui);
                for a in resp.actions {
                    self.handle(a, cx);
                }
            }
        }
        if let Some((msg, err)) = &self.status {
            let color = if *err { crate::theme::error_color(ui.visuals().dark_mode) } else { ui.visuals().weak_text_color() };
            ui.label(RichText::new(msg).small().color(color));
        }
        self.dialogs(ui, cx);
        if self.src.is_fetching() || self.save_job.is_some() || self.refs_job.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// Clipboard text for the "Paste" menu item (⌘V is handled by egui).
fn read_clipboard() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        return out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned());
    }
    #[allow(unreachable_code)]
    None
}

/// Kind of object a data view can show.
pub fn has_data(kind: ObjectKind) -> bool {
    kind.has_data()
}

#[doc(hidden)]
impl ResultPanel {
    /// Feed a grid action (headless tests).
    pub fn handle_for_tests(&mut self, action: GridAction, cx: &mut AppContext) {
        self.handle(action, cx);
    }
}
