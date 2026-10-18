//! AI assistant (PRD phase 3, DBeaver "AI smart assistance"): a chat with
//! streamed answers, quick actions (generate SQL, explain, fix an error,
//! optimize, describe an object), schema context from the metadata cache and
//! SQL blocks that can be copied or opened in a SQL editor.
//!
//! Privacy: nothing is sent while the assistant is disabled in the settings.
//! A request carries the instructions, the schema structure (only when
//! "include schema context" is on), sample rows (only when "send sample data"
//! is on, and only for "Describe object") and what the user typed. The API
//! key is read from the secret store when a request starts and is never shown
//! or logged.

// `DbError` carries the full server diagnostics (144 bytes); every `DbResult` triggers this lint.
#![allow(clippy::result_large_err)]

use crate::backend::Job;
use crate::command::AppCommand;
use crate::conn::session::SessionHandle;
use crate::cx::AppContext;
use crate::sqllang::SqlLanguage;
use crate::tabs::TabView;
use dbiji_ai::prompts;
use dbiji_ai::{AiClient, AiConfig, AiProviderKind, ChatMessage, ChatRequest, ChatResponse, SchemaContextBuilder, Usage};
use dbiji_config::settings::{AiProvider, AiSettings};
use dbiji_core::ddl::SchemaSnapshot;
use dbiji_core::i18n::Locale;
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbError, DbKind, DbResult, Dialect, ErrorKind, Secret, tr, trf};
use dbiji_editor::{EditorLanguage, EditorTheme, TokenStyle};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Character budget of the schema description put into the system prompt.
pub const SCHEMA_CONTEXT_BUDGET: usize = prompts::DEFAULT_CONTEXT_BUDGET;
/// Character budget of earlier messages repeated in a request (oldest dropped first).
pub const HISTORY_BUDGET: usize = 100_000;
/// Rows read for "Describe object" when sample data may be sent.
pub const SAMPLE_ROWS: u64 = 5;
/// Longest value text of a sample row.
const SAMPLE_VALUE_CHARS: usize = 120;

/// Fence tags treated as SQL (same list as `dbiji_ai::extract_sql_blocks`).
const SQL_TAGS: &[&str] = &["sql", "postgresql", "postgres", "pgsql", "plpgsql", "mysql", "mariadb", "sqlite", "tsql", "t-sql", "mssql", "plsql", "clickhouse", "duckdb"];

// ------------------------------------------------------------ quick actions

/// What the user asks the assistant to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuickAction {
    /// Free conversation.
    Chat,
    /// Write a query from a description.
    GenerateSql,
    /// Explain a statement.
    Explain,
    /// Explain an error and correct the statement.
    FixError,
    /// Make a query faster (optionally with its execution plan).
    Optimize,
    /// Describe a table, view or routine from its DDL.
    Describe,
}

impl QuickAction {
    pub const ALL: [QuickAction; 6] = [QuickAction::Chat, QuickAction::GenerateSql, QuickAction::Explain, QuickAction::FixError, QuickAction::Optimize, QuickAction::Describe];

    pub fn label(self) -> &'static str {
        match self {
            QuickAction::Chat => tr("Chat"),
            QuickAction::GenerateSql => tr("Generate SQL"),
            QuickAction::Explain => tr("Explain query"),
            QuickAction::FixError => tr("Fix error"),
            QuickAction::Optimize => tr("Optimize query"),
            QuickAction::Describe => tr("Describe object"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            QuickAction::Chat => icon::CHAT_TEXT,
            QuickAction::GenerateSql => icon::MAGIC_WAND,
            QuickAction::Explain => icon::LIGHTBULB,
            QuickAction::FixError => icon::WRENCH,
            QuickAction::Optimize => icon::LIGHTNING,
            QuickAction::Describe => icon::INFO,
        }
    }

    /// The main input holds SQL (code editor style).
    fn takes_sql(self) -> bool {
        matches!(self, QuickAction::Explain | QuickAction::FixError | QuickAction::Optimize)
    }

    fn hint(self) -> &'static str {
        match self {
            QuickAction::Chat => tr("Ask anything about SQL or this database…"),
            QuickAction::GenerateSql => tr("Describe the data you need, e.g. \"customers with more than 3 orders last month\""),
            QuickAction::Explain => tr("Paste the SQL statement to explain"),
            QuickAction::FixError => tr("Paste the SQL statement that failed"),
            QuickAction::Optimize => tr("Paste the query to optimize"),
            QuickAction::Describe => tr("Paste a CREATE statement, or choose an object above"),
        }
    }
}

/// Inputs of a quick action: what the user typed plus what was read from the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionInput {
    /// Chat text, description or SQL statement.
    pub text: String,
    /// "Fix error": the error message reported by the database.
    pub error: String,
    /// "Optimize": the execution plan as text.
    pub plan: Option<String>,
    /// "Describe": the object's DDL (read from the database, or `text`).
    pub ddl: Option<String>,
    /// "Describe": sample rows as text (only when sample data may be sent).
    pub sample: Option<String>,
}

/// A fenced block whose fence is longer than any backtick run in `content`.
fn fence(lang: &str, content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let f = "`".repeat((longest + 1).max(3));
    format!("{f}{lang}\n{}\n{f}", content.trim_end())
}

