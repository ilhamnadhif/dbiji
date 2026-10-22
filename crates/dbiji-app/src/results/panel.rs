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

/// Something that reloads the rows, waiting for "discard the pending changes?".
enum Deferred {
    Refresh,
    Query { where_clause: Option<String>, order_by: Option<String> },
    Navigate(Location),
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
    /// Rows saved, committed, and the values the database gave the new rows.
    save_job: Option<Job<DbResult<SaveOutcome>>>,
    /// Statistics of a whole column (its name) being computed by the server.
    column_stats: Option<(String, Job<DbResult<Vec<Value>>>)>,
    /// `src.columns_version` the model was built with.
    columns_seen: u64,
    /// Saved inside an open transaction: the session's rollback count then.
    saved_in_tx: Option<u64>,
    /// A reload asked while edits are pending (it would discard them).
    confirm_discard: Option<Deferred>,
    /// Re-running the statement of this result would change data again: asking first.
    confirm_rerun: Option<Deferred>,
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

type SaveOutcome = (u64, bool, HashMap<u64, Vec<(usize, Value)>>);

/// How a new row is read back after it was inserted without `RETURNING`.
struct ReadBack {
    /// Pending row id.
    id: u64,
    /// Its statement in the save.
    stmt: usize,
    /// Key columns (name, type) and the value given in the insert (`None`: generated).
    key: Vec<(String, String, Option<Value>)>,
}

impl ReadBack {
    /// `SELECT` of the new row by its key; one key column left out of the insert is
    /// the generated id. `None` when the key is not known.
    fn select_sql(&self, d: &dyn Dialect, table: &ObjectRef, columns: &[(usize, String, String)], generated: Option<Value>) -> Option<String> {
        let missing = self.key.iter().filter(|(_, _, v)| v.is_none()).count();
        if self.key.is_empty() || missing > 1 {
            return None;
        }
        let conds = self
            .key
            .iter()
            .map(|(name, type_name, given)| {
                let v = match given {
                    Some(v) => v.clone(),
                    None => generated.clone()?,
                };
                let q = d.quote_ident(name);
                Some(if v.is_null() { format!("{q} IS NULL") } else { format!("{q} = {}", d.literal(&v, (!type_name.is_empty()).then_some(type_name.as_str()))) })
            })
            .collect::<Option<Vec<_>>>()?;
        let cols: Vec<String> = columns.iter().map(|(_, n, _)| n.clone()).collect();
        Some(d.select_table(table, Some(&cols), Some(&conds.join(" AND ")), None, Some(1)))
    }
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
        model.fetch.fetched_at = Some(src.executed_at);
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
            column_stats: None,
            columns_seen: 0,
            saved_in_tx: None,
            confirm_discard: None,
            confirm_rerun: None,
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

    /// Id of the connection the result belongs to.
    pub fn conn(&self) -> &str {
        &self.conn
    }

    /// Edits are being saved (or their preview waits for a confirmation).
    pub fn is_saving(&self) -> bool {
        self.save_job.is_some() || self.save_preview.is_some()
    }

