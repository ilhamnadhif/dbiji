//! SQL editor tab (FR-S1..S10): script or console bound to a connection and
//! schema, with its own session, statement / script / selection execution,
//! parameters and client commands, confirmations, result tabs, statistics,
//! output log, transaction controls, explain plans, formatting, outline,
//! autocomplete, diagnostics, spell checking and script files.

use crate::backend::Job;
use crate::command::{AppCommand, ObjectPage};
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::exec::{ClientAction, ExecEvent, ExecOpts, Execution};
use crate::keys::{Action, ActionContext};
use crate::results::panel::ResultPanel;
use crate::results::source::ResultSource;
use crate::sqllang::SqlLanguage;
use crate::sqlprep::{PrepConfig, Scope};
use crate::tabs::TabView;
use dbiji_core::driver::TransactionMode;
use dbiji_core::meta::{ObjectRef, SchemaRef};
use dbiji_core::plan::QueryPlan;
use dbiji_core::{DbResult, tr, trf};
use dbiji_editor::{ContextAction, EditorEvent, EditorOptions, EditorState, EditorTheme, SqlEditor};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One entry of the results area.
enum ResultItem {
    Rows(Box<ResultPanel>),
    Plan { sql: String, plan: Box<QueryPlan>, state: dbiji_visual::plan::PlanViewState, view: PlanView },
    Error { sql: String, text: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanView {
    Diagram,
    Tree,
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bottom {
    Result(usize),
    Statistics,
    Output,
    Transaction,
    Variables,
}

struct StatRow {
    index: usize,
    sql: String,
    status: Option<bool>,
    rows: Option<u64>,
    elapsed: Option<Duration>,
    message: String,
}

#[derive(Clone)]
struct LogLine {
    at: chrono::DateTime<chrono::Local>,
    severity: String,
    text: String,
}

/// What the user asked to run; kept while parameters or a confirmation are pending.
#[derive(Clone)]
struct Request {
    scope: Scope,
    new_tab: bool,
    explain: Option<bool>,
    confirmed: bool,
    /// SQL to run instead of the editor text (row count, evaluate expression).
    sql: Option<String>,
}

struct ParamPrompt {
    request: Request,
    names: Vec<String>,
    values: Vec<String>,
    /// The first field got the keyboard focus.
    focused: bool,
}

struct Confirm {
    request: Request,
    reasons: Vec<String>,
}

enum SessionSlot {
    None,
    Opening(Job<DbResult<SessionHandle>>),
    Ready(SessionHandle),
    Failed(String),
}

pub struct SqlEditorTab {
    pub conn: Option<String>,
    pub schema: Option<SchemaRef>,
    pub path: Option<PathBuf>,
    pub console: bool,
    title: String,
    editor: EditorState,
    lang: SqlLanguage,
    lang_kind: Option<dbiji_core::DbKind>,
    session: SessionSlot,
    /// Schema to apply to the session once it is ready.
    pending_schema: bool,
    exec: Option<Execution>,
    explain_job: Option<(String, Job<DbResult<QueryPlan>>)>,
    results: Vec<ResultItem>,
    bottom: Bottom,
    stats: Vec<StatRow>,
    output: Vec<LogLine>,
    show_results: bool,
    maximized: bool,
    show_outline: bool,
    variables: BTreeMap<String, String>,
    params: Option<ParamPrompt>,
    confirm: Option<Confirm>,
    pending_export: Option<BTreeMap<String, String>>,
    /// The next row result replaces the active result tab.
    replace_result: bool,
    editor_focused: bool,
    last_version: u64,
    analysed_version: u64,
    changed_at: Option<Instant>,
    catalog_generation: Option<(u64, Option<String>)>,
    user_words: HashSet<String>,
    close_prompt: bool,
    ddl_executed: bool,
    last_run: Option<(Instant, Duration)>,
    terminal: bool,
    select_conn_open: bool,
    select_schema_open: bool,
    request_focus: bool,
    /// Dock id of this tab (to close it after the "unsaved changes" prompt).
    tab_id: Option<u64>,
    /// Run the whole script as soon as the session is ready (benchmarks, "run on open").
    pub auto_run: bool,
    /// Fetch every row of new results (benchmarks).
    pub fetch_all_results: bool,
}

#[allow(clippy::field_reassign_with_default)]
fn editor_options(cx: &AppContext) -> EditorOptions {
    let e = &cx.settings.editor;
    let mut o = EditorOptions::default();
    o.font_size = e.font_size;
    o.tab_size = e.tab_size as usize;
    o.insert_spaces = e.insert_spaces;
    o.line_numbers = e.line_numbers;
    o.folding = e.folding;
    o.mark_occurrences = e.mark_occurrences;
    o.highlight_current_line = e.highlight_current_line;
    o.word_wrap = e.word_wrap;
    o.auto_close_brackets = e.auto_close_brackets || e.auto_close_quotes;
    o.auto_complete = e.autocomplete.enabled;
    o.auto_complete_delay_ms = e.autocomplete.delay_ms as u64;
    o.auto_complete_on_letters = e.autocomplete.auto_activate_on_letters;
    o.auto_complete_min_chars = e.autocomplete.min_chars as usize;
    // Editor-level shortcuts that the application keymap owns.
    o.keys.save.clear();
    o
}

fn split_options(cx: &AppContext) -> dbiji_sql::SplitOptions {
    let e = &cx.settings.editor;
    dbiji_sql::SplitOptions {
        delimiter: if e.statement_delimiter.trim().is_empty() { ";".into() } else { e.statement_delimiter.clone() },
        blank_line: match e.blank_line_delimiter {
            dbiji_config::settings::BlankLineDelimiter::Never => dbiji_sql::BlankLineMode::Never,
            dbiji_config::settings::BlankLineDelimiter::Smart => dbiji_sql::BlankLineMode::Smart,
            dbiji_config::settings::BlankLineDelimiter::Always => dbiji_sql::BlankLineMode::Always,
        },
        client_commands: true,
    }
}

fn keyword_case(k: dbiji_config::settings::KeywordCase) -> dbiji_sql::KeywordCase {
    match k {
        dbiji_config::settings::KeywordCase::Upper => dbiji_sql::KeywordCase::Upper,
        dbiji_config::settings::KeywordCase::Lower => dbiji_sql::KeywordCase::Lower,
        dbiji_config::settings::KeywordCase::Preserve => dbiji_sql::KeywordCase::Preserve,
    }
}

impl SqlEditorTab {
    pub fn new(conn: Option<String>, schema: Option<SchemaRef>, text: String, path: Option<PathBuf>, console: bool, title: String) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let editor = EditorState::new(("sql_editor", n), text);
        SqlEditorTab {
            conn,
            schema,
            path,
            console,
            title,
            last_version: editor.version(),
            editor,
            lang: SqlLanguage::new(Arc::new(dbiji_core::dialect::GenericDialect::new())),
            lang_kind: None,
            session: SessionSlot::None,
            pending_schema: true,
            exec: None,
            explain_job: None,
            results: Vec::new(),
            bottom: Bottom::Output,
            stats: Vec::new(),
            output: Vec::new(),
            show_results: true,
            maximized: false,
            show_outline: false,
            variables: BTreeMap::new(),
            params: None,
            confirm: None,
            pending_export: None,
            replace_result: true,
            editor_focused: true,
            analysed_version: 0,
            changed_at: None,
            catalog_generation: None,
            user_words: HashSet::new(),
            close_prompt: false,
            ddl_executed: false,
            last_run: None,
            terminal: console,
            select_conn_open: false,
            select_schema_open: false,
            request_focus: true,
            tab_id: None,
            auto_run: false,
            fetch_all_results: false,
        }
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    fn log(&mut self, severity: &str, text: impl Into<String>) {
        self.output.push(LogLine { at: chrono::Local::now(), severity: severity.into(), text: text.into() });
        if self.output.len() > 5000 {
            self.output.drain(..1000);
        }
    }

    fn reset_session(&mut self, cx: &mut AppContext) {
        if let SessionSlot::Ready(s) = std::mem::replace(&mut self.session, SessionSlot::None) {
            let main = self.conn.as_ref().and_then(|c| cx.conns.live(c)).map(|l| l.main.clone());
            if !main.is_some_and(|m| m.same(&s)) {
                cx.backend.spawn_detached(async move { s.close().await });
            }
        }
        self.pending_schema = true;
    }

    /// Change the editor's connection.
    pub fn set_connection(&mut self, cx: &mut AppContext, conn: Option<String>) {
        if self.conn == conn {
            return;
        }
        self.reset_session(cx);
        self.conn = conn;
        self.schema = None;
        self.catalog_generation = None;
        self.lang.set_catalog(None);
        if let Some(c) = &self.conn
            && !cx.conns.is_connected(c)
        {
            cx.push(AppCommand::Connect(c.clone()));
        }
    }

    /// The editor's session, opening it on first use.
    fn session(&mut self, cx: &mut AppContext) -> Option<SessionHandle> {
        let conn = self.conn.clone()?;
        if let SessionSlot::Opening(j) = &mut self.session
            && let Some(r) = j.poll()
        {
            self.session = match r {
                Ok(s) => SessionSlot::Ready(s),
                Err(e) => SessionSlot::Failed(e.full_text()),
            };
        }
        if let SessionSlot::Ready(s) = &self.session {
            if s.is_closed() {
                self.session = SessionSlot::None;
                self.log("WARNING", tr("The session was closed; a new one is opened for the next statement."));
            } else {
                return Some(s.clone());
            }
        }
        let live = cx.conns.live_mut(&conn)?;
        if !matches!(self.session, SessionSlot::None) {
            return None;
        }
        let database = self.schema.as_ref().and_then(|s| s.database.clone());
        if live.config.init.separate_editor_connection {
            let label = format!("{} — {}", live.config.name, self.title);
            self.session = SessionSlot::Opening(live.open_session_job(cx.backend, label, database));
            None
        } else {
            let s = live.ensure_session(cx.backend, database.as_deref())?;
            self.session = SessionSlot::Ready(s.clone());
            Some(s)
        }
    }

    fn dialect_sync(&mut self, cx: &AppContext) {
        let kind = self.conn.as_ref().and_then(|c| cx.conns.config(c)).map(|c| c.kind);
        if kind != self.lang_kind {
            self.lang_kind = kind;
            let d: Arc<dyn dbiji_core::Dialect> = match (self.conn.as_ref().and_then(|c| cx.conns.live(c)), kind) {
                (Some(l), _) => l.dialect.clone(),
                (None, Some(k)) => cx.conns.registry.dialect(k),
                _ => Arc::new(dbiji_core::dialect::GenericDialect::new()),
            };
            self.lang.set_dialect(d);
        }
        if self.lang.templates.is_none() {
            self.lang.templates = Some(cx.services.templates.clone());
        }
        self.lang.kind = kind;
        let e = &cx.settings.editor;
        self.lang.completion.max_items = e.autocomplete.max_items as usize;
        self.lang.completion.keyword_case = keyword_case(e.autocomplete.keyword_case);
        self.lang.completion.split = split_options(cx);
    }

    /// Autocomplete catalog from the metadata cache of the active schema.
    fn catalog_sync(&mut self, cx: &mut AppContext) {
        let Some(conn) = self.conn.clone() else { return };
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&conn) else { return };
        let schema = self.schema.clone().unwrap_or_else(|| SchemaRef::new(None, live.current_schema.clone().or(live.current_database.clone())));
        let main = live.main.clone();
        live.meta.request_snapshot(backend, &main, &schema);
        live.meta.request_schemas(backend, &main, schema.database.as_deref());
        let key = (live.meta.generation, schema.schema.clone());
        if self.catalog_generation.as_ref() == Some(&key) {
            return;
        }
        self.catalog_generation = Some(key);
        let snaps: Vec<dbiji_core::ddl::SchemaSnapshot> = live.meta.loaded_snapshots().iter().map(|s| (**s).clone()).collect();
        let names: Vec<String> = live.meta.schemas_of(schema.database.as_deref()).get().map(|v| v.iter().map(|s| s.name.clone()).collect()).unwrap_or_default();
        let catalog = dbiji_sql::SnapshotCatalog::new(snaps, schema.schema.clone()).with_extra_schemas(names);
        self.lang.set_catalog(Some(Arc::new(catalog)));
        self.analysed_version = 0;
    }

    fn prep_config(&self, cx: &AppContext) -> PrepConfig<'_> {
        let p = &cx.settings.editor.parameters;
        let params = p.enabled.then_some(dbiji_sql::ParamOptions { named: p.named_colon, template: p.dollar_brace, positional: p.question_mark, numbered: false });
        PrepConfig { syntax: self.lang.syntax(), split: split_options(cx), params, base_dir: self.path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf())) }
    }

    fn scope_for(&self, statement: bool) -> Scope {
        match self.editor.selection() {
            Some(r) if !r.is_empty() => Scope::Selection(r),
            _ if statement => Scope::Statement(self.editor.cursor()),
            _ => Scope::Script,
        }
    }

    /// The selection, or the statement under the cursor.
    fn current_statement(&self, cx: &AppContext) -> Option<std::ops::Range<usize>> {
        if let Some(r) = self.editor.selection().filter(|r| !r.is_empty()) {
            return Some(r);
        }
        let split = split_options(cx);
        let text = self.editor.text();
        dbiji_sql::statement_at(text, self.editor.cursor().min(text.len()), self.lang.syntax(), &split).map(|s| s.start..s.end)
    }

    /// Move the cursor to the next (`forward`) or previous statement.
    fn goto_statement(&mut self, ctx: &egui::Context, cx: &AppContext, forward: bool) {
        let split = split_options(cx);
        let spans = dbiji_sql::split_statements(self.editor.text(), self.lang.syntax(), &split);
        let c = self.editor.cursor();
        let target = if forward { spans.iter().find(|s| s.start > c).map(|s| s.start) } else { spans.iter().rev().find(|s| s.start < c).map(|s| s.start) };
        if let Some(t) = target {
            self.editor.set_cursor(ctx, t);
            self.editor.request_focus(ctx);
        }
    }

    /// The selection or the word under the cursor.
    fn selection_or_word(&self) -> Option<String> {
        match self.editor.selected_text().filter(|t| !t.trim().is_empty()) {
            Some(t) => Some(t.to_string()),
            None => self.editor.word_at(&self.lang, self.editor.cursor()).map(|r| self.editor.text()[r].to_string()),
        }
    }

    /// Open the table or view at `offset` (Cmd+click, F3).
    fn open_declaration(&mut self, cx: &mut AppContext, offset: usize) {
        let Some(conn) = self.conn.clone() else { return };
        let Some(o) = dbiji_sql::object_at(self.editor.text(), offset, self.lang.syntax()) else { return };
        let table = self.lang.catalog().and_then(|c| dbiji_sql::CompletionCatalog::find_table(c.as_ref(), o.schema.as_deref(), &o.table));
        match table {
            Some(t) => {
                let obj = ObjectRef::new(t.kind, t.schema.clone(), t.name.clone()).with_database(self.schema.as_ref().and_then(|s| s.database.clone()));
                cx.push(AppCommand::OpenObject { connection: conn, object: obj, page: ObjectPage::Properties });
            }
            None => cx.toasts.info(trf("No table or view named {}", &[&o.table])),
        }
    }

    /// Editor commands of the "More" menu and their actions.
    fn editor_command(&mut self, ctx: &egui::Context, cx: &mut AppContext, action: Action) {
        match action {
            Action::NextStatement => self.goto_statement(ctx, cx, true),
            Action::PreviousStatement => self.goto_statement(ctx, cx, false),
            Action::CopyStatement => {
                if let Some(r) = self.current_statement(cx) {
                    ctx.copy_text(self.editor.text()[r].trim().to_string());
                    cx.toasts.info(tr("Statement copied"));
                }
            }
            Action::SelectRowCount => {
                if let Some(r) = self.current_statement(cx) {
                    let sql = crate::textops::row_count_sql(&self.editor.text()[r]);
                    self.run(cx, Request { scope: Scope::Script, new_tab: true, explain: None, confirmed: false, sql: Some(sql) });
                }
            }
            Action::EvaluateExpression => {
                if let Some(e) = self.selection_or_word() {
                    let sql = format!("SELECT {}", e.trim().trim_end_matches(';'));
                    self.run(cx, Request { scope: Scope::Script, new_tab: true, explain: None, confirmed: false, sql: Some(sql) });
                }
            }
            Action::OpenDeclaration => {
                let c = self.editor.cursor();
                self.open_declaration(cx, c);
            }
            Action::MorphToDelimited => match self.editor.selection().filter(|r| !r.is_empty()) {
                Some(r) => {
                    let list = crate::textops::morph_to_delimited(&self.editor.text()[r.clone()]);
                    self.editor.replace_range(ctx, r.clone(), &list);
                    self.editor.select(ctx, r.start..r.start + list.len());
                }
                None => cx.toasts.info(tr("Select the lines to turn into a list first.")),
            },
            Action::TrimTrailingSpaces => {
                let r = self.editor.selection().filter(|r| !r.is_empty()).unwrap_or(0..self.editor.text().len());
                let trimmed = crate::textops::trim_trailing(&self.editor.text()[r.clone()]);
                if trimmed != self.editor.text()[r.clone()] {
                    self.editor.replace_range(ctx, r, &trimmed);
                }
            }
            Action::ToggleFold => {
                self.editor.toggle_fold_at_cursor();
            }
            Action::CollapseAllFolds => self.editor.fold_all(),
            Action::ExpandAllFolds => self.editor.unfold_all(),
            Action::SearchWeb => {
                if let Some(q) = self.selection_or_word() {
                    ctx.open_url(egui::OpenUrl::new_tab(crate::textops::search_url(&q)));
                }
            }
            _ => {}
        }
    }

    /// Execute (or explain) according to `req`.
    fn run(&mut self, cx: &mut AppContext, req: Request) {
        if self.exec.is_some() || self.explain_job.is_some() {
            cx.toasts.info(tr("A query is already running in this editor."));
            return;
        }
        let Some(conn) = self.conn.clone() else {
            self.select_conn_open = true;
            cx.toasts.info(tr("Choose a connection for this editor first."));
            return;
        };
        if !cx.conns.is_connected(&conn) {
            cx.push(AppCommand::Connect(conn));
            cx.toasts.info(tr("Connecting… run the statement again when connected."));
            return;
        }
        let Some(session) = self.session(cx) else {
            cx.toasts.info(tr("Opening a session… run the statement again in a moment."));
            return;
        };
        let text = req.sql.clone().unwrap_or_else(|| self.editor.text().to_string());
        let prepared = {
            let cfg = self.prep_config(cx);
            crate::sqlprep::prepare(&text, req.scope.clone(), &cfg, &self.variables)
        };
        for e in &prepared.errors {
            self.log("ERROR", e.clone());
        }
        if !prepared.errors.is_empty() {
            self.bottom = Bottom::Output;
            self.show_results = true;
            return;
        }
        if prepared.plans.is_empty() {
            return;
        }
        if !prepared.missing.is_empty() {
            let names: Vec<String> = prepared.missing.iter().map(|(_, n)| n.clone()).collect();
            let values = names.iter().map(|n| self.variables.get(n).cloned().unwrap_or_default()).collect();
            self.params = Some(ParamPrompt { request: req, names, values, focused: false });
            return;
        }
        let cfg = cx.conns.config(&conn).cloned();
        let ctype = cx.conns.type_of(&conn);
        if cfg.as_ref().is_some_and(|c| c.read_only) && prepared.has_writes {
            self.log("ERROR", tr("The connection is read-only; data and schema changes are blocked."));
            cx.toasts.error(tr("Read-only connection"), Some(tr("The script contains statements that change data or schema.").to_string()));
            return;
        }
        if !req.confirmed {
            let mut reasons: Vec<String> = Vec::new();
            if ctype.confirm_execute {
                reasons.push(trf("Connection type \"{}\" asks before every execution.", &[&ctype.name]));
            }
            if ctype.confirm_dangerous || cx.settings.execution.confirm_dangerous_statements {
                for (i, d) in &prepared.dangers {
                    let sql = prepared.plans.get(*i).map(|p| p.original.lines().next().unwrap_or("").to_string()).unwrap_or_default();
                    reasons.push(format!("{} — {sql}", tr(d.message())));
                }
            }
            if !reasons.is_empty() {
                self.confirm = Some(Confirm { request: req, reasons });
                return;
            }
        }
        if let Some(analyze) = req.explain {
            let Some(plan) = prepared.plans.iter().find(|p| p.client.is_none()) else { return };
            let sql = plan.sql.clone();
            let s = session.clone();
            let q = sql.clone();
            self.explain_job = Some((sql, cx.backend.spawn(async move { s.lock().await.explain(&q, analyze).await })));
            self.log("INFO", if analyze { tr("Explain analyze…") } else { tr("Explain…") });
            return;
        }
        self.replace_result = !req.new_tab && !cx.settings.results.open_each_query_in_new_tab && prepared.plans.len() == 1;
        self.stats = prepared.plans.iter().map(|p| StatRow { index: p.index, sql: p.original.clone(), status: None, rows: None, elapsed: None, message: String::new() }).collect();
        self.ddl_executed = prepared.plans.iter().any(|p| {
            let k = dbiji_sql::classify(&p.sql, self.lang.syntax()).kind;
            matches!(k, dbiji_sql::StatementKind::Create | dbiji_sql::StatementKind::Alter | dbiji_sql::StatementKind::Drop | dbiji_sql::StatementKind::Rename)
        });
        let smart = cx.settings.execution.smart_commit && prepared.has_writes && session.tx_state().mode == TransactionMode::Auto;
        let opts = ExecOpts {
            fetch_size: cx.settings.results.effective_fetch_size() as usize,
            stop_on_error: cx.settings.execution.stop_on_error,
            resolve_sources: true,
            timeout: match cx.settings.execution.query_timeout_secs {
                0 => cfg.as_ref().and_then(|c| (c.init.query_timeout_secs > 0).then(|| Duration::from_secs(c.init.query_timeout_secs as u64))),
                n => Some(Duration::from_secs(n as u64)),
            },
            store_budget: cx.settings.results.store_budget_bytes(),
            switch_to_manual: smart,
        };
        let name = cfg.as_ref().map(|c| c.name.clone());
        let history = cx.services.history_sink(Some(conn.clone()), name, self.schema.as_ref().and_then(|s| s.database.clone()));
        self.variables = prepared.variables.clone();
        self.log("INFO", trf("Executing {} statement(s)…", &[&prepared.plans.len()]));
        self.exec = Some(Execution::start(cx.backend, session, prepared.plans, opts, history));
        self.editor.set_diagnostics(Vec::new());
    }

    fn poll_exec(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        if let Some((sql, job)) = self.explain_job.as_mut()
            && let Some(r) = job.poll()
        {
            let sql = sql.clone();
            self.explain_job = None;
            match r {
                Ok(plan) => {
                    self.results.push(ResultItem::Plan { sql, plan: Box::new(plan), state: Default::default(), view: PlanView::Diagram });
                    self.bottom = Bottom::Result(self.results.len() - 1);
                    self.show_results = true;
                }
                Err(e) => {
                    self.log("ERROR", e.full_text());
                    self.bottom = Bottom::Output;
                }
            }
        }
        let Some(exec) = self.exec.as_mut() else { return };
        let events = exec.poll();
        let finished = exec.is_finished();
        let session = exec.session.clone();
        for ev in events {
            match ev {
                ExecEvent::Started { index } => {
                    let range = self.statement_range(index);
                    self.editor.set_statement_highlight(range);
                }
                ExecEvent::Rows { index, sql, columns, store, token, exhausted, elapsed } => {
                    if let Some(st) = self.stats.iter_mut().find(|s| s.index == index) {
                        st.rows = Some(store.lock().len() as u64);
                    }
                    if let Some(opts) = self.pending_export.take() {
                        self.export_result(cx, &sql, opts);
                        continue;
                    }
                    let budget = cx.settings.results.store_budget_bytes();
                    let mut src = ResultSource::from_executed(session.clone(), sql.clone(), Vec::new(), columns, store, token, exhausted, budget);
                    src.exec_duration = elapsed;
                    if self.terminal {
                        self.print_rows(&src, cx);
                        continue;
                    }
                    let title = result_title(&sql, self.results.len() + 1);
                    let mut panel = ResultPanel::new(self.conn.clone().unwrap_or_default(), title, src, None);
                    if self.fetch_all_results {
                        panel.src.fetch_more(cx.backend, usize::MAX, None);
                    }
                    let item = ResultItem::Rows(Box::new(panel));
                    let pos = match (self.replace_result, self.bottom) {
                        (true, Bottom::Result(i)) if i < self.results.len() && !self.result_has_changes(i) => {
                            self.results[i] = item;
                            i
                        }
                        _ => {
                            self.results.push(item);
                            self.results.len() - 1
                        }
                    };
                    let max = cx.settings.results.max_result_tabs.max(1) as usize;
                    let mut pos = pos;
                    while self.results.len() > max {
                        let victim = (0..self.results.len()).find(|&i| i != pos && !self.result_has_changes(i));
                        match victim {
                            Some(v) => {
                                self.results.remove(v);
                                if v < pos {
                                    pos -= 1;
                                }
                            }
                            None => break,
                        }
                    }
                    self.bottom = Bottom::Result(pos);
                    self.show_results = true;
                    self.replace_result = true;
                }
                ExecEvent::Update { index, rows, tag, last_insert_id } => {
                    if let Some(st) = self.stats.iter_mut().find(|s| s.index == index) {
                        st.rows = Some(st.rows.unwrap_or(0) + rows.unwrap_or(0));
                        st.message = tag.clone().unwrap_or_default();
                    }
                    let mut msg = match rows {
                        Some(n) => trf("{} row(s) affected", &[&n]),
                        None => tag.unwrap_or_else(|| tr("Done").to_string()),
                    };
                    if let Some(id) = last_insert_id.filter(|i| *i > 0) {
                        msg.push_str(&format!(" (last insert id {id})"));
                    }
                    self.log("INFO", msg);
                }
                ExecEvent::Message { severity, text, .. } => self.log(&severity, text),
                ExecEvent::Error { index, error } => {
                    let text = error.full_text();
                    self.log("ERROR", text.clone());
                    if let Some(st) = self.stats.iter_mut().find(|s| s.index == index) {
                        st.status = Some(false);
                        st.message = error.message.clone();
                    }
                    self.mark_error(index, error.position, &error.message);
                    let sql = self.stats.iter().find(|s| s.index == index).map(|s| s.sql.clone()).unwrap_or_default();
                    if self.stats.len() == 1 {
                        self.results.push(ResultItem::Error { sql, text });
                        self.bottom = Bottom::Result(self.results.len() - 1);
                    }
                    self.show_results = true;
                }
                ExecEvent::Client { action, .. } => match action {
                    ClientAction::Set { name, value } => {
                        self.log("INFO", format!("@set {name} = {value}"));
                        self.variables.insert(name, value);
                    }
                    ClientAction::Echo(t) => self.log("ECHO", t),
                    ClientAction::Export(opts) => {
                        self.log("INFO", tr("The next result is exported."));
                        self.pending_export = Some(opts);
                    }
                },
                ExecEvent::Finished { index, elapsed, ok } => {
                    if let Some(st) = self.stats.iter_mut().find(|s| s.index == index) {
                        st.elapsed = Some(elapsed);
                        if st.status.is_none() {
                            st.status = Some(ok);
                        }
                    }
                }
                ExecEvent::Done { elapsed, cancelled, executed, failed } => {
                    self.last_run = Some((Instant::now(), elapsed));
                    let msg = if cancelled {
                        trf("Cancelled after {}", &[&crate::widgets::format_duration(elapsed)])
                    } else {
                        trf("{} statement(s) executed, {} failed, in {}", &[&executed, &failed, &crate::widgets::format_duration(elapsed)])
                    };
                    self.log(if failed > 0 { "WARNING" } else { "INFO" }, msg);
                    if self.stats.len() > 1 && !matches!(self.bottom, Bottom::Result(_)) {
                        self.bottom = Bottom::Statistics;
                    }
                    if self.stats.len() > 1 && failed > 0 {
                        self.bottom = Bottom::Statistics;
                    }
                    if self.ddl_executed
                        && let Some(c) = &self.conn
                    {
                        cx.push(AppCommand::RefreshMetadata { connection: c.clone(), object: None, schema: self.schema.clone() });
                    }
                }
            }
        }
        if finished {
            self.exec = None;
            self.editor.set_statement_highlight(None);
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn result_has_changes(&self, i: usize) -> bool {
        matches!(self.results.get(i), Some(ResultItem::Rows(p)) if p.has_changes())
    }

    /// Editor range of the statement with this plan index (only for plans
    /// from the editor text).
    fn statement_range(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let _ = index;
        let text = self.editor.text();
        let st = self.stats.iter().find(|s| s.index == index)?;
        let pos = text.find(st.sql.as_str())?;
        Some(pos..pos + st.sql.len())
    }

    /// Mark a failed statement in the editor (error position when known).
    fn mark_error(&mut self, index: usize, position: Option<usize>, message: &str) {
        let Some(range) = self.statement_range(index) else { return };
        let text = self.editor.text();
        let r = match position {
            Some(p) if p > 0 => {
                let stmt = &text[range.clone()];
                let off = range.start + dbiji_sql::char_pos_to_byte(stmt, p);
                let end = text[off..].find(|c: char| c.is_whitespace() || c == ',' || c == ';').map(|e| off + e.max(1)).unwrap_or(text.len());
                off..end.max(off + 1).min(text.len())
            }
            _ => range,
        };
        self.editor.set_diagnostics(vec![dbiji_editor::Diagnostic::new(r.clone(), dbiji_editor::Severity::Error, message.to_string())]);
    }

    /// SQL terminal mode: print rows as text in the output.
    fn print_rows(&mut self, src: &ResultSource, cx: &AppContext) {
        let fmt = &cx.settings.results.format;
        let cols: Vec<String> = src.columns.iter().map(|c| c.name.clone()).collect();
        let mut store = src.store.lock();
        let n = store.len().min(200);
        let mut rows: Vec<Vec<String>> = Vec::with_capacity(n);
        for i in 0..n {
            if let Some(r) = store.get(i) {
                rows.push(r.iter().map(|v| v.display(fmt).chars().take(60).collect()).collect());
            }
        }
        drop(store);
        let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                if let Some(w) = widths.get_mut(i) {
                    *w = (*w).max(c.chars().count());
                }
            }
        }
        let line = |cells: &[String]| cells.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = widths.get(i).copied().unwrap_or(0))).collect::<Vec<_>>().join(" | ");
        let mut out = vec![line(&cols), widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-")];
        out.extend(rows.iter().map(|r| line(r)));
        let more = if src.has_more() { tr(" (more rows available)") } else { "" };
        out.push(trf("({} rows{})", &[&n, &more]));
        self.log("ROWS", out.join("\n"));
        self.bottom = Bottom::Output;
    }

    fn export_result(&mut self, cx: &mut AppContext, sql: &str, opts: BTreeMap<String, String>) {
        let Some(conn) = self.conn.clone() else { return };
        cx.push(AppCommand::OpenExport { connection: conn, objects: Vec::new(), sql: Some(sql.to_string()) });
        let _ = opts;
        self.log("INFO", tr("@export: the export dialog was opened with this query."));
    }

    fn format(&mut self, cx: &AppContext) {
        let f = &cx.settings.editor.format;
        let opts = dbiji_sql::FormatOptions {
            keyword_case: keyword_case(f.keyword_case),
            indent: if f.use_tabs { dbiji_sql::IndentStyle::Tab } else { dbiji_sql::IndentStyle::Spaces(f.indent_width.min(16) as u8) },
            comma_first: f.comma_first,
            newline_before_and_or: f.newline_before_and_or,
            max_line_length: f.max_line_length as usize,
            lines_between_statements: f.lines_between_statements as usize,
            ..Default::default()
        };
        let ctx = cx.egui_ctx();
        match self.editor.selection().filter(|r| !r.is_empty()) {
            Some(r) => {
                let formatted = dbiji_sql::format_sql(&self.editor.text()[r.clone()], self.lang.syntax(), &opts);
                self.editor.replace_range(&ctx, r, &formatted);
            }
            None => {
                let formatted = dbiji_sql::format_sql(self.editor.text(), self.lang.syntax(), &opts);
                self.editor.set_text(&ctx, formatted);
            }
        }
    }

    /// Diagnostics and spell checking after the text settled.
    fn analyse(&mut self, cx: &AppContext) {
        if self.analysed_version == self.editor.version() {
            return;
        }
        if self.changed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(400)) {
            return;
        }
        self.analysed_version = self.editor.version();
        if self.exec.is_none() {
            let diags = match self.lang.catalog() {
                Some(c) => dbiji_sql::script_diagnostics(self.editor.text(), self.lang.syntax(), c.as_ref(), &split_options(cx)),
                None => Vec::new(),
            };
            let mapped = diags
                .into_iter()
                .map(|d| {
                    let sev = match d.severity {
                        dbiji_sql::Severity::Error => dbiji_editor::Severity::Error,
                        dbiji_sql::Severity::Warning => dbiji_editor::Severity::Warning,
                        dbiji_sql::Severity::Info => dbiji_editor::Severity::Info,
                    };
                    dbiji_editor::Diagnostic::new(d.start..d.end, sev, d.message)
                })
                .collect();
            self.editor.set_diagnostics(mapped);
        }
        if cx.settings.editor.spell_check {
            let issues = dbiji_sql::check_spelling(self.editor.text(), self.lang.syntax(), dbiji_sql::SpellChecker::english(), &self.user_words);
            self.editor.set_spell_issues(issues.into_iter().map(|i| i.start..i.end).collect());
        } else if !self.editor.spell_issues().is_empty() {
            self.editor.set_spell_issues(Vec::new());
        }
    }

    /// Save the script; consoles have no file. Returns true when written.
    pub fn save(&mut self, cx: &mut AppContext) -> bool {
        if self.console {
            return false;
        }
        let Some(projects) = cx.services.projects.as_ref() else { return false };
        let project = match projects.default_project() {
            Ok(p) => p,
            Err(e) => {
                cx.toasts.error(tr("Cannot save the script"), Some(e.to_string()));
                return false;
            }
        };
        let conn_name = self.conn.as_ref().and_then(|c| cx.conns.config(c)).map(|c| c.name.clone());
        let path = match &self.path {
            Some(p) => p.clone(),
            None => match project.new_script_path(conn_name.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    cx.toasts.error(tr("Cannot save the script"), Some(e.to_string()));
                    return false;
                }
            },
        };
        let text = self.editor.text_with_original_line_endings().into_owned();
        match project.save_script(&path, &text) {
            Ok(p) => {
                let meta = dbiji_config::ScriptMeta {
                    connection_id: self.conn.clone(),
                    database: self.schema.as_ref().and_then(|s| s.database.clone()),
                    schema: self.schema.as_ref().and_then(|s| s.schema.clone()),
                };
                let _ = project.set_script_meta(&p, Some(meta));
                self.title = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| self.title.clone());
                self.path = Some(p);
                self.editor.mark_saved();
                true
            }
            Err(e) => {
                cx.toasts.error(tr("Cannot save the script"), Some(e.to_string()));
                false
            }
        }
    }

    fn set_tx_mode(&mut self, cx: &mut AppContext, mode: TransactionMode) {
        let Some(s) = self.session(cx) else { return };
        cx.backend.spawn_detached(async move {
            let _ = s.set_transaction_mode(mode).await;
        });
    }

    fn commit(&mut self, cx: &mut AppContext, commit: bool) {
        let Some(s) = self.session(cx) else { return };
        let job = cx.backend.spawn(async move { if commit { s.commit().await } else { s.rollback().await } });
        let _ = job;
        self.log("INFO", if commit { tr("Commit") } else { tr("Rollback") });
    }

    fn cancel(&mut self, cx: &mut AppContext) {
        if let Some(e) = &self.exec {
            e.cancel(cx.backend);
            self.log("WARNING", tr("Cancel requested"));
        }
    }

    fn active_panel(&mut self) -> Option<&mut ResultPanel> {
        match self.bottom {
            Bottom::Result(i) => match self.results.get_mut(i) {
                Some(ResultItem::Rows(p)) => Some(p),
                _ => None,
            },
            _ => None,
        }
    }

    // ------------------------------------------------------------------ UI

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Connection selector.
            let conn_name = self.conn.as_ref().and_then(|c| cx.conns.config(c)).map(|c| c.name.clone()).unwrap_or_else(|| tr("No connection").to_string());
            let marker = self.conn.as_ref().and_then(|c| cx.conns.color_of(c)).and_then(crate::theme::connection_marker);
            let mut text = RichText::new(format!("{} {conn_name}", icon::PLUGS));
            if let Some(m) = marker {
                text = text.color(m);
            }
            let r = ui.button(text).on_hover_text(format!("{} ({})", tr("Active connection"), crate::keys::label(cx.keymap, Action::SelectConnection)));
            if r.clicked() {
                self.select_conn_open = !self.select_conn_open;
            }
            let conns: Vec<(String, String)> = cx.conns.configs().iter().map(|c| (c.id.clone(), c.name.clone())).collect();
            let mut chosen: Option<Option<String>> = None;
            egui::Popup::from_response(&r).open_bool(&mut self.select_conn_open).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.set_min_width(220.0);
                if ui.selectable_label(self.conn.is_none(), tr("No connection")).clicked() {
                    chosen = Some(None);
                }
                for (id, name) in &conns {
                    let st = if cx.conns.is_connected(id) { icon::PLUGS_CONNECTED } else { icon::PLUGS };
                    if ui.selectable_label(self.conn.as_deref() == Some(id), format!("{st} {name}")).clicked() {
                        chosen = Some(Some(id.clone()));
                    }
                }
            });
            if let Some(c) = chosen {
                self.select_conn_open = false;
                self.set_connection(cx, c);
            }
            // Schema selector.
            let schema_label = self
                .schema
                .as_ref()
                .and_then(|s| s.schema.clone().or(s.database.clone()))
                .or_else(|| self.conn.as_ref().and_then(|c| cx.conns.live(c)).and_then(|l| l.current_schema.clone().or(l.current_database.clone())))
                .unwrap_or_else(|| "—".into());
            let r =
                ui.button(format!("{} {schema_label}", icon::DATABASE)).on_hover_text(format!("{} ({})", tr("Active schema"), crate::keys::label(cx.keymap, Action::SelectSchema)));
            if r.clicked() {
                self.select_schema_open = !self.select_schema_open;
            }
            let mut chosen_schema: Option<SchemaRef> = None;
            if self.select_schema_open
                && let Some(conn) = self.conn.clone()
            {
                let backend = cx.backend;
                if let Some(live) = cx.conns.live_mut(&conn) {
                    let main = live.main.clone();
                    let multi = live.driver.info().capabilities.multiple_databases;
                    if multi {
                        live.meta.request_databases(backend, &main);
                    }
                    live.meta.request_schemas(backend, &main, None);
                    let dbs: Vec<String> = live.meta.databases.get().map(|v| v.iter().map(|d| d.name.clone()).collect()).unwrap_or_default();
                    let schemas: Vec<String> = live.meta.schemas_of(None).get().map(|v| v.iter().filter(|s| !s.is_system).map(|s| s.name.clone()).collect()).unwrap_or_default();
                    let cur_db = live.current_database.clone();
                    egui::Popup::from_response(&r).open_bool(&mut self.select_schema_open).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                        ui.set_min_width(200.0);
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            if multi && !dbs.is_empty() {
                                ui.weak(tr("Databases"));
                                for d in &dbs {
                                    if ui.selectable_label(cur_db.as_deref() == Some(d.as_str()), format!("{} {d}", icon::DATABASE)).clicked() {
                                        chosen_schema = Some(SchemaRef::new(Some(d.clone()), None));
                                    }
                                }
                                ui.separator();
                            }
                            ui.weak(tr("Schemas"));
                            for s in &schemas {
                                if ui.selectable_label(false, format!("{} {s}", icon::TREE_STRUCTURE)).clicked() {
                                    chosen_schema = Some(SchemaRef::new(None, Some(s.clone())));
                                }
                            }
                        });
                    });
                }
            }
            if let Some(s) = chosen_schema {
                self.select_schema_open = false;
                let db_changed = s.database.is_some() && s.database != self.schema.as_ref().and_then(|x| x.database.clone());
                self.schema = Some(s);
                if db_changed {
                    self.reset_session(cx);
                }
                self.pending_schema = true;
                self.catalog_generation = None;
            }
            ui.separator();
            let running = self.exec.is_some() || self.explain_job.is_some();
            let tip = |a: Action| format!("{} ({})", crate::keys::action_label(a), crate::keys::label(cx.keymap, a));
            if ui.add_enabled(!running, egui::Button::new(icon::PLAY)).on_hover_text(tip(Action::ExecuteStatement)).clicked() {
                self.run(cx, Request { scope: self.scope_for(true), new_tab: false, explain: None, confirmed: false, sql: None });
            }
            if ui.add_enabled(!running, egui::Button::new(icon::PLUS_SQUARE)).on_hover_text(tip(Action::ExecuteInNewTab)).clicked() {
                self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: None, confirmed: false, sql: None });
            }
            if ui.add_enabled(!running, egui::Button::new(icon::FAST_FORWARD)).on_hover_text(tip(Action::ExecuteScript)).clicked() {
                self.run(cx, Request { scope: self.scope_for(false), new_tab: false, explain: None, confirmed: false, sql: None });
            }
            if ui.add_enabled(!running, egui::Button::new(icon::TREE_VIEW)).on_hover_text(tip(Action::ExplainPlan)).clicked() {
                self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: Some(false), confirmed: false, sql: None });
            }
            if ui
                .add_enabled(running, egui::Button::new(RichText::new(icon::STOP).color(crate::theme::error_color(ui.visuals().dark_mode))))
                .on_hover_text(tip(Action::CancelQuery))
                .clicked()
            {
                self.cancel(cx);
            }
            if ui.button(icon::TEXT_INDENT).on_hover_text(tip(Action::FormatSql)).clicked() {
                self.format(cx);
            }
            ui.menu_button(icon::DOTS_THREE_VERTICAL, |ui| {
                for a in [
                    Action::NextStatement,
                    Action::PreviousStatement,
                    Action::CopyStatement,
                    Action::SelectRowCount,
                    Action::EvaluateExpression,
                    Action::OpenDeclaration,
                    Action::MorphToDelimited,
                    Action::TrimTrailingSpaces,
                    Action::SearchWeb,
                    Action::ToggleFold,
                    Action::CollapseAllFolds,
                    Action::ExpandAllFolds,
                ] {
                    let keys = crate::keys::label(cx.keymap, a);
                    let mut b = egui::Button::new(format!("{}  {}", crate::keys::action_icon(a), crate::keys::action_label(a)));
                    if !keys.is_empty() {
                        b = b.shortcut_text(keys);
                    }
                    if ui.add(b).clicked() {
                        ui.close();
                        let ctx = ui.ctx().clone();
                        self.editor_command(&ctx, cx, a);
                    }
                }
            })
            .response
            .on_hover_text(tr("More editor commands"));
            ui.separator();
            // Transaction controls.
            let tx = match &self.session {
                SessionSlot::Ready(s) => Some(s.tx_state()),
                _ => None,
            };
            let manual = tx.as_ref().is_some_and(|t| t.mode == TransactionMode::Manual);
            let pending = tx.as_ref().map(|t| t.log.len()).unwrap_or(0);
            let label = if manual { format!("{} {}", icon::TOGGLE_LEFT, tr("Manual commit")) } else { format!("{} {}", icon::TOGGLE_RIGHT, tr("Auto-commit")) };
            if ui.add_enabled(tx.is_some(), egui::Button::new(label)).on_hover_text(tip(Action::ToggleAutoCommit)).clicked() {
                self.set_tx_mode(cx, if manual { TransactionMode::Auto } else { TransactionMode::Manual });
            }
            if manual {
                if ui.button(format!("{} {}", icon::CHECK_CIRCLE, tr("Commit"))).on_hover_text(tip(Action::Commit)).clicked() {
                    self.commit(cx, true);
                }
                if ui.button(format!("{} {}", icon::ARROW_U_UP_LEFT, tr("Rollback"))).on_hover_text(tip(Action::Rollback)).clicked() {
                    self.commit(cx, false);
                }
                if pending > 0 && ui.small_button(trf("{} pending", &[&pending])).clicked() {
                    self.bottom = Bottom::Transaction;
                    self.show_results = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.show_outline, icon::LIST_BULLETS).on_hover_text(tr("Outline"));
                ui.toggle_value(&mut self.terminal, icon::TERMINAL).on_hover_text(tr("Terminal mode: print results as text"));
                if ui.selectable_label(self.show_results, icon::ROWS).on_hover_text(tip(Action::ToggleResultsPanel)).clicked() {
                    self.show_results = !self.show_results;
                }
                if running {
                    ui.spinner();
                    if let Some(e) = &self.exec {
                        ui.weak(crate::widgets::format_duration(e.started.elapsed()));
                    }
                }
            });
        });
    }

    fn results_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let mut close: Option<usize> = None;
        ui.horizontal(|ui| {
            for i in 0..self.results.len() {
                let (label, dirty) = match &self.results[i] {
                    ResultItem::Rows(p) => (format!("{} {}", icon::TABLE, p.title), p.has_changes()),
                    ResultItem::Plan { .. } => (format!("{} {}", icon::TREE_VIEW, tr("Plan")), false),
                    ResultItem::Error { .. } => (format!("{} {}", icon::WARNING_CIRCLE, tr("Error")), false),
                };
                let label = if dirty { format!("● {label}") } else { label };
                let r = ui.selectable_label(self.bottom == Bottom::Result(i), label);
                if r.clicked() {
                    self.bottom = Bottom::Result(i);
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
                r.context_menu(|ui| {
                    if ui.button(tr("Close")).clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            if !self.stats.is_empty() && ui.selectable_label(self.bottom == Bottom::Statistics, format!("{} {}", icon::LIST_NUMBERS, tr("Statistics"))).clicked() {
                self.bottom = Bottom::Statistics;
            }
            if ui.selectable_label(self.bottom == Bottom::Output, format!("{} {}", icon::TERMINAL_WINDOW, tr("Output"))).clicked() {
                self.bottom = Bottom::Output;
            }
            if ui.selectable_label(self.bottom == Bottom::Transaction, format!("{} {}", icon::CLOCK, tr("Transaction log"))).clicked() {
                self.bottom = Bottom::Transaction;
            }
            let vars = if self.variables.is_empty() { tr("Variables").to_string() } else { format!("{} ({})", tr("Variables"), self.variables.len()) };
            if ui.selectable_label(self.bottom == Bottom::Variables, format!("{} {vars}", icon::BRACKETS_CURLY)).clicked() {
                self.bottom = Bottom::Variables;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(if self.maximized { icon::ARROWS_IN } else { icon::ARROWS_OUT }).on_hover_text(crate::keys::action_label(Action::MaximizeResults)).clicked() {
                    self.maximized = !self.maximized;
                }
            });
        });
        if let Some(i) = close {
            if self.result_has_changes(i) {
                cx.toasts.info(tr("Save or cancel the data changes first."));
            } else {
                self.results.remove(i);
                self.bottom = if self.results.is_empty() { Bottom::Output } else { Bottom::Result(i.min(self.results.len() - 1)) };
            }
        }
        ui.separator();
        match self.bottom {
            Bottom::Result(i) => match self.results.get_mut(i) {
                Some(ResultItem::Rows(p)) => p.ui(ui, cx),
                Some(ResultItem::Plan { sql, plan, state, view }) => {
                    ui.horizontal(|ui| {
                        ui.selectable_value(view, PlanView::Diagram, tr("Diagram"));
                        ui.selectable_value(view, PlanView::Tree, tr("Tree"));
                        ui.selectable_value(view, PlanView::Text, tr("Text"));
                        let s = dbiji_visual::plan::plan_summary(plan);
                        if let Some(ms) = s.execution_ms {
                            ui.weak(trf("Execution {} ms", &[&format!("{ms:.1}")]));
                        }
                        if let Some(c) = s.total_cost {
                            ui.weak(trf("Cost {}", &[&format!("{c:.1}")]));
                        }
                        ui.weak(sql.lines().next().unwrap_or(""));
                    });
                    match view {
                        PlanView::Diagram => {
                            dbiji_visual::plan::show_plan_diagram(ui, plan, state);
                        }
                        PlanView::Tree => {
                            dbiji_visual::plan::show_plan_tree(ui, plan, state);
                        }
                        PlanView::Text => dbiji_visual::plan::show_plan_text(ui, plan),
                    }
                }
                Some(ResultItem::Error { sql, text }) => {
                    crate::widgets::error_box(ui, text);
                    ui.weak(sql.lines().next().unwrap_or(""));
                }
                None => {}
            },
            Bottom::Statistics => {
                let rows: Vec<Vec<String>> = self
                    .stats
                    .iter()
                    .map(|s| {
                        vec![
                            (s.index + 1).to_string(),
                            match s.status {
                                None => "…".into(),
                                Some(true) => tr("OK").to_string(),
                                Some(false) => tr("Error").to_string(),
                            },
                            s.rows.map(|r| r.to_string()).unwrap_or_default(),
                            s.elapsed.map(crate::widgets::format_duration).unwrap_or_default(),
                            s.sql.split_whitespace().collect::<Vec<_>>().join(" "),
                            s.message.clone(),
                        ]
                    })
                    .collect();
                crate::widgets::simple_table(ui, ("stats", self.editor.id()), &["#", tr("Status"), tr("Rows"), tr("Time"), tr("SQL"), tr("Message")], &rows, None);
            }
            Bottom::Output => {
                ui.horizontal(|ui| {
                    if ui.small_button(format!("{} {}", icon::COPY, tr("Copy"))).clicked() {
                        let all: String = self.output.iter().map(|l| format!("{} {} {}\n", l.at.format("%H:%M:%S"), l.severity, l.text)).collect();
                        ui.ctx().copy_text(all);
                    }
                    if ui.small_button(format!("{} {}", icon::TRASH, tr("Clear"))).clicked() {
                        self.output.clear();
                    }
                });
                egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    let dark = ui.visuals().dark_mode;
                    for l in &self.output {
                        let color = match l.severity.as_str() {
                            "ERROR" => crate::theme::error_color(dark),
                            "WARNING" => crate::theme::warning_color(dark),
                            _ => ui.visuals().text_color(),
                        };
                        ui.horizontal_top(|ui| {
                            ui.weak(l.at.format("%H:%M:%S").to_string());
                            ui.add(egui::Label::new(RichText::new(&l.text).monospace().color(color)).selectable(true));
                        });
                    }
                });
            }
            Bottom::Variables => {
                ui.weak(tr("Values of :name and ${name} parameters and @set variables; they are inserted as SQL text."));
                let mut remove: Option<String> = None;
                egui::Grid::new(("vars", self.editor.id())).num_columns(3).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                    for (k, v) in self.variables.iter_mut() {
                        ui.monospace(k);
                        ui.add(egui::TextEdit::singleline(v).desired_width(320.0).code_editor());
                        if ui.small_button(icon::TRASH).on_hover_text(tr("Remove")).clicked() {
                            remove = Some(k.clone());
                        }
                        ui.end_row();
                    }
                });
                if let Some(k) = remove {
                    self.variables.remove(&k);
                }
                ui.horizontal(|ui| {
                    let id = ui.id().with("new_var");
                    let mut name: String = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
                    ui.add(egui::TextEdit::singleline(&mut name).hint_text(tr("name")).desired_width(140.0));
                    if ui.add_enabled(!name.trim().is_empty(), egui::Button::new(format!("{} {}", icon::PLUS, tr("Add variable")))).clicked() {
                        self.variables.entry(name.trim().to_string()).or_default();
                        name.clear();
                    }
                    ui.data_mut(|d| d.insert_temp(id, name));
                    if !self.variables.is_empty() && ui.button(tr("Clear")).clicked() {
                        self.variables.clear();
                    }
                });
            }
            Bottom::Transaction => {
                let log = match &self.session {
                    SessionSlot::Ready(s) => s.tx_state().log,
                    _ => Vec::new(),
                };
                if log.is_empty() {
                    ui.weak(tr("No uncommitted statements."));
                } else {
                    let rows: Vec<Vec<String>> = log
                        .iter()
                        .map(|e| {
                            vec![
                                e.at.format("%H:%M:%S").to_string(),
                                e.rows.map(|r| r.to_string()).unwrap_or_default(),
                                crate::widgets::format_duration(e.duration),
                                if e.ok { tr("OK").to_string() } else { tr("Error").to_string() },
                                e.sql.split_whitespace().collect::<Vec<_>>().join(" "),
                            ]
                        })
                        .collect();
                    crate::widgets::simple_table(ui, ("txlog", self.editor.id()), &[tr("Time"), tr("Rows"), tr("Duration"), tr("Status"), tr("SQL")], &rows, None);
                }
            }
        }
    }

    fn outline_ui(&mut self, ui: &mut Ui) {
        let items = dbiji_sql::outline(self.editor.text(), self.lang.syntax());
        let mut jump: Option<usize> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            fn walk(ui: &mut Ui, items: &[dbiji_sql::OutlineItem], depth: usize, jump: &mut Option<usize>) {
                for it in items {
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 12.0);
                        if ui.selectable_label(false, &it.label).clicked() {
                            *jump = Some(it.start);
                        }
                    });
                    walk(ui, &it.children, depth + 1, jump);
                }
            }
            walk(ui, &items, 0, &mut jump);
        });
        if let Some(off) = jump {
            self.editor.set_cursor(ui.ctx(), off);
            self.editor.request_focus(ui.ctx());
        }
    }

    fn dialogs(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if let Some(p) = self.params.as_mut() {
            let mut run = false;
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new(("params", self.editor.id()))).show(ui.ctx(), |ui| {
                ui.heading(tr("Parameters"));
                ui.label(tr("Values are inserted as SQL text: quote strings, e.g. 'abc'."));
                egui::Grid::new("params_grid").num_columns(2).show(ui, |ui| {
                    for (i, n) in p.names.iter().enumerate() {
                        ui.label(n);
                        let r = ui.add(egui::TextEdit::singleline(&mut p.values[i]).desired_width(260.0));
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            run = true;
                        }
                        if i == 0 && !p.focused {
                            r.request_focus();
                            p.focused = true;
                        }
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Execute")).strong()).clicked() {
                        run = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if run {
                let p = self.params.take().expect("present");
                for (n, v) in p.names.iter().zip(p.values) {
                    self.variables.insert(n.clone(), v);
                }
                self.run(cx, p.request);
            } else if close || modal.should_close() {
                self.params = None;
            }
        }
        if let Some(c) = &self.confirm {
            let mut run = false;
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new(("confirm_exec", self.editor.id()))).show(ui.ctx(), |ui| {
                ui.set_width(520.0);
                ui.heading(format!("{} {}", icon::WARNING, tr("Confirm execution")));
                for r in &c.reasons {
                    ui.label(r);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Execute")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        run = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if run {
                let mut req = self.confirm.take().expect("present").request;
                req.confirmed = true;
                self.run(cx, req);
            } else if close || modal.should_close() {
                self.confirm = None;
            }
        }
        if self.close_prompt {
            let mut close = false;
            let mut decided: Option<bool> = None;
            let modal = egui::Modal::new(egui::Id::new(("close_editor", self.editor.id()))).show(ui.ctx(), |ui| {
                ui.heading(tr("Unsaved changes"));
                ui.label(trf("Save changes to \"{}\"?", &[&self.title]));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Save")).strong()).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("Don't save")).clicked() {
                        decided = Some(false);
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            match decided {
                Some(save) => {
                    if !save || self.save(cx) {
                        self.editor.mark_saved();
                        for r in &mut self.results {
                            if let ResultItem::Rows(p) = r {
                                p.discard();
                            }
                        }
                        self.close_prompt = false;
                        if let Some(id) = self.tab_id {
                            cx.push(AppCommand::CloseTab(id));
                        }
                    }
                }
                None if close || modal.should_close() => self.close_prompt = false,
                None => {}
            }
        }
    }

    fn handle_events(&mut self, events: Vec<EditorEvent>, cx: &mut AppContext) {
        for ev in events {
            match ev {
                EditorEvent::FocusChanged(f) => self.editor_focused = f,
                EditorEvent::SaveRequested => {
                    self.save(cx);
                }
                EditorEvent::LinkClicked { range, .. } => self.open_declaration(cx, range.start),
                EditorEvent::ContextMenu(a) => match a {
                    ContextAction::Execute => self.run(cx, Request { scope: self.scope_for(true), new_tab: false, explain: None, confirmed: false, sql: None }),
                    ContextAction::ExecuteScript => self.run(cx, Request { scope: self.scope_for(false), new_tab: false, explain: None, confirmed: false, sql: None }),
                    ContextAction::ExecuteNewTab => self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: None, confirmed: false, sql: None }),
                    ContextAction::Explain => self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: Some(false), confirmed: false, sql: None }),
                    ContextAction::Format => self.format(cx),
                    ContextAction::AddSpellingWord(w) => {
                        self.user_words.insert(w.to_lowercase());
                        self.analysed_version = 0;
                    }
                    _ => {}
                },
                EditorEvent::CursorMoved(_) | EditorEvent::CompletionAccepted(_) => {}
            }
        }
    }
}

fn result_title(sql: &str, n: usize) -> String {
    let words: Vec<&str> = sql.split_whitespace().take(6).collect();
    let mut t = words.join(" ");
    if t.chars().count() > 28 {
        t = t.chars().take(27).collect::<String>() + "…";
    }
    if t.is_empty() { trf("Result {}", &[&n]) } else { t }
}

impl TabView for SqlEditorTab {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn icon(&self) -> &'static str {
        if self.console { icon::TERMINAL_WINDOW } else { icon::FILE_SQL }
    }

    fn tooltip(&self) -> Option<String> {
        self.path.as_ref().map(|p| p.display().to_string())
    }

    fn connection_id(&self) -> Option<&str> {
        self.conn.as_deref()
    }

    fn dedupe_key(&self) -> Option<String> {
        self.path.as_ref().map(|p| format!("script:{}", p.display()))
    }

    fn is_dirty(&self) -> bool {
        (!self.console && self.editor.is_modified()) || self.results.iter().any(|r| matches!(r, ResultItem::Rows(p) if p.has_changes()))
    }

    fn focus_context(&self) -> ActionContext {
        if self.editor_focused {
            ActionContext::SqlEditor
        } else if matches!(self.bottom, Bottom::Result(_)) {
            ActionContext::ResultsGrid
        } else {
            ActionContext::Global
        }
    }

    fn on_action(&mut self, action: Action, cx: &mut AppContext) -> bool {
        use Action as A;
        let ctx = cx.egui_ctx();
        match action {
            A::NextStatement
            | A::PreviousStatement
            | A::CopyStatement
            | A::SelectRowCount
            | A::EvaluateExpression
            | A::OpenDeclaration
            | A::MorphToDelimited
            | A::TrimTrailingSpaces
            | A::SearchWeb
            | A::ToggleFold
            | A::CollapseAllFolds
            | A::ExpandAllFolds => self.editor_command(&ctx, cx, action),
            A::ExecuteStatement => self.run(cx, Request { scope: self.scope_for(true), new_tab: false, explain: None, confirmed: false, sql: None }),
            A::ExecuteSelection => self.run(cx, Request { scope: self.scope_for(true), new_tab: false, explain: None, confirmed: false, sql: None }),
            A::ExecuteScript => self.run(cx, Request { scope: self.scope_for(false), new_tab: false, explain: None, confirmed: false, sql: None }),
            A::ExecuteInNewTab => self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: None, confirmed: false, sql: None }),
            A::ExplainPlan | A::ShowPlanDiagram => self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: Some(false), confirmed: false, sql: None }),
            A::ExplainAnalyze => self.run(cx, Request { scope: self.scope_for(true), new_tab: true, explain: Some(true), confirmed: false, sql: None }),
            A::CancelQuery => self.cancel(cx),
            A::SelectConnection => self.select_conn_open = true,
            A::SelectSchema => self.select_schema_open = true,
            A::ToggleResultsPanel => self.show_results = !self.show_results,
            A::MaximizeResults => self.maximized = !self.maximized,
            A::FormatSql => self.format(cx),
            A::Save => {
                if self.save(cx) {
                    return true;
                }
                if let Some(p) = self.active_panel() {
                    p.save(cx);
                }
            }
            A::SaveData => {
                if let Some(p) = self.active_panel() {
                    p.save(cx);
                }
            }
            A::CancelDataChanges => {
                if let Some(p) = self.active_panel() {
                    p.discard();
                }
            }
            A::Find => self.editor.open_find(false),
            A::Replace => self.editor.open_find(true),
            A::FindNext | A::FindPrevious => self.editor.open_find(false),
            A::GoToLine => self.editor.open_goto_line(),
            A::Autocomplete => self.editor.trigger_completion(),
            A::ToggleComment | A::ToggleBlockComment | A::UpperCase | A::LowerCase => {
                let ca = match action {
                    A::ToggleComment => ContextAction::ToggleComment,
                    A::ToggleBlockComment => ContextAction::ToggleBlockComment,
                    A::UpperCase => ContextAction::UpperCase,
                    _ => ContextAction::LowerCase,
                };
                let opts = editor_options(cx);
                self.editor.apply_action(&ctx, &self.lang, &opts, &ca);
            }
            A::Commit => self.commit(cx, true),
            A::Rollback => self.commit(cx, false),
            A::ToggleAutoCommit => {
                let manual = matches!(&self.session, SessionSlot::Ready(s) if s.tx_state().mode == TransactionMode::Manual);
                self.set_tx_mode(cx, if manual { TransactionMode::Auto } else { TransactionMode::Manual });
            }
            A::Refresh => {
                if let Some(p) = self.active_panel() {
                    p.src.refresh(cx.backend, cx.settings.results.effective_fetch_size() as usize);
                } else {
                    return false;
                }
            }
            A::Export => {
                let sql = match self.active_panel() {
                    Some(p) => p.sql().to_string(),
                    None => return false,
                };
                if let Some(c) = &self.conn {
                    cx.push(AppCommand::OpenExport { connection: c.clone(), objects: Vec::new(), sql: Some(sql) });
                }
            }
            _ => return false,
        }
        true
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        if self.is_dirty() {
            self.close_prompt = true;
            return false;
        }
        if let Some(e) = &self.exec {
            e.cancel(cx.backend);
        }
        self.reset_session(cx);
        true
    }

    fn session_state(&self) -> Option<serde_json::Value> {
        let tab = dbiji_config::SessionTab {
            kind: if self.console { dbiji_config::project::TabKind::SqlConsole } else { dbiji_config::project::TabKind::SqlScript },
            title: Some(self.title.clone()),
            path: self.path.clone(),
            connection_id: self.conn.clone(),
            database: self.schema.as_ref().and_then(|s| s.database.clone()),
            schema: self.schema.as_ref().and_then(|s| s.schema.clone()),
            cursor: Some(self.editor.byte_to_char(self.editor.cursor())),
            content: if self.console || self.path.is_none() { Some(self.editor.text().to_string()) } else { None },
            ..Default::default()
        };
        serde_json::to_value(tab).ok()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let ctx = ui.ctx().clone();
        self.tab_id = cx.current_tab;
        self.dialect_sync(cx);
        if self.conn.as_ref().is_some_and(|c| cx.conns.is_connected(c)) {
            self.catalog_sync(cx);
            // Open the session early and apply the active schema.
            if let Some(s) = self.session(cx)
                && self.pending_schema
            {
                self.pending_schema = false;
                if let Some(schema) = self.schema.as_ref().and_then(|s| s.schema.clone()) {
                    let s2 = s.clone();
                    cx.backend.spawn_detached(async move {
                        let _ = s2.lock().await.set_current_schema(&schema).await;
                    });
                }
            }
        }
        if let SessionSlot::Failed(e) = &self.session {
            let e = e.clone();
            ui.horizontal(|ui| {
                crate::widgets::error_box(ui, &e);
                if ui.button(tr("Retry")).clicked() {
                    self.session = SessionSlot::None;
                }
            });
        }
        if self.auto_run && self.session_ready() && self.exec.is_none() {
            self.auto_run = false;
            self.run(cx, Request { scope: Scope::Script, new_tab: true, explain: None, confirmed: true, sql: None });
        }
        self.poll_exec(&ctx, cx);
        self.toolbar(ui, cx);
        ui.separator();
        // Status line.
        egui::Panel::bottom(egui::Id::new(("sql_status", self.editor.id()))).exact_size(18.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (l, c) = self.editor.line_col();
                ui.weak(format!("{l}:{c}"));
                if let Some(sel) = self.editor.selected_text() {
                    ui.weak(trf("{} selected", &[&sel.chars().count()]));
                }
                if let Some((_, d)) = self.last_run {
                    ui.weak(trf("last run {}", &[&crate::widgets::format_duration(d)]));
                }
                if self.editor.is_modified() && !self.console {
                    ui.weak(tr("modified"));
                }
            });
        });
        let results_visible = self.show_results;
        if results_visible && self.maximized {
            self.results_ui(ui, cx);
            self.dialogs(ui, cx);
            return;
        }
        if results_visible {
            let right = cx.settings.results.layout == dbiji_config::settings::ResultsLayout::Right;
            let id = egui::Id::new(("sql_results", self.editor.id()));
            if right {
                egui::Panel::right(id).resizable(true).default_size(ui.available_width() * 0.45).show(ui, |ui| self.results_ui(ui, cx));
            } else {
                egui::Panel::bottom(id).resizable(true).default_size(ui.available_height() * 0.45).size_range(80.0..=f32::INFINITY).show(ui, |ui| self.results_ui(ui, cx));
            }
        }
        if self.show_outline {
            egui::Panel::right(egui::Id::new(("sql_outline", self.editor.id()))).resizable(true).default_size(200.0).show(ui, |ui| self.outline_ui(ui));
        }
        let opts = editor_options(cx);
        let theme = EditorTheme::from_visuals(ui.visuals());
        if self.request_focus {
            self.request_focus = false;
            self.editor.request_focus(&ctx);
        }
        let resp = egui::CentralPanel::default().show(ui, |ui| SqlEditor::new(&mut self.editor, &self.lang, &opts, &theme).show(ui)).inner;
        if resp.changed {
            self.changed_at = Some(Instant::now());
        }
        if self.editor.version() != self.last_version {
            self.last_version = self.editor.version();
            self.changed_at = Some(Instant::now());
        }
        self.handle_events(resp.events, cx);
        self.analyse(cx);
        if self.changed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(450)) {
            ctx.request_repaint_after(Duration::from_millis(450));
        }
        // Auto-save.
        let interval = cx.settings.editor.auto_save_interval_secs;
        if interval > 0 && self.path.is_some() && self.editor.is_modified() && self.changed_at.is_some_and(|t| t.elapsed() > Duration::from_secs(interval as u64)) {
            self.save(cx);
        }
        self.dialogs(ui, cx);
    }

    crate::impl_tab_any!();
}

