//! Agent mode of the AI assistant: the model may run read-only queries on
//! the connection (the `run_query` tool) and answer from their results.
//!
//! A query runs only when every guard lets it through:
//! * one statement that reads (`SELECT`, `WITH … SELECT`, `VALUES`, `SHOW`,
//!   `DESCRIBE`, `EXPLAIN` without `ANALYZE`), as the SQL editor classifies it;
//! * none of the functions or clauses that act beyond reading (ending
//!   sessions, reading server files, writing files, sleeping);
//! * a session of its own, read-only where the database has the mode
//!   (PostgreSQL / CockroachDB, MySQL / MariaDB, SQLite);
//! * at most [`MAX_ROWS`] rows, within [`TIMEOUT`].

use crate::conn::session::SessionHandle;
use dbiji_ai::{ChatMessage, ChatRequest, ChatResponse, ToolCall, ToolDef, Usage};
use dbiji_core::{DbError, DbKind, DbResult, ExecOptions, ResultHeader, SqlSyntax, tr, trf};
use dbiji_sql::StatementKind;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The tool's name.
pub const TOOL: &str = "run_query";
/// Rows of a result the model gets.
pub const MAX_ROWS: usize = 100;
/// Time a query may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Requests of one answer (each may run queries).
pub const MAX_ROUNDS: usize = 8;
/// Characters of one value the model gets.
const MAX_VALUE_CHARS: usize = 200;

/// Calls and clauses that do more than read, refused even in a query.
const DENIED: &[&str] = &[
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_sleep",
    "pg_advisory",
    "lo_import",
    "lo_export",
    "dblink",
    "set_config",
    "load_file",
    "sleep(",
    "benchmark(",
    "get_lock",
    "openrowset",
    "opendatasource",
    "openquery",
    "xp_",
    "into outfile",
    "into dumpfile",
];

/// The `run_query` tool.
pub fn tool() -> ToolDef {
    ToolDef {
        name: TOOL.into(),
        description: format!(
            "Run one read-only SQL statement (SELECT, WITH … SELECT, SHOW, DESCRIBE or EXPLAIN without ANALYZE) on the user's database \
             connection and get its result as tab separated text: at most {MAX_ROWS} rows, values cut at {MAX_VALUE_CHARS} characters. \
             Statements that change data or schema are refused."
        ),
        parameters: r#"{"type":"object","properties":{"sql":{"type":"string","description":"The SQL statement, in the database's dialect"}},"required":["sql"]}"#.into(),
    }
}

/// What the system prompt says about the tool.
pub fn system_note() -> String {
    format!(
        "\n\n## Running queries\nYou can run read-only SQL on the user's connection with the `{TOOL}` tool. Use it whenever the question is about \
         the data (counts, values, examples, checks) instead of guessing, then answer from the results and show the query you ran. One \
         statement per call; at most {MAX_ROWS} rows come back, so aggregate in SQL. Never try to change data or schema with it: give such \
         statements to the user to run."
    )
}

/// `Ok(statement)` when `sql` may run, else why not.
pub fn check(sql: &str, syntax: &SqlSyntax) -> Result<String, String> {
    let stmts = dbiji_sql::split_and_classify(sql, syntax, &Default::default());
    let [(span, info)] = stmts.as_slice() else {
        return Err(tr("Exactly one statement per query").to_string());
    };
    let stmt = sql[span.start..span.end].trim().to_string();
    let reads = matches!(info.kind, StatementKind::Select | StatementKind::Show | StatementKind::Describe | StatementKind::Explain);
    let lower = stmt.to_lowercase();
    let analyze = info.kind == StatementKind::Explain && lower.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "analyze" || w == "analyse");
    if !reads || info.is_write || analyze {
        return Err(trf("Only queries can run here ({} is not one); show the statement to the user instead", &[&info.kind.label()]));
    }
    if lower.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "into") {
        return Err(tr("SELECT … INTO writes; only queries can run here").to_string());
    }
    if let Some(d) = DENIED.iter().find(|d| lower.contains(*d)) {
        return Err(trf("{} does more than read; it cannot run here", &[d]));
    }
    Ok(stmt)
}

/// Opens the agent's session (awaited at its first query).
pub type Opener = Pin<Box<dyn Future<Output = DbResult<SessionHandle>> + Send>>;