    /// The SQL preview of a save waits for a confirmation (shown with the result).
    pub fn awaits_confirmation(&self) -> bool {
        self.save_preview.is_some()
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
    pub(crate) fn poll(&mut self, cx: &mut AppContext) {
        if let Some(op) = self.src.take_cancelled_op() {
            cx.ops.finish(op, crate::ops::OpState::Cancelled);
            if self.fetch_op == Some(op) {
                self.fetch_op = None;
            }
        }
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
        // A new store (refresh, requery), or other columns (foreign key navigation, ALTER):
        // the model follows; other columns need their editability resolved again.
        if self.columns_seen != self.src.columns_version {
            self.columns_seen = self.src.columns_version;
            self.editability = None;
            self.foreign_keys.clear();
            self.model.reset(self.src.columns.clone(), self.src.store.clone());
            self.chart = None;
            self.spatial = None;
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
        self.model.fetch.elapsed = Some(self.src.exec_duration);
        self.model.fetch.fetched_at = Some(self.src.executed_at);
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
        if let Some((name, job)) = self.column_stats.as_mut()
            && let Some(r) = job.poll()
        {
            let title = trf("Statistics of {}", &[name]);
            match r {
                Ok(v) => cx.toasts.push(crate::toasts::ToastKind::Info, title, Some(column_stats_text(&v))),
                Err(e) => cx.toasts.error(title, Some(e.full_text())),
            }
            self.status = None;
            self.column_stats = None;
        }
        if let Some(j) = self.save_job.as_mut()
            && let Some(r) = j.poll()
        {
            self.save_job = None;
            match r {
                Ok((n, committed, inserted)) => {
                    self.src.rows_deleted(self.model.deleted_below(self.src.server_rows()));
                    self.model.apply_saved_with(&inserted);
                    self.saved_in_tx = (!committed).then(|| self.src.session.rollbacks());
                    if committed {
                        cx.toasts.success(trf("Saved: {} row(s) changed", &[&n]));
                    } else {
                        cx.toasts.info(tr("Changes applied inside the open transaction — commit to keep them."));
                    }
                }
                Err(e) => cx.toasts.error(tr("Cannot save the changes"), Some(e.full_text())),
            }
        }
        // Saved inside a transaction that was rolled back since: the rows shown are no
        // longer in the database. A commit ends the watch.
        if let Some(r) = self.saved_in_tx
            && self.save_job.is_none()
        {
            let session = self.src.session.clone();
            if session.rollbacks() != r {
                self.saved_in_tx = None;
                if self.has_changes() || !self.src.rerun_safe {
                    self.status = Some((tr("The transaction was rolled back: refresh to see the current rows.").to_string(), true));
                } else {
                    self.refresh(cx);
                }
            } else if !session.tx_state().in_transaction {
                self.saved_in_tx = None;
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
        // Not before the columns of a new query arrived (they decide the table and key).
        if self.editability.is_some() || (self.src.is_fetching() && self.src.row_count() == 0) {
            return;
        }
        let read_only = cx.conns.config(&self.conn).is_some_and(|c| c.read_only);
        // Columns without a database come from the result's one (a table of
        // another database of the server).
        let result_database = self.table.as_ref().and_then(|t| t.database.clone());
        let mut tables: Vec<ObjectRef> = Vec::new();
        for c in self.model.columns() {
            if let Some(s) = &c.source
                && !tables.iter().any(|t| same_table(t, s.schema.as_deref(), &s.table))
            {
                tables.push(ObjectRef::table(s.schema.clone(), s.table.clone()).with_database(s.database.clone().or(result_database.clone())));
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
        let mut defs: HashMap<(Option<String>, String), Arc<TableDef>> = HashMap::new();
        let mut waiting = false;
        let mut lookup_error: Option<String> = None;
        for t in tables.iter().take(4) {
            // Keys are read in the table's own database: another one does not see them.
            let Some(session) = live.ensure_session(backend, t.database.as_deref()) else {
                let failed = live.session_error(t.database.as_deref());
                waiting |= failed.is_none();
                lookup_error = lookup_error.or(failed);
                continue;
            };
            live.meta.request_table_def(backend, &session, t);
            match live.meta.table_defs.get(t) {
                Some(crate::backend::Loadable::Loaded(d)) => {
                    defs.insert((t.schema.clone(), t.name.to_lowercase()), d.clone());
                }
                Some(crate::backend::Loadable::Failed(e)) => lookup_error = lookup_error.or(Some(e.full_text())),
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
            find(obj)?.row_key_columns()
        };
        let mut e = determine_editability(self.model.columns(), key_lookup, read_only);
        // A column of the table twice is fine (`SELECT name, * FROM t`) unless the query
        // reads the table twice (`t a JOIN t b`): then a column cannot tell its row.
        if let Ok(t) = &e
            && self.table.is_none()
            && dbiji_grid::model::has_duplicate_sources(self.model.columns(), &t.table.name)
        {
            let reads = dbiji_sql::analyze(&self.src.sql, live.dialect.syntax())
                .tables
                .iter()
                .filter(|r| r.role == dbiji_sql::RefRole::Source && !r.is_cte && r.name.eq_ignore_ascii_case(&t.table.name))
                .count();
            if reads > 1 {
                e = Err(ReadOnlyReason::MultipleTables);
            }
        }
        // "No unique key" because the table could not be read: say so (no virtual key needed).
        if e == Err(ReadOnlyReason::NoKey)
            && let Some(err) = lookup_error
        {
            e = Err(ReadOnlyReason::Custom(trf("Read-only: the table definition could not be read: {}", &[&err])));
        }
        // Foreign keys of the (first) source table, mapped to result columns.
        let col_index =
            |name: &str| -> Option<usize> { self.model.columns().iter().position(|c| c.source.as_ref().map(|s| s.column.as_str()).unwrap_or(&c.name).eq_ignore_ascii_case(name)) };
        let base = match &e {
            Ok(t) => Some(t.table.clone()),
            Err(_) => tables.first().cloned().filter(|_| tables.len() == 1),
        };
        let mut links = Vec::new();
        let mut relations = Vec::new();
        // Referenced tables live in the base table's database (another database of the server too).
        let base_db = base.as_ref().and_then(|b| b.database.clone());
        let in_db = |mut o: ObjectRef| {
            o.database = base_db.clone();
            o
        };
        if let Some(def) = base.as_ref().and_then(find) {
            for fk in &def.foreign_keys {
                let cols: Option<Vec<usize>> = fk.columns.iter().map(|c| col_index(c)).collect();
                if let Some(cols) = cols {
                    links.push(ForeignKeyLink {
                        name: fk.name.clone(),
                        columns: cols,
                        ref_table: in_db(ObjectRef::table(fk.ref_schema.clone().or(def.schema.clone()), fk.ref_table.clone())),
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
                        table: in_db(ObjectRef::table(fk.schema.clone().or(def.schema.clone()), fk.table.clone())),
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
                // The shared session of the table's database (the result's own session would
                // lose its open cursor); not open yet: no dictionary this time.
                let Some(s) = live.ensure_session(backend, link.ref_table.database.as_deref()) else { continue };
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
                // On its own line: a query ending in a `-- comment` would comment out the `)`.
                let mut sql = format!("SELECT * FROM (\n{base}\n) dbiji_q");
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
            GridAction::CancelFetch => self.src.stop(backend),
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
            GridAction::ColumnStatistics { column } => {
                if let Some(d) = self.dialect(cx)
                    && self.column_stats.is_none()
                {
                    let name = self.model.column_name(column).to_string();
                    let c = d.quote_ident(&name);
                    let full = d.aggregate_query(&self.src.sql, &format!("COUNT(*), COUNT({c}), COUNT(DISTINCT {c}), MIN({c}), MAX({c})"));
                    // Types without DISTINCT, MIN or MAX (json, xml, bytea …): counts only.
                    let counts = d.aggregate_query(&self.src.sql, &format!("COUNT(*), COUNT({c})"));
                    let (session, params) = (self.src.session.clone(), self.src.params.clone());
                    let job = backend.spawn(async move {
                        let rows = match session.query_all(&full, &params).await {
                            Ok((_, rows)) => rows,
                            Err(_) => session.query_all(&counts, &params).await?.1,
                        };
                        rows.into_iter().next().ok_or_else(|| dbiji_core::DbError::query("The statistics query returned no row"))
                    });
                    self.column_stats = Some((name, job));
                    self.status = Some((tr("Computing the column's statistics…").to_string(), false));
                }
            }
            GridAction::Refresh => self.run_or_confirm(Deferred::Refresh, cx),
            GridAction::SaveChanges => {
                // One save at a time: a second one would run the same statements again.
                if self.save_job.is_some() {
                    return;
                }
                let Some(d) = self.dialect(cx) else { return };
                let confirm = cx.conns.type_of(&self.conn).confirm_data_change;
                if confirm {
                    self.save_preview = Some(self.model.change_preview_sql(d.as_ref()));
                } else {
                    self.start_save(cx);
                }
            }
            GridAction::ChangesDiscarded
            | GridAction::SelectionChanged
            | GridAction::Copied { .. }
            | GridAction::SwitchView(_)
            | GridAction::TogglePanel(_)
            | GridAction::EditFilter(_)
            | GridAction::SaveDefaultFilter
            | GridAction::ClearFilterText => {}
            GridAction::OpenValuePanel { .. } | GridAction::AutoRefreshChanged(None) => {}
            // Auto-refresh would run INSERT / UPDATE … RETURNING, CALL … again every few seconds.
            GridAction::AutoRefreshChanged(Some(_)) if !self.src.rerun_safe => {
                self.view.toolbar.auto_refresh_secs = None;
                self.status = Some((tr("No auto-refresh: it would run the statement that changes data again.").to_string(), true));
            }
            GridAction::AutoRefreshChanged(Some(_)) => {}
            GridAction::ServerQueryChanged { where_clause, order_by } => self.run_or_confirm(Deferred::Query { where_clause, order_by }, cx),
            GridAction::NavigateForeignKey { link, values } => {
                let Some(d) = self.dialect(cx) else { return };
                let w = Self::where_for(d.as_ref(), &link.ref_columns, &values);
                let target = Location { table: link.ref_table.clone(), where_clause: Some(w) };
                if self.table.is_some() {
                    self.run_or_confirm(Deferred::Navigate(target), cx);
                } else {
                    cx.push(AppCommand::OpenTab(Box::new(crate::results::data_tab::TableDataTab::new(self.conn.clone(), target.table, target.where_clause))));
                }
            }
            GridAction::RequestReferences { relation, values } => {
                let (Some(rel), Some(d)) = (self.relations.get(relation).cloned(), self.dialect(cx)) else { return };
                let w = Self::where_for(d.as_ref(), &rel.columns, &values);
                let sql = d.select_table(&rel.table, None, Some(&w), None, Some(200));
                // The shared session of that database (the result's own one would lose its cursor).
                let backend = cx.backend;
                let Some(s) = cx.conns.live_mut(&self.conn).and_then(|l| l.ensure_session(backend, rel.table.database.as_deref())) else {
                    self.status = Some((tr("Opening a session…").to_string(), false));
                    return;
                };
                self.refs_job = Some((relation, values, backend.spawn(async move { s.query_all(&sql, &[]).await })));
            }
            GridAction::PasteRequested if self.save_job.is_some() => {}
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
                    self.table.clone(),
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
            GridAction::RecordPrefs { hide_empty } => {
                cx.settings.results.record_hide_empty = hide_empty;
                cx.push(AppCommand::SettingsChanged);
            }
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
        let Some(cs) = self.model.change_set() else { return };
        let mut stmts = cs.save_statements(d.as_ref());
        if stmts.is_empty() {
            return;
        }
        // New rows are read back, so that generated ids and defaults show without a refresh:
        // from `RETURNING`, else by their key (an id generated by MySQL / SQL Server included).
        let returning = d.supports_returning() && !cs.columns.is_empty();
        let mssql = d.kind() == dbiji_core::DbKind::SqlServer;
        let mut reads: Vec<ReadBack> = Vec::new();
        // Without RETURNING each new row costs a query: only so many are read back.
        const MAX_READ_BACK: usize = 500;
        for ins in cs.inserts.iter().take(if returning { usize::MAX } else { MAX_READ_BACK }) {
            let Some(stmt) = cs.insert_statement(ins.id) else { continue };
            if mssql && !returning {
                stmts[stmt].0.push_str("; SELECT CAST(SCOPE_IDENTITY() AS BIGINT)");
            }
            let key = cs
                .key_columns
                .iter()
                .filter_map(|&k| {
                    let (_, name, type_name) = cs.columns.iter().find(|(c, _, _)| *c == k)?;
                    let given = ins.values.iter().find(|v| v.column == k).map(|v| v.value.value_or_null().clone());
                    Some((name.clone(), type_name.clone(), given))
                })
                .collect::<Vec<_>>();
            // A key column left out must be the generated one (the id of LAST_INSERT_ID / SCOPE_IDENTITY).
            let generated_key =
                cs.key_columns.iter().filter(|&&k| !ins.values.iter().any(|v| v.column == k)).all(|&k| self.model.column_meta(k).and_then(|m| m.auto_increment) != Some(false));
            if !returning && !generated_key {
                continue;
            }
            reads.push(ReadBack { id: ins.id, stmt, key });
        }
        let columns = cs.columns.clone();
        let table = cs.table.clone();
        let s = self.src.session.clone();
        let expect_one: Vec<bool> = (0..stmts.len()).map(|i| i < cs.deletes.len() + cs.updates.len()).collect();
        self.save_job = Some(cx.backend.spawn(async move {
            let applied = s.apply_changes_reading(&stmts, &expect_one).await?;
            let mut inserted: HashMap<u64, Vec<(usize, Value)>> = HashMap::new();
            let as_values = |row: &Row| columns.iter().zip(row).map(|((c, _, t), v)| (*c, d.returned_value(t, v.clone()))).collect::<Vec<_>>();
            for rb in &reads {
                let result = applied.results.get(rb.stmt);
                if returning {
                    if let Some(row) = result.and_then(|r| r.first_row.as_ref()) {
                        inserted.insert(rb.id, as_values(row));
                    }
                    continue;
                }
                let generated =
                    result.and_then(|r| r.last_insert_id.map(Value::UInt).or_else(|| if mssql { r.first_row.as_ref()?.first().filter(|v| !v.is_null()).cloned() } else { None }));
                let Some(sql) = rb.select_sql(d.as_ref(), &table, &columns, generated) else { continue };
                if let Ok((_, rows)) = s.query_all(&sql, &[]).await
                    && let Some(row) = rows.first()
                {
                    inserted.insert(rb.id, as_values(row));
                }
            }
            Ok((applied.rows, applied.committed, inserted))
        }));
    }

    /// Reload the rows (Refresh), asking first when edits are pending.
    pub fn refresh(&mut self, cx: &mut AppContext) {
        self.run_or_confirm(Deferred::Refresh, cx);
    }

    /// Reloading replaces the rows and drops the pending edits: ask first.
    fn run_or_confirm(&mut self, d: Deferred, cx: &mut AppContext) {
        if self.model.has_changes() {
            self.confirm_discard = Some(d);
        } else {
            self.run_deferred(d, cx);
        }
    }

    fn run_deferred(&mut self, d: Deferred, cx: &mut AppContext) {
        // Refreshing or filtering on the server runs the statement again: not a write
        // (INSERT / UPDATE … RETURNING, CALL) without asking.
        if !self.src.rerun_safe && matches!(d, Deferred::Refresh | Deferred::Query { .. }) && self.confirm_rerun.is_none() {
            self.confirm_rerun = Some(d);
            return;
        }
        self.run_now(d, cx);
    }

    fn run_now(&mut self, d: Deferred, cx: &mut AppContext) {
        match d {
            Deferred::Refresh => {
                self.src.refresh(cx.backend, Self::fetch_size(cx));
                self.executed = Instant::now();
            }
            Deferred::Query { where_clause, order_by } => {
                if let Some(w) = where_clause {
                    self.where_clause = Some(w).filter(|w| !w.trim().is_empty());
                }
                if let Some(o) = order_by {
                    self.order_by = Some(o).filter(|o| !o.trim().is_empty());
                }
                self.requery(cx);
            }
            Deferred::Navigate(to) => self.navigate(cx, to, true),
        }
    }

    /// "Run the statement again?" for a result of a statement that changes data.
    fn confirm_rerun_ui(&mut self, ui: &Ui, cx: &mut AppContext) {
        if self.confirm_rerun.is_none() {
            return;
        }
        let mut decided: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new(("confirm_rerun", &self.conn, &self.title))).show(ui.ctx(), |ui| {
            ui.set_max_width(460.0);
            ui.heading(tr("Run the statement again?"));
            ui.label(tr("These rows come from a statement that changes data (INSERT / UPDATE … RETURNING, CALL, EXEC). Reloading them runs it again."));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Run again")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    decided = Some(true);
                }
                if ui.button(tr("Cancel")).clicked() {
                    decided = Some(false);
                }
            });
        });
        if decided.is_none() && modal.should_close() {
            decided = Some(false);
        }
        match decided {
            Some(true) => {
                if let Some(d) = self.confirm_rerun.take() {
                    self.run_now(d, cx);
                }
            }
            Some(false) => self.confirm_rerun = None,
            None => {}
        }
    }

    fn confirm_discard_ui(&mut self, ui: &Ui, cx: &mut AppContext) {
        if self.confirm_discard.is_none() {
            return;
        }
        let mut decided: Option<bool> = None;
        let n = self.model.change_set().map(|c| c.len()).unwrap_or(0);
        let modal = egui::Modal::new(egui::Id::new(("confirm_discard", &self.conn, &self.title))).show(ui.ctx(), |ui| {
            ui.heading(tr("Unsaved changes"));
            ui.label(trf("Reloading the rows discards the {} pending change(s). Save them first to keep them.", &[&n]));
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Discard and reload")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    decided = Some(true);
                }
                if ui.button(tr("Cancel")).clicked() {
                    decided = Some(false);
                }
            });
        });
        if decided.is_none() && modal.should_close() {
            decided = Some(false);
        }
        match decided {
            Some(true) => {
                self.model.discard_changes();
                if let Some(d) = self.confirm_discard.take() {
                    self.run_deferred(d, cx);
                }
            }
            Some(false) => self.confirm_discard = None,
            None => {}
        }
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
            if ui.button((dbiji_icons::ic(icon::TABLE), tr("Back to grid"))).clicked() {
                self.mode = Mode::Grid;
            }
            if ui.button((dbiji_icons::ic(icon::EXPORT), tr("Export chart…"))).clicked()
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
            if ui.button((dbiji_icons::ic(icon::TABLE), tr("Back to grid"))).clicked() {
                self.mode = Mode::Grid;
            }
            egui::ComboBox::from_id_salt("spatial_col").selected_text(names.get(sv.column).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                for c in &cols {
                    if ui.selectable_value(&mut sv.column, *c, names.get(*c).cloned().unwrap_or_default()).changed() {
                        rebuild = true;
                    }
                }
            });
            if ui.button(dbiji_icons::ic(icon::ARROW_CLOCKWISE)).clicked() {
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
        self.confirm_discard_ui(ui, cx);
        self.confirm_rerun_ui(ui, cx);
        if !self.settings_applied {
            self.settings_applied = true;
            let r = &cx.settings.results;
            self.view.grid.mode = match r.sort_filter_mode {
                dbiji_config::settings::SortFilterMode::Server => dbiji_grid::DataMode::Server,
                dbiji_config::settings::SortFilterMode::Client => dbiji_grid::DataMode::Client,
            };
            if r.auto_refresh_secs > 0 && self.view.toolbar.auto_refresh_secs.is_none() && self.src.rerun_safe {
                self.view.toolbar.auto_refresh_secs = Some(r.auto_refresh_secs as u64);
            }
        }
        self.poll(cx);
        self.resolve_editability(cx);
        if self.table.is_some() && (!self.back.is_empty() || !self.forward.is_empty()) {
            ui.horizontal(|ui| {
                // Not with pending edits: moving to another table would discard them.
                let free = !self.model.has_changes();
                if ui.add_enabled(free && !self.back.is_empty(), egui::Button::new(dbiji_icons::ic(icon::ARROW_LEFT))).on_hover_text(tr("Back")).clicked()
                    && let Some(loc) = self.back.pop()
                {
                    if let Some(t) = &self.table {
                        self.forward.push(Location { table: t.clone(), where_clause: self.where_clause.clone() });
                    }
                    self.navigate(cx, loc, false);
                }
                if ui.add_enabled(free && !self.forward.is_empty(), egui::Button::new(dbiji_icons::ic(icon::ARROW_RIGHT))).on_hover_text(tr("Forward")).clicked()
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
                    compact_rows: cx.settings.results.row_density == dbiji_config::settings::RowDensity::Compact,
                    fetch_more_threshold: Self::fetch_size(cx),
                    sql_table_name: self.table.as_ref().map(|t| t.name.clone()),
                    dialect,
                    // Not while a save runs: edits made then would be marked saved with it.
                    allow_editing: !cx.conns.config(&self.conn).is_some_and(|c| c.read_only) && self.save_job.is_none(),
                    record_hide_empty: cx.settings.results.record_hide_empty,
                    ..Default::default()
                };
                // Messages appear in the toolbar next to the status.
                if self.status.is_some() {
                    self.view.toolbar.notice = self.status.take();
                }
                let resp = ResultViewer::new(&mut self.model, &mut self.view, &options).id_salt(("result", &self.conn, &self.title)).show(ui);
                for a in resp.actions {
                    self.handle(a, cx);
                }
                if self.status.is_some() {
                    self.view.toolbar.notice = self.status.take();
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

/// The statistics of a whole column: `COUNT(*)`, `COUNT(c)` and, where the
/// type has them, `COUNT(DISTINCT c)`, `MIN(c)`, `MAX(c)`.
fn column_stats_text(v: &[Value]) -> String {
    let int = |i: usize| v.get(i).and_then(Value::as_i64);
    let short = |i: usize| {
        let t = v.get(i).map(|x| if x.is_null() { "NULL".to_string() } else { x.to_text().into_owned() }).unwrap_or_default();
        if t.chars().count() > 60 { t.chars().take(60).collect::<String>() + "…" } else { t }
    };
    let (rows, values) = (int(0).unwrap_or(0), int(1).unwrap_or(0));
    let mut text = format!("{}: {rows} · {}: {}", tr("Rows"), tr("NULLs"), rows - values);
    if let Some(d) = int(2) {
        text.push_str(&format!(" · {}: {d}", tr("Distinct")));
    }
    if v.len() >= 5 {
        text.push_str(&format!("\n{}: {} · {}: {}", tr("Min"), short(3), tr("Max"), short(4)));
    }
    text
}