/// Inspection and dialog helpers for headless tests.
#[doc(hidden)]
impl SqlEditorTab {
    pub fn is_running(&self) -> bool {
        self.exec.is_some() || self.explain_job.is_some()
    }
    pub fn session_ready(&self) -> bool {
        matches!(self.session, SessionSlot::Ready(_))
    }
    pub fn result_count(&self) -> usize {
        self.results.len()
    }
    /// Rows fetched so far by result `i` (`None` for plans and errors).
    pub fn result_rows(&self, i: usize) -> Option<usize> {
        match self.results.get(i)? {
            ResultItem::Rows(p) => Some(p.src.row_count()),
            _ => None,
        }
    }
    pub fn result_is_plan(&self, i: usize) -> bool {
        matches!(self.results.get(i), Some(ResultItem::Plan { .. }))
    }
    pub fn result_is_error(&self, i: usize) -> bool {
        matches!(self.results.get(i), Some(ResultItem::Error { .. }))
    }
    pub fn stats_len(&self) -> usize {
        self.stats.len()
    }
    pub fn output_text(&self) -> String {
        self.output.iter().map(|l| format!("{} {}\n", l.severity, l.text)).collect()
    }
    pub fn diagnostics_len(&self) -> usize {
        self.editor.diagnostics().len()
    }
    pub fn pending_params(&self) -> Option<Vec<String>> {
        self.params.as_ref().map(|p| p.names.clone())
    }
    /// Fill the parameter prompt and run.
    pub fn answer_params(&mut self, cx: &mut AppContext, values: &[&str]) {
        if let Some(p) = self.params.take() {
            for (n, v) in p.names.iter().zip(values) {
                self.variables.insert(n.clone(), v.to_string());
            }
            self.run(cx, p.request);
        }
    }
    pub fn pending_confirmation(&self) -> Option<Vec<String>> {
        self.confirm.as_ref().map(|c| c.reasons.clone())
    }
    pub fn answer_confirmation(&mut self, cx: &mut AppContext, run: bool) {
        if let Some(c) = self.confirm.take()
            && run
        {
            let mut req = c.request;
            req.confirmed = true;
            self.run(cx, req);
        }
    }
    pub fn set_text(&mut self, ctx: &egui::Context, text: &str) {
        self.editor.reset_text(ctx, text.to_string());
    }
    pub fn editor_cursor(&self) -> usize {
        self.editor.cursor()
    }
    pub fn select_range(&mut self, ctx: &egui::Context, range: std::ops::Range<usize>) {
        self.editor.select(ctx, range);
    }
    /// Foldable regions (first line, last line).
    pub fn fold_regions(&self) -> Vec<(usize, usize)> {
        self.editor.fold_regions()
    }
    /// First lines of the collapsed folds.
    pub fn folded_lines(&self) -> Vec<usize> {
        self.editor.folded_lines()
    }
    /// The autocomplete catalog of the connection is loaded.
    pub fn has_catalog(&self) -> bool {
        self.lang.catalog().is_some()
    }
    pub fn set_cursor(&mut self, ctx: &egui::Context, offset: usize) {
        self.editor.set_cursor(ctx, offset);
    }
    pub fn variables(&self) -> &BTreeMap<String, String> {
        &self.variables
    }
}