/// What the agent needs to run queries during one answer.
pub struct Agent {
    pub kind: DbKind,
    pub syntax: SqlSyntax,
    /// The schema the assistant is set to (made the session's current one).
    pub schema: Option<String>,
    opener: Option<Opener>,
    session: Option<SessionHandle>,
}

impl Agent {
    pub fn new(kind: DbKind, syntax: SqlSyntax, schema: Option<String>, opener: Opener) -> Agent {
        Agent { kind, syntax, schema, opener: Some(opener), session: None }
    }

    /// The session, opened at the first query and made read-only where the
    /// database can.
    pub async fn session(&mut self) -> DbResult<SessionHandle> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let opener = self.opener.take().ok_or_else(|| DbError::connection(tr("The assistant's session could not be opened")))?;
        let s = opener.await?;
        // Each statement its own transaction, so that the read-only default applies to it.
        s.lock().await.set_transaction_mode(dbiji_core::TransactionMode::Auto).await?;
        let read_only = match self.kind {
            DbKind::Postgres | DbKind::CockroachDb => Some("SET default_transaction_read_only = on"),
            DbKind::MySql | DbKind::MariaDb => Some("SET SESSION TRANSACTION READ ONLY"),
            DbKind::Sqlite => Some("PRAGMA query_only = ON"),
            _ => None,
        };
        if let Some(q) = read_only {
            s.execute_update(q, &[]).await?;
        }
        if let Some(schema) = &self.schema {
            let _ = s.lock().await.set_current_schema(schema).await;
        }
        self.session = Some(s.clone());
        Ok(s)
    }

    /// Run the tool call `call`.
    pub async fn answer(&mut self, call: &ToolCall) -> ToolStep {
        let sql = call_sql(call);
        if call.name != TOOL {
            return ToolStep::refused(sql, trf("There is no tool {}", &[&call.name]));
        }
        let stmt = match check(&sql, &self.syntax) {
            Ok(s) => s,
            Err(why) => return ToolStep::refused(sql, why),
        };
        let session = match self.session().await {
            Ok(s) => s,
            Err(e) => return ToolStep::failed(stmt, e.full_text()),
        };
        match run(&session, &stmt).await {
            Ok((cols, rows, more)) => ToolStep::rows(stmt, cols, rows, more),
            Err(e) => ToolStep::failed(stmt, e.full_text()),
        }
    }
}

/// Run `stmt`: the column names, at most [`MAX_ROWS`] rows as text, and
/// whether there were more.
async fn run(session: &SessionHandle, stmt: &str) -> DbResult<(Vec<String>, Vec<Vec<String>>, bool)> {
    let ex = session.execute(stmt, &[], &ExecOptions { timeout: Some(TIMEOUT), ..Default::default() }).await?;
    let Some(ResultHeader::Rows(cols)) = ex.header else { return Ok((Vec::new(), Vec::new(), false)) };
    let out = session.fetch(ex.token, MAX_ROWS + 1).await;
    session.close_cursor_token(ex.token).await;
    let out = out?;
    let more = out.rows.len() > MAX_ROWS;
    let rows = out
        .rows
        .into_iter()
        .take(MAX_ROWS)
        .map(|r| {
            r.iter()
                .map(|v| {
                    let t = if v.is_null() { "NULL".to_string() } else { v.to_text().replace(['\t', '\n', '\r'], " ") };
                    if t.chars().count() > MAX_VALUE_CHARS { t.chars().take(MAX_VALUE_CHARS).collect::<String>() + "…" } else { t }
                })
                .collect()
        })
        .collect();
    Ok((cols.iter().map(|c| c.name.clone()).collect(), rows, more))
}