/// The user message of a quick action, built with the `dbiji_ai::prompts` helpers.
pub fn user_message(action: QuickAction, input: &ActionInput) -> DbResult<ChatMessage> {
    let text = input.text.trim();
    let need = |ok: bool, what: &str| if ok { Ok(()) } else { Err(DbError::config(what.to_string())) };
    match action {
        QuickAction::Chat => {
            need(!text.is_empty(), tr("Type a question first."))?;
            Ok(prompts::chat(text))
        }
        QuickAction::GenerateSql => {
            need(!text.is_empty(), tr("Describe the query you need first."))?;
            Ok(prompts::generate_sql(text))
        }
        QuickAction::Explain => {
            need(!text.is_empty(), tr("Enter the SQL statement first."))?;
            Ok(prompts::explain_query(text))
        }
        QuickAction::FixError => {
            need(!text.is_empty(), tr("Enter the SQL statement first."))?;
            need(!input.error.trim().is_empty(), tr("Enter the error message first."))?;
            Ok(prompts::fix_error(text, input.error.trim()))
        }
        QuickAction::Optimize => {
            need(!text.is_empty(), tr("Enter the SQL statement first."))?;
            Ok(prompts::optimize_query(text, input.plan.as_deref()))
        }
        QuickAction::Describe => {
            let ddl = input.ddl.as_deref().map(str::trim).filter(|d| !d.is_empty()).unwrap_or(text);
            need(!ddl.is_empty(), tr("Choose an object or paste its definition first."))?;
            let mut m = prompts::describe_object(ddl);
            if let Some(sample) = input.sample.as_deref().filter(|s| !s.trim().is_empty()) {
                m.content.push_str("\n\nSample rows (the first rows of the table, for illustration only):\n");
                m.content.push_str(&fence("", sample));
            }
            Ok(m)
        }
    }
}

/// What the chat shows as the user's message (markdown-like; SQL in fences).
fn shown_text(action: QuickAction, input: &ActionInput, object: Option<&ObjectRef>) -> String {
    let text = input.text.trim();
    match action {
        QuickAction::Chat | QuickAction::GenerateSql => text.to_string(),
        QuickAction::Explain | QuickAction::Optimize => fence("sql", text),
        QuickAction::FixError => format!("{}\n\n{}", fence("sql", text), fence("text", input.error.trim())),
        QuickAction::Describe => match object {
            Some(o) => format!("{} {}", tr(o.kind.label()), o.display_path()),
            None => fence("sql", text),
        },
    }
}

// ------------------------------------------------------- request assembly

/// Database facts for the system prompt.
#[derive(Debug, Clone, Copy)]
pub struct RequestContext<'a> {
    /// Product of the connection (`Generic` without a connection).
    pub kind: DbKind,
    /// Server version string ("" when unknown).
    pub server_version: &'a str,
    /// Light snapshot of the selected schema.
    pub snapshot: Option<&'a SchemaSnapshot>,
    /// `AiSettings::include_schema_context`.
    pub include_schema: bool,
    /// Answer language.
    pub locale: Locale,
}

/// The complete request: system prompt (instructions, dialect, schema
/// context ranked by `question`), earlier exchanges that fit
/// [`HISTORY_BUDGET`] and the new user message last.
pub fn build_request(ctx: &RequestContext<'_>, history: &[(ChatMessage, String)], message: ChatMessage, question: &str) -> ChatRequest {
    let schema_text = match (ctx.include_schema, ctx.snapshot) {
        (true, Some(s)) => SchemaContextBuilder::new().snapshot(s).question(question).budget(SCHEMA_CONTEXT_BUDGET).build(),
        _ => String::new(),
    };
    let system = prompts::system_prompt(ctx.kind, ctx.server_version, &schema_text, ctx.locale);
    let mut used = message.content.len();
    let mut kept: Vec<(&ChatMessage, &String)> = Vec::new();
    for (q, a) in history.iter().rev() {
        if q.content.trim().is_empty() || a.trim().is_empty() {
            continue;
        }
        let size = q.content.len() + a.len();
        if used + size > HISTORY_BUDGET {
            break;
        }
        used += size;
        kept.push((q, a));
    }
    kept.reverse();
    let mut messages = Vec::with_capacity(kept.len() * 2 + 1);
    for (q, a) in kept {
        messages.push(ChatMessage::user(q.content.clone()));
        messages.push(ChatMessage::assistant(a.clone()));
    }
    messages.push(message);
    ChatRequest::new(system, messages)
}

/// Client configuration from the preferences and the stored API key.
pub fn ai_config(settings: &AiSettings, api_key: Option<Secret>) -> AiConfig {
    let provider = match settings.provider {
        AiProvider::Anthropic => AiProviderKind::Anthropic,
        AiProvider::OpenAiCompatible => AiProviderKind::OpenAiCompatible,
    };
    let anthropic = provider == AiProviderKind::Anthropic;
    AiConfig {
        provider,
        base_url: settings.base_url.clone().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()),
        model: settings.model.trim().to_string(),
        api_key: api_key.filter(|k| !k.expose().trim().is_empty()),
        max_tokens: settings.max_tokens,
        effort: anthropic.then(|| settings.effort.as_str().to_string()),
        server_fallback: anthropic,
        ..AiConfig::default()
    }
}

fn provider_label(p: AiProvider) -> &'static str {
    match p {
        AiProvider::Anthropic => "Anthropic",
        AiProvider::OpenAiCompatible => tr("OpenAI compatible"),
    }
}

// ------------------------------------------------------------- answer text

/// A piece of an answer: prose or a fenced code block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerSegment {
    Text(String),
    Code {
        /// Fence info word, lower case ("sql", "json", "" ...).
        lang: String,
        code: String,
        /// The block is SQL: tagged with a SQL dialect, or untagged when the
        /// answer has no tagged SQL block (as `dbiji_ai::extract_sql_blocks`).
        sql: bool,
    },
}

fn is_sql_tag(tag: &str) -> bool {
    SQL_TAGS.contains(&tag)
}

/// Split an answer into prose and code blocks. An unterminated last fence
/// (while streaming) is a code block too.
pub fn answer_segments(text: &str) -> Vec<AnswerSegment> {
    let mut out: Vec<AnswerSegment> = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let flush = |prose: &mut Vec<&str>, out: &mut Vec<AnswerSegment>| {
        let t = prose.join("\n");
        prose.clear();
        let t = t.trim_matches('\n').trim_end();
        if !t.trim().is_empty() {
            out.push(AnswerSegment::Text(t.to_string()));
        }
    };
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let fence_char = match trimmed.chars().next() {
            Some(c @ ('`' | '~')) => c,
            _ => {
                prose.push(line);
                continue;
            }
        };
        let fence_len = trimmed.chars().take_while(|c| *c == fence_char).count();
        if fence_len < 3 {
            prose.push(line);
            continue;
        }
        let info = trimmed[fence_len * fence_char.len_utf8()..].trim();
        let closing = fence_char.to_string().repeat(fence_len);
        if let Some(inner) = info.strip_suffix(closing.as_str()) {
            // single line block: ```sql SELECT 1```
            let mut parts = inner.trim().splitn(2, char::is_whitespace);
            let tag = parts.next().unwrap_or("").to_lowercase();
            let body = parts.next().unwrap_or("").trim().to_string();
            if body.is_empty() {
                prose.push(line);
                continue;
            }
            flush(&mut prose, &mut out);
            out.push(AnswerSegment::Code { lang: tag, code: body, sql: false });
            continue;
        }
        flush(&mut prose, &mut out);
        let tag = info.split_whitespace().next().unwrap_or("").to_lowercase();
        let mut body = Vec::new();
        for l in lines.by_ref() {
            let t = l.trim();
            if t.chars().count() >= fence_len && t.chars().all(|c| c == fence_char) {
                break;
            }
            body.push(l);
        }
        let code = body.join("\n").trim().to_string();
        if !code.is_empty() {
            out.push(AnswerSegment::Code { lang: tag, code, sql: false });
        }
    }
    flush(&mut prose, &mut out);
    let any_tagged = out.iter().any(|s| matches!(s, AnswerSegment::Code { lang, .. } if is_sql_tag(lang)));
    for s in &mut out {
        if let AnswerSegment::Code { lang, sql, .. } = s {
            *sql = is_sql_tag(lang) || (lang.is_empty() && !any_tagged);
        }
    }
    out
}

/// The SQL blocks of an answer, in order.
pub fn sql_blocks(text: &str) -> Vec<String> {
    answer_segments(text)
        .into_iter()
        .filter_map(|s| match s {
            AnswerSegment::Code { code, sql: true, .. } => Some(code),
            _ => None,
        })
        .collect()
}

/// Highlighted, non-wrapping layout of SQL text.
fn highlight_job(code: &str, lang: Option<&SqlLanguage>, theme: &EditorTheme, font: egui::FontId) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut pos = 0;
    if let Some(lang) = lang {
        let mut tokens = lang.highlight(code);
        tokens.sort_by_key(|(r, _)| r.start);
        for (r, style) in tokens {
            if r.start < pos || r.end > code.len() || !code.is_char_boundary(r.start) || !code.is_char_boundary(r.end) {
                continue;
            }
            if r.start > pos {
                job.append(&code[pos..r.start], 0.0, fmt(theme.token_color(TokenStyle::Plain)));
            }
            job.append(&code[r.clone()], 0.0, fmt(theme.token_color(style)));
            pos = r.end;
        }
    }
    if pos < code.len() {
        job.append(&code[pos..], 0.0, fmt(theme.token_color(TokenStyle::Plain)));
    }
    job.wrap.max_width = f32::INFINITY;
    job
}

// -------------------------------------------------------------- the chat

/// State of one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnState {
    /// Reading the plan / definition / sample rows from the database.
    Preparing,
    Streaming,
    Done,
    /// Stopped by the user; the partial answer is kept.
    Stopped,
    Failed(String),
    /// The model declined; streamed text was discarded.
    Refused(String),
}

/// One question and its answer.
#[derive(Debug, Clone)]
pub struct Turn {
    pub action: QuickAction,
    /// What the chat shows as the question.
    pub shown: String,
    /// The message sent to the model (known once prepared).
    pub message: Option<ChatMessage>,
    pub answer: String,
    pub state: TurnState,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    pub truncated: bool,
    pub fallback_from: Option<String>,
    /// Parsed `answer`, keyed by its length (answers only grow while streaming).
    segments: Option<(usize, Vec<AnswerSegment>)>,
}

impl Turn {
    fn new(action: QuickAction, shown: String) -> Turn {
        Turn { action, shown, message: None, answer: String::new(), state: TurnState::Preparing, model: None, usage: None, truncated: false, fallback_from: None, segments: None }
    }

    /// Part of the conversation sent with later questions.
    fn in_history(&self) -> bool {
        self.message.is_some() && matches!(self.state, TurnState::Done | TurnState::Stopped) && !self.answer.trim().is_empty()
    }

    fn segments(&mut self) -> &[AnswerSegment] {
        let len = self.answer.len();
        if self.segments.as_ref().is_none_or(|(l, _)| *l != len) {
            self.segments = Some((len, answer_segments(&self.answer)));
        }
        &self.segments.as_ref().expect("just set").1
    }
}