/// One query the assistant ran (or wanted to): shown in the chat, its
/// `for_model` text sent back as the tool's result.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolStep {
    pub sql: String,
    /// Ran and returned this.
    pub ok: bool,
    /// "42 rows", or why it did not run.
    pub summary: String,
    /// The result as the model gets it.
    pub for_model: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl ToolStep {
    fn refused(sql: String, why: String) -> ToolStep {
        ToolStep { sql, ok: false, summary: why.clone(), for_model: format!("Not run: {why}"), columns: Vec::new(), rows: Vec::new() }
    }

    fn failed(sql: String, error: String) -> ToolStep {
        ToolStep { sql, ok: false, summary: error.clone(), for_model: format!("Error: {error}"), columns: Vec::new(), rows: Vec::new() }
    }

    fn rows(sql: String, columns: Vec<String>, rows: Vec<Vec<String>>, more: bool) -> ToolStep {
        let mut text = columns.join("\t");
        for r in &rows {
            text.push('\n');
            text.push_str(&r.join("\t"));
        }
        let summary = if more { trf("first {} rows", &[&rows.len()]) } else { trf("{} row(s)", &[&rows.len()]) };
        text.push_str(&if more { format!("\n({} rows shown; there are more)", rows.len()) } else { format!("\n({} rows)", rows.len()) });
        ToolStep { sql, ok: true, summary, for_model: text, columns, rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg() -> SqlSyntax {
        crate::driver_registry().dialect(DbKind::Postgres).syntax().clone()
    }

    #[test]
    fn only_queries_run() {
        let s = pg();
        assert_eq!(check("SELECT count(*) FROM charges;", &s).unwrap(), "SELECT count(*) FROM charges");
        assert!(check("WITH x AS (SELECT 1) SELECT * FROM x", &s).is_ok());
        assert!(check("EXPLAIN SELECT 1", &s).is_ok());
        assert!(check("SHOW search_path", &s).is_ok());
        for refused in [
            "DELETE FROM charges",
            "UPDATE charges SET paid = true",
            "DROP TABLE charges",
            "SELECT 1; DELETE FROM charges",
            "EXPLAIN ANALYZE DELETE FROM charges",
            "EXPLAIN (ANALYZE) SELECT 1",
            "SELECT * INTO copy FROM charges",
            "SELECT pg_terminate_backend(123)",
            "SELECT pg_read_file('/etc/passwd')",
            "COPY charges TO '/tmp/x'",
            "CALL cleanup()",
            "",
        ] {
            assert!(check(refused, &s).is_err(), "{refused}");
        }
    }

    async fn sqlite(path: std::path::PathBuf) -> DbResult<SessionHandle> {
        use dbiji_core::Driver;
        let mut cfg = dbiji_core::ConnectionConfig::new(DbKind::Sqlite, "t");
        cfg.file_path = Some(path.display().to_string());
        let s = dbiji_driver_sqlite::SqliteDriver::new().connect(&dbiji_core::ConnectParams::from_config(&cfg, None)).await?;
        Ok(SessionHandle::new(s, "t", None))
    }

    fn reply(text: &str, calls: &[&str]) -> ChatResponse {
        let tool_calls =
            calls.iter().enumerate().map(|(i, sql)| ToolCall { id: format!("c{i}"), name: TOOL.into(), arguments: serde_json::json!({ "sql": sql }).to_string() }).collect();
        ChatResponse { text: text.into(), tool_calls, usage: Usage { input_tokens: 10, output_tokens: 1, ..Default::default() }, ..Default::default() }
    }

    #[tokio::test]
    async fn the_model_queries_a_read_only_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let setup = sqlite(path.clone()).await.unwrap();
        setup.execute_update("CREATE TABLE charges (id INTEGER, paid INTEGER)", &[]).await.unwrap();
        setup.execute_update("INSERT INTO charges VALUES (1, 1), (2, 0), (3, 1)", &[]).await.unwrap();
        let syntax = crate::driver_registry().dialect(DbKind::Sqlite).syntax().clone();
        let mut agent = Agent::new(DbKind::Sqlite, syntax, None, Box::pin(sqlite(path)));
        // The model tries a write, then counts; the third request answers.
        let mut replies =
            vec![reply("", &["DELETE FROM charges"]), reply("Counting.", &["SELECT count(*) AS n FROM charges WHERE paid = 1"]), reply("2 are paid.", &[])].into_iter();
        let mut sent = Vec::new();
        let mut events = Vec::new();
        let r = converse(
            ChatRequest::new("system", vec![ChatMessage::user("How many are paid?")]),
            &mut agent,
            &CancellationToken::new(),
            |req| {
                sent.push(req);
                let r = replies.next().unwrap();
                Box::pin(async move { Ok(r) })
            },
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(r.text, "Counting.\n\n2 are paid.");
        assert_eq!(r.usage.input_tokens, 30);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].tools[0].name, TOOL);
        assert!(sent[0].system.contains(TOOL));
        // The refusal and the result went back to the model.
        let last = &sent[2].messages;
        assert!(last[2].content.starts_with("Not run:"), "{:?}", last[2]);
        assert_eq!(last[4].content, "n\n2\n(1 rows)");
        let steps: Vec<_> = events.iter().filter_map(|e| if let AgentEvent::Step(s) = e { Some(s) } else { None }).collect();
        assert_eq!((steps[0].ok, steps[1].ok), (false, true));
        assert_eq!(steps[1].rows, vec![vec!["2".to_string()]]);
        // Its session cannot write even what the check would let through.
        let session = agent.session().await.unwrap();
        assert!(session.execute_update("INSERT INTO charges VALUES (4, 0)", &[]).await.is_err());
    }

    #[test]
    fn results_for_the_model() {
        let step = ToolStep::rows("SELECT 1".into(), vec!["n".into()], vec![vec!["1".into()], vec!["2".into()]], false);
        assert_eq!(step.for_model, "n\n1\n2\n(2 rows)");
        assert!(step.ok);
        let more = ToolStep::rows("SELECT 1".into(), vec!["n".into()], vec![vec!["1".into()]], true);
        assert!(more.for_model.ends_with("(1 rows shown; there are more)"));
    }
}