enum Event {
    Prepared(ChatMessage),
    Delta(String),
}

struct Pending {
    turn: usize,
    job: Job<DbResult<ChatResponse>>,
    rx: mpsc::Receiver<Event>,
    cancel: CancellationToken,
}

/// Database work done before a request (in the background).
enum Prep {
    None,
    Plan { session: SessionHandle, sql: String },
    Describe { session: SessionHandle, dialect: Arc<dyn Dialect>, object: ObjectRef, sample: bool },
}

fn short_value(s: &str) -> String {
    let one_line = s.replace(['\n', '\r', '\t'], " ");
    match one_line.char_indices().nth(SAMPLE_VALUE_CHARS) {
        Some((i, _)) => format!("{}…", &one_line[..i]),
        None => one_line,
    }
}

impl Prep {
    async fn run(self, input: &mut ActionInput) -> DbResult<()> {
        match self {
            Prep::None => Ok(()),
            Prep::Plan { session, sql } => {
                // Without a plan the request is still useful: ignore errors.
                if let Ok(plan) = session.lock().await.explain(&sql, false).await {
                    let text = if plan.text.trim().is_empty() { plan.to_text_tree() } else { plan.text.clone() };
                    input.plan = Some(text).filter(|t| !t.trim().is_empty());
                }
                Ok(())
            }
            Prep::Describe { session, dialect, object, sample } => {
                let ddl = session.lock().await.object_ddl(&object).await.map_err(|e| e.with_context(trf("Reading the definition of {}", &[&object.display_path()])))?;
                input.ddl = Some(ddl);
                if sample && object.kind.has_data() {
                    let sql = dialect.limit_query(&dialect.select_table(&object, None, None, None, None), SAMPLE_ROWS, 0);
                    if let Ok((cols, rows)) = session.query_all(&sql, &[]).await {
                        let mut text = cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t");
                        for r in rows {
                            text.push('\n');
                            text.push_str(&r.iter().map(|v| if v.is_null() { "NULL".to_string() } else { short_value(&v.to_text()) }).collect::<Vec<_>>().join("\t"));
                        }
                        input.sample = Some(text);
                    }
                }
                Ok(())
            }
        }
    }
}

/// Whether requests can be sent with the current preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiStatus {
    Disabled,
    MissingKey(String),
    Invalid(String),
    Ready,
}

pub struct AiAssistantTab {
    uid: u64,
    conn: Option<String>,
    schema: Option<SchemaRef>,
    action: QuickAction,
    input: String,
    error_input: String,
    include_plan: bool,
    object: Option<ObjectRef>,
    turns: Vec<Turn>,
    pending: Option<Pending>,
    /// Provider whose key presence was checked, the result and when.
    key_check: Option<(AiProvider, Result<bool, String>, Instant)>,
    notice: Option<String>,
    auto_send: bool,
    auto_since: Option<Instant>,
    connect_requested: bool,
    dedupe: bool,
    lang: Option<(DbKind, SqlLanguage)>,
    focus_input: bool,
}

impl AiAssistantTab {
    pub fn new(conn: Option<String>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        AiAssistantTab {
            uid: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            conn,
            schema: None,
            action: QuickAction::Chat,
            input: String::new(),
            error_input: String::new(),
            include_plan: true,
            object: None,
            turns: Vec::new(),
            pending: None,
            key_check: None,
            notice: None,
            auto_send: false,
            auto_since: None,
            connect_requested: false,
            dedupe: true,
            lang: None,
            focus_input: true,
        }
    }

    /// A new conversation about `sql`: "Fix error" when `error` is given,
    /// "Explain query" otherwise. The question is sent as soon as the
    /// assistant is enabled and configured (the user asked for it).
    pub fn ask_about(conn: Option<String>, sql: String, error: Option<String>) -> Self {
        let mut t = AiAssistantTab::new(conn);
        t.dedupe = false;
        t.action = if error.is_some() { QuickAction::FixError } else { QuickAction::Explain };
        t.input = sql;
        t.error_input = error.unwrap_or_default();
        t.auto_send = true;
        t
    }

    /// Like [`AiAssistantTab::ask_about`] with a chosen action, without sending.
    pub fn with_action(conn: Option<String>, action: QuickAction, text: String) -> Self {
        let mut t = AiAssistantTab::new(conn);
        t.dedupe = false;
        t.action = action;
        t.input = text;
        t
    }

    fn kind(&self, cx: &AppContext) -> DbKind {
        self.conn.as_ref().and_then(|c| cx.conns.live(c)).map(|l| l.kind()).unwrap_or(DbKind::Generic)
    }

    /// Readiness from the preferences and the stored key (checked at most every few seconds).
    pub fn status(&mut self, cx: &AppContext) -> AiStatus {
        let ai = &cx.settings.ai;
        if !ai.enabled {
            return AiStatus::Disabled;
        }
        let stale = match &self.key_check {
            Some((p, r, at)) => *p != ai.provider || (!matches!(r, Ok(true)) && at.elapsed() > Duration::from_secs(2)),
            None => true,
        };
        if stale {
            let r = match cx.services.secrets.read().ai_api_key(ai.provider.id()) {
                Ok(k) => Ok(k.is_some_and(|k| !k.expose().trim().is_empty())),
                Err(e) => Err(e.full_text()),
            };
            self.key_check = Some((ai.provider, r, Instant::now()));
        }
        let present = match &self.key_check {
            Some((_, Ok(p), _)) => *p,
            Some((_, Err(e), _)) => return AiStatus::Invalid(e.clone()),
            None => false,
        };
        // A placeholder key only for validation: the real key is read on send.
        let cfg = ai_config(ai, present.then(|| Secret::new("present")));
        match cfg.validate() {
            Ok(()) => AiStatus::Ready,
            Err(e) if e.kind == ErrorKind::Auth => AiStatus::MissingKey(provider_label(ai.provider).to_string()),
            Err(e) => AiStatus::Invalid(e.full_text()),
        }
    }