/// Answer `req`, running the queries the model asks for, in at most
/// [`MAX_ROUNDS`] requests. The text of every request goes to `on_text`
/// (with a blank line between requests), each query to `on_step`.
pub async fn converse<'a, S, F>(mut req: ChatRequest, agent: &mut Agent, cancel: &CancellationToken, mut stream: S, mut on_event: F) -> DbResult<ChatResponse>
where
    S: FnMut(ChatRequest) -> Pin<Box<dyn Future<Output = DbResult<ChatResponse>> + Send + 'a>>,
    F: FnMut(AgentEvent),
{
    req.tools = vec![tool()];
    req.system.push_str(&system_note());
    let mut text = String::new();
    let mut usage = Usage::default();
    for round in 1..=MAX_ROUNDS {
        let mut r = stream(req.clone()).await?;
        usage.input_tokens += r.usage.input_tokens;
        usage.output_tokens += r.usage.output_tokens;
        usage.cache_read_input_tokens += r.usage.cache_read_input_tokens;
        usage.cache_creation_input_tokens += r.usage.cache_creation_input_tokens;
        text.push_str(&r.text);
        if r.tool_calls.is_empty() || round == MAX_ROUNDS {
            if !r.tool_calls.is_empty() {
                let note = format!("\n\n{}", trf("(Stopped after {} rounds of queries.)", &[&MAX_ROUNDS]));
                on_event(AgentEvent::Text(note.clone()));
                text.push_str(&note);
            }
            r.text = text;
            r.usage = usage;
            r.tool_calls.clear();
            return Ok(r);
        }
        if !text.trim().is_empty() && !text.ends_with("\n\n") {
            let sep = if text.ends_with('\n') { "\n" } else { "\n\n" };
            on_event(AgentEvent::Text(sep.to_string()));
            text.push_str(sep);
        }
        req.messages.push(ChatMessage::assistant_calls(r.text, r.tool_calls.clone(), Some(r.reasoning)));
        for call in &r.tool_calls {
            on_event(AgentEvent::Running(call_sql(call)));
            let step = tokio::select! {
                s = agent.answer(call) => s,
                _ = cancel.cancelled() => return Err(DbError::cancelled()),
            };
            req.messages.push(ChatMessage::tool_result(call.id.clone(), step.for_model.clone()));
            on_event(AgentEvent::Step(step));
        }
    }
    unreachable!("the last round returns")
}

/// Progress of [`converse`] besides the streamed text.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Text added between requests.
    Text(String),
    /// A query started.
    Running(String),
    /// A query finished (or was refused).
    Step(ToolStep),
}

fn call_sql(call: &ToolCall) -> String {
    serde_json::from_str::<serde_json::Value>(&call.arguments).ok().and_then(|v| v["sql"].as_str().map(str::to_string)).unwrap_or_default()
}