    /// Earlier exchanges sent with the next question.
    pub fn history(&self) -> Vec<(ChatMessage, String)> {
        self.turns.iter().filter(|t| t.in_history()).filter_map(|t| t.message.clone().map(|m| (m, t.answer.clone()))).collect()
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Start a new conversation.
    pub fn reset(&mut self) {
        self.stop();
        self.pending = None;
        self.turns.clear();
        self.notice = None;
    }

    pub fn stop(&mut self) {
        if let Some(p) = &self.pending {
            p.cancel.cancel();
        }
    }

    fn snapshot(&self, cx: &AppContext) -> Option<Arc<SchemaSnapshot>> {
        let live = cx.conns.live(self.conn.as_ref()?)?;
        live.meta.snapshot(self.schema.as_ref()?)
    }

    /// Validate the inputs, start the background request and add the turn.
    pub fn send(&mut self, cx: &mut AppContext) {
        self.notice = None;
        if self.pending.is_some() {
            return;
        }
        if self.status(cx) != AiStatus::Ready {
            return;
        }
        let ai = cx.settings.ai.clone();
        let key = match cx.services.secrets.read().ai_api_key(ai.provider.id()) {
            Ok(k) => k,
            Err(e) => {
                self.notice = Some(e.full_text());
                return;
            }
        };
        let client = match AiClient::new(ai_config(&ai, key)) {
            Ok(c) => c,
            Err(e) => {
                self.key_check = None;
                self.notice = Some(e.full_text());
                return;
            }
        };
        let action = self.action;
        let mut input = ActionInput { text: self.input.clone(), error: self.error_input.clone(), ..Default::default() };
        let live = self.conn.as_ref().and_then(|c| cx.conns.live(c));
        let session = live.map(|l| l.main.clone());
        let object = if action == QuickAction::Describe { self.object.clone() } else { None };
        let prep = match (action, &session) {
            (QuickAction::Optimize, Some(s)) if self.include_plan && !input.text.trim().is_empty() => Prep::Plan { session: s.clone(), sql: input.text.trim().to_string() },
            (QuickAction::Describe, Some(s)) if object.is_some() => {
                Prep::Describe { session: s.clone(), dialect: s.dialect(), object: object.clone().expect("checked"), sample: ai.send_sample_data }
            }
            _ => Prep::None,
        };
        // Check the inputs now (the real message is built after the database lookups).
        let mut probe = input.clone();
        if matches!(prep, Prep::Describe { .. }) {
            probe.ddl = Some("-".into());
        }
        if let Err(e) = user_message(action, &probe) {
            self.notice = Some(e.message);
            return;
        }
        let kind = live.map(|l| l.kind()).unwrap_or(DbKind::Generic);
        let version = live.map(|l| l.server.version.clone()).unwrap_or_default();
        let snapshot = if ai.include_schema_context { self.snapshot(cx) } else { None };
        let history = self.history();
        let question = format!("{} {}", input.text, input.error);
        let locale = dbiji_core::i18n::locale();
        let include_schema = ai.include_schema_context;
        let preparing = !matches!(prep, Prep::None);
        let mut turn = Turn::new(action, shown_text(action, &input, object.as_ref()));
        turn.state = if preparing { TurnState::Preparing } else { TurnState::Streaming };
        let (tx, rx) = mpsc::channel();
        let repaint = cx.backend.repainter();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let job = cx.backend.spawn(async move {
            prep.run(&mut input).await?;
            if token.is_cancelled() {
                return Err(DbError::cancelled());
            }
            let message = user_message(action, &input)?;
            let ctx = RequestContext { kind, server_version: &version, snapshot: snapshot.as_deref(), include_schema, locale };
            let req = build_request(&ctx, &history, message.clone(), &question);
            let _ = tx.send(Event::Prepared(message));
            repaint.request();
            let rp = repaint.clone();
            client
                .chat_stream(
                    req,
                    move |d| {
                        let _ = tx.send(Event::Delta(d.to_string()));
                        rp.request();
                    },
                    token,
                )
                .await
        });
        self.turns.push(turn);
        self.pending = Some(Pending { turn: self.turns.len() - 1, job, rx, cancel });
        // Keep the SQL for follow-ups; clear free text.
        if !action.takes_sql() {
            self.input.clear();
        }
    }

    fn poll(&mut self) {
        let Some(p) = &mut self.pending else { return };
        let Some(turn) = self.turns.get_mut(p.turn) else {
            self.pending = None;
            return;
        };
        while let Ok(ev) = p.rx.try_recv() {
            match ev {
                Event::Prepared(m) => {
                    turn.message = Some(m);
                    turn.state = TurnState::Streaming;
                }
                Event::Delta(d) => turn.answer.push_str(&d),
            }
        }
        let Some(result) = p.job.poll() else { return };
        // Deltas sent before the result arrived.
        while let Ok(ev) = p.rx.try_recv() {
            match ev {
                Event::Prepared(m) => turn.message = Some(m),
                Event::Delta(d) => turn.answer.push_str(&d),
            }
        }
        match result {
            Ok(r) => {
                turn.answer = r.text;
                turn.state = TurnState::Done;
                turn.model = Some(r.model).filter(|m| !m.is_empty());
                turn.usage = Some(r.usage);
                turn.truncated = r.truncated;
                turn.fallback_from = r.fallback_from;
            }
            Err(e) if e.code.as_deref() == Some("refusal") => {
                turn.answer.clear();
                turn.state = TurnState::Refused(e.full_text());
            }
            Err(e) if e.kind == ErrorKind::Cancelled => turn.state = TurnState::Stopped,
            Err(e) => {
                if e.kind == ErrorKind::Auth {
                    self.key_check = None;
                }
                turn.state = TurnState::Failed(e.full_text());
            }
        }
        self.pending = None;
    }

    /// The SQL blocks of the answer of turn `turn`.
    pub fn answer_sql(&self, turn: usize) -> Vec<String> {
        self.turns.get(turn).map(|t| sql_blocks(&t.answer)).unwrap_or_default()
    }

    /// Open SQL in a new editor bound to the assistant's connection and schema.
    pub fn open_in_editor(&self, cx: &mut AppContext, sql: &str) {
        let mut text = sql.trim_end().to_string();
        if !text.ends_with(';') {
            text.push(';');
        }
        text.push('\n');
        cx.push(AppCommand::NewSqlEditor { connection: self.conn.clone(), schema: self.schema.clone(), text: Some(text) });
    }

    /// Test helper: add a finished exchange without contacting any service.
    #[doc(hidden)]
    pub fn push_exchange(&mut self, action: QuickAction, question: &str, answer: &str) {
        let mut t = Turn::new(action, question.to_string());
        t.message = Some(ChatMessage::user(question));
        t.answer = answer.to_string();
        t.state = TurnState::Done;
        self.turns.push(t);
    }

    #[doc(hidden)]
    pub fn set_input(&mut self, action: QuickAction, text: &str, error: &str) {
        self.action = action;
        self.input = text.to_string();
        self.error_input = error.to_string();
    }

    #[doc(hidden)]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    // ----------------------------------------------------------------- UI

    fn sync_language(&mut self, cx: &AppContext) {
        let kind = self.kind(cx);
        if self.lang.as_ref().is_some_and(|(k, _)| *k == kind) {
            return;
        }
        let dialect: Arc<dyn Dialect> = match self.conn.as_ref().and_then(|c| cx.conns.live(c)) {
            Some(l) => l.dialect.clone(),
            None => Arc::new(dbiji_core::dialect::GenericDialect::new()),
        };
        self.lang = Some((kind, SqlLanguage::new(dialect)));
    }

    fn header(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.horizontal(|ui| {
            let mut conn = self.conn.clone();
            if crate::tabs::data_compare::connection_combo(ui, cx, ("ai_conn", self.uid), &mut conn, Some(tr("No connection"))) {
                self.conn = conn;
                self.schema = None;
                self.object = None;
                self.lang = None;
            }
            if let Some(c) = self.conn.clone()
                && cx.conns.is_connected(&c)
            {
                let mut schema = self.schema.clone();
                if crate::tabs::data_compare::schema_combo(ui, cx, ("ai_schema", self.uid), &c, &mut schema) {
                    self.schema = schema;
                    self.object = None;
                }
            }
            self.context_status(ui, cx);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(icon::GEAR).on_hover_text(tr("AI settings")).clicked() {
                    cx.push(AppCommand::OpenSettings);
                }
                if ui.add_enabled(!self.turns.is_empty(), egui::Button::new(format!("{} {}", icon::PLUS, tr("New conversation")))).clicked() {
                    self.reset();
                }
                ui.weak(cx.settings.ai.model.clone());
            });
        });
    }

    /// What is sent besides the question.
    fn context_status(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !cx.settings.ai.include_schema_context {
            ui.weak(tr("Schema context: off"));
            return;
        }
        let (Some(conn), Some(schema)) = (self.conn.clone(), self.schema.clone()) else {
            ui.weak(tr("Schema context: none"));
            return;
        };
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&conn) else { return };
        let main = live.main.clone();
        live.meta.request_snapshot(backend, &main, &schema);
        match live.meta.snapshots.get(&schema) {
            Some(l) if l.is_loading() => {
                ui.spinner();
                ui.weak(tr("Reading the schema…"));
            }
            Some(l) if l.error().is_some() => {
                let e = l.error().map(|e| e.full_text()).unwrap_or_default();
                ui.label(RichText::new(format!("{} {}", icon::WARNING, tr("No schema context"))).color(crate::theme::warning_color(ui.visuals().dark_mode))).on_hover_text(e);
            }
            Some(l) => {
                if let Some(s) = l.get() {
                    ui.weak(trf("Schema context: {} tables, {} views", &[&s.tables.len(), &s.views.len()]))
                        .on_hover_text(tr("Table and column names of this schema are sent with each question. No data rows are sent."));
                }
            }
            None => {}
        }
        if cx.settings.ai.send_sample_data {
            ui.weak(tr("· sample rows on"));
        }
    }

    fn setup_ui(ui: &mut Ui, cx: &mut AppContext, status: &AiStatus) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading(format!("{} {}", icon::SPARKLE, tr("AI assistant")));
            ui.add_space(8.0);
            match status {
                AiStatus::Disabled => {
                    ui.label(tr("The AI assistant is turned off."));
                    ui.weak(tr("Turn it on in the settings and enter the API key of your provider. Only the schema structure and what you type are sent, and only to the provider you choose."));
                }
                AiStatus::MissingKey(p) => {
                    ui.label(trf("No API key is saved for {}.", &[p]));
                    ui.weak(tr("Enter the key in the AI section of the settings. It is kept in the system keychain or the encrypted vault."));
                }
                AiStatus::Invalid(e) => {
                    ui.label(tr("The AI settings are incomplete."));
                    crate::widgets::error_box(ui, e);
                }
                AiStatus::Ready => {}
            }
            ui.add_space(8.0);
            if ui.button(format!("{} {}", icon::GEAR, tr("Open settings"))).clicked() {
                cx.push(AppCommand::OpenSettings);
            }
        });
    }

    fn code_block(&self, ui: &mut Ui, cx: &mut AppContext, id: (u64, usize, usize, usize), lang_tag: &str, code: &str, sql: bool) {
        let theme = EditorTheme::from_visuals(ui.visuals());
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        egui::Frame::group(ui.style()).fill(theme.background).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.weak(if sql {
                    "SQL".to_string()
                } else if lang_tag.is_empty() {
                    tr("Code").to_string()
                } else {
                    lang_tag.to_string()
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if sql && ui.small_button(format!("{} {}", icon::FILE_SQL, tr("Open in SQL editor"))).clicked() {
                        self.open_in_editor(cx, code);
                    }
                    if ui.small_button(format!("{} {}", icon::COPY, tr("Copy"))).clicked() {
                        ui.ctx().copy_text(code.to_string());
                    }
                });
            });
            egui::ScrollArea::horizontal().id_salt(("ai_code", id)).auto_shrink([false, true]).show(ui, |ui| {
                let lang = if sql { self.lang.as_ref().map(|(_, l)| l) } else { None };
                ui.add(egui::Label::new(highlight_job(code, lang, &theme, font.clone())).selectable(true).extend());
            });
        });
    }

    fn segments_ui(&self, ui: &mut Ui, cx: &mut AppContext, segments: &[AnswerSegment], id: (u64, usize, usize)) {
        for (i, s) in segments.iter().enumerate() {
            match s {
                AnswerSegment::Text(t) => {
                    ui.add(egui::Label::new(t.as_str()).wrap().selectable(true));
                }
                AnswerSegment::Code { lang, code, sql } => self.code_block(ui, cx, (id.0, id.1, id.2, i), lang, code, *sql),
            }
        }
    }

    fn conversation_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if self.turns.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.weak(tr("Ask a question or choose a quick action below. SQL in answers can be copied or opened in a SQL editor."));
            });
            return;
        }
        let dark = ui.visuals().dark_mode;
        egui::ScrollArea::vertical().id_salt(("ai_chat", self.uid)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for ti in 0..self.turns.len() {
                let (action, shown, state, model, usage, truncated, fallback, answer_empty) = {
                    let t = &self.turns[ti];
                    (t.action, t.shown.clone(), t.state.clone(), t.model.clone(), t.usage, t.truncated, t.fallback_from.clone(), t.answer.trim().is_empty())
                };
                // question
                egui::Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} {}", icon::USER, tr("You"))).strong());
                        if action != QuickAction::Chat {
                            ui.weak(format!("{} {}", action.icon(), action.label()));
                        }
                    });
                    let segs = answer_segments(&shown);
                    self.segments_ui(ui, cx, &segs, (self.uid, ti, 0));
                });
                ui.add_space(4.0);
                // answer
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} {}", icon::SPARKLE, tr("Assistant"))).strong());
                    if let Some(m) = &model {
                        ui.weak(m);
                    }
                });
                match &state {
                    TurnState::Preparing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(tr("Reading from the database…"));
                        });
                    }
                    TurnState::Streaming if answer_empty => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(tr("Thinking…"));
                        });
                    }
                    _ => {}
                }
                let segs = self.turns[ti].segments().to_vec();
                self.segments_ui(ui, cx, &segs, (self.uid, ti, 1));
                match &state {
                    TurnState::Stopped => {
                        ui.weak(format!("{} {}", icon::STOP, tr("Stopped.")));
                    }
                    TurnState::Failed(e) => crate::widgets::error_box(ui, e),
                    TurnState::Refused(e) => {
                        ui.label(RichText::new(format!("{} {}", icon::WARNING, tr("The model declined to answer this request."))).color(crate::theme::warning_color(dark)));
                        ui.weak(e);
                    }
                    TurnState::Done => {
                        if truncated {
                            ui.label(RichText::new(format!("{} {}", icon::WARNING, tr("The answer was cut off at the token limit."))).color(crate::theme::warning_color(dark)));
                        }
                        if let (Some(from), Some(m)) = (&fallback, &model) {
                            ui.weak(trf("Answered by {} because {} declined.", &[m, from]));
                        }
                        if let Some(u) = usage {
                            ui.weak(trf("{} input / {} output tokens", &[&(u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens), &u.output_tokens]));
                        }
                    }
                    _ => {}
                }
                ui.add_space(10.0);
            }
        });
    }

    fn composer_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for a in QuickAction::ALL {
                ui.selectable_value(&mut self.action, a, format!("{} {}", a.icon(), a.label()));
            }
        });
        let connected = self.conn.as_ref().is_some_and(|c| cx.conns.is_connected(c));
        match self.action {
            QuickAction::Describe if connected => self.object_picker(ui, cx),
            QuickAction::Optimize if connected => {
                ui.checkbox(&mut self.include_plan, tr("Include the execution plan (EXPLAIN runs on the connection, the query is not executed)"));
            }
            _ => {}
        }
        let busy = self.pending.is_some();
        let mut send = false;
        let rows = if self.action.takes_sql() || self.action == QuickAction::Describe { 4 } else { 2 };
        let mut edit = egui::TextEdit::multiline(&mut self.input).hint_text(self.action.hint()).desired_rows(rows).desired_width(f32::INFINITY).id_salt(("ai_input", self.uid));
        if self.action.takes_sql() || self.action == QuickAction::Describe {
            edit = edit.code_editor();
        }
        let resp = ui.add(edit);
        if self.focus_input {
            self.focus_input = false;
            resp.request_focus();
        }
        if resp.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
            send = true;
        }
        if self.action == QuickAction::FixError {
            let r = ui.add(
                egui::TextEdit::multiline(&mut self.error_input)
                    .hint_text(tr("Error message from the database"))
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .id_salt(("ai_error", self.uid)),
            );
            if r.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
                send = true;
            }
        }
        ui.horizontal(|ui| {
            if busy {
                if ui.button(RichText::new(format!("{} {}", icon::STOP, tr("Stop"))).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    self.stop();
                }
                ui.spinner();
            } else if ui.button(RichText::new(format!("{} {}", icon::PAPER_PLANE_RIGHT, tr("Send"))).strong()).clicked() {
                send = true;
            }
            ui.weak(trf("{} to send", &[&if crate::keys::IS_MAC { "⌘↩" } else { "Ctrl+Enter" }]));
            if let Some(n) = &self.notice {
                ui.label(RichText::new(n).color(crate::theme::warning_color(ui.visuals().dark_mode)));
            }
        });
        if send && !busy {
            self.send(cx);
        }
    }

    /// Send the question of [`AiAssistantTab::ask_about`] once the connection
    /// and its schema context are available (or after a few seconds without).
    fn maybe_auto_send(&mut self, cx: &mut AppContext) {
        if !self.auto_send {
            return;
        }
        let since = *self.auto_since.get_or_insert_with(Instant::now);
        let ready = match self.conn.clone() {
            None => true,
            Some(c) if !cx.conns.is_connected(&c) => {
                if !cx.conns.is_connecting() && !self.connect_requested {
                    self.connect_requested = true;
                    cx.push(AppCommand::Connect(c));
                }
                false
            }
            Some(c) => {
                !cx.settings.ai.include_schema_context
                    || match (&self.schema, cx.conns.live(&c)) {
                        (Some(s), Some(l)) => l.meta.snapshots.get(s).is_some_and(|x| !x.is_loading()),
                        _ => false,
                    }
            }
        };
        if ready || since.elapsed() > Duration::from_secs(10) {
            self.auto_send = false;
            self.send(cx);
        }
    }

    fn object_picker(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let Some(snap) = self.snapshot(cx) else {
            ui.weak(tr("Choose a schema to pick an object, or paste its definition below."));
            return;
        };
        ui.horizontal(|ui| {
            ui.label(tr("Object:"));
            let label = self.object.as_ref().map(|o| o.name.clone()).unwrap_or_else(|| tr("(paste a definition)").to_string());
            egui::ComboBox::from_id_salt(("ai_obj", self.uid)).selected_text(label).width(240.0).show_ui(ui, |ui| {
                if ui.selectable_label(self.object.is_none(), tr("(paste a definition)")).clicked() {
                    self.object = None;
                }
                let db = self.schema.as_ref().and_then(|s| s.database.clone());
                let items = snap.tables.iter().map(|t| ObjectRef::table(snap.schema.clone(), t.name.clone())).chain(
                    snap.views.iter().map(|v| ObjectRef::new(if v.materialized { ObjectKind::MaterializedView } else { ObjectKind::View }, snap.schema.clone(), v.name.clone())),
                );
                for o in items {
                    let o = o.with_database(db.clone());
                    let on = self.object.as_ref() == Some(&o);
                    if ui.selectable_label(on, format!("{} {}", crate::nav::kind_icon(o.kind), o.name)).clicked() {
                        self.object = Some(o);
                    }
                }
            });
            if cx.settings.ai.send_sample_data && self.object.as_ref().is_some_and(|o| o.kind.has_data()) {
                ui.weak(trf("The first {} rows are sent as a sample.", &[&SAMPLE_ROWS]));
            }
        });
    }
}

impl TabView for AiAssistantTab {
    fn title(&self) -> String {
        tr("AI assistant").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::SPARKLE
    }

    fn connection_id(&self) -> Option<&str> {
        self.conn.as_deref()
    }

    fn dedupe_key(&self) -> Option<String> {
        self.dedupe.then(|| "ai-assistant".to_string())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll();
        self.sync_language(cx);
        let status = self.status(cx);
        if status != AiStatus::Ready {
            self.header(ui, cx);
            ui.separator();
            Self::setup_ui(ui, cx, &status);
            return;
        }
        self.header(ui, cx);
        ui.separator();
        self.maybe_auto_send(cx);
        egui::Panel::bottom(egui::Id::new(("ai_composer", self.uid))).resizable(true).default_size(170.0).size_range(110.0..=600.0).show(ui, |ui| self.composer_ui(ui, cx));
        egui::CentralPanel::default().show(ui, |ui| self.conversation_ui(ui, cx));
        if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_close(&mut self, _cx: &mut AppContext) -> bool {
        self.stop();
        true
    }

    crate::impl_tab_any!();
}
