//! ClickHouse [`Session`] over the HTTP interface.

use crate::catalog;
use crate::cursor::{self, ActiveQuery, Job};
use crate::dialect::{ClickHouseDialect, ident};
use crate::http::{QueryRequest, Transport, sql_string, tls_plan};
use crate::plan;
use async_trait::async_trait;
use dbiji_core::admin::*;
use dbiji_core::config::SslMode;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::{CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor, ResultHeader, Row, Session, TransactionMode, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Settings sent with every query unless the user is `readonly = 1`.
fn default_settings(params: &ConnectParams) -> Vec<(String, String)> {
    let mut s = vec![
        ("output_format_json_quote_denormals".to_string(), "1".to_string()),
        ("date_time_output_format".to_string(), "simple".to_string()),
        // Allow a client to pause reading (grid paging) without the server
        // aborting the stream.
        ("http_send_timeout".to_string(), "3600".to_string()),
    ];
    if params.read_only {
        s.push(("readonly".into(), "2".into()));
    }
    if params.property("compression").is_some_and(|v| v == "true" || v == "1") {
        s.push(("enable_http_compression".into(), "1".into()));
    }
    for (k, v) in &params.properties {
        if let Some(name) = k.strip_prefix("setting.")
            && !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            s.push((name.to_string(), v.clone()));
        }
    }
    s
}

/// Cancels the statement running on a session with `KILL QUERY`.
pub struct ClickHouseCancelHandle {
    transport: Arc<Transport>,
    active: Arc<ActiveQuery>,
}

#[async_trait]
impl CancelHandle for ClickHouseCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        let Some((query_id, state)) = self.active.get() else { return Ok(()) };
        state.cancelled.store(true, Ordering::SeqCst);
        let transport = self.transport.clone();
        tokio::task::spawn_blocking(move || transport.kill_query(&query_id, false)).await.map_err(|e| DbError::internal(format!("cancel task failed: {e}")))?
    }
}

/// One ClickHouse HTTP session.
pub struct ClickHouseSession {
    pub(crate) transport: Arc<Transport>,
    dialect: Arc<ClickHouseDialect>,
    info: ServerInfo,
    database: Arc<Mutex<Option<String>>>,
    active: Arc<ActiveQuery>,
    gate: Arc<Semaphore>,
    /// Id of the open cursor (0: none).
    cursor_open: Arc<AtomicU64>,
    cancel: Arc<ClickHouseCancelHandle>,
    closed: bool,
    /// `readonly = 1` user: no settings may be sent.
    pub(crate) minimal: bool,
    /// Server time zone (`X-ClickHouse-Timezone`).
    pub(crate) timezone: Option<String>,
}

/// First keyword of a statement (upper case), skipping comments and `(`.
pub fn first_keyword(sql: &str) -> String {
    let s = skip_leading_trivia(sql);
    s.chars().take_while(|c| c.is_ascii_alphabetic() || *c == '_').collect::<String>().to_ascii_uppercase()
}

/// `sql` without its leading comments, white space and `(`.
fn skip_leading_trivia(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(r) = s.strip_prefix("--").or_else(|| s.strip_prefix('#')) {
            s = r.find('\n').map(|i| &r[i + 1..]).unwrap_or("").trim_start();
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.find("*/").map(|i| &r[i + 2..]).unwrap_or("").trim_start();
        } else if let Some(r) = s.strip_prefix('(') {
            s = r.trim_start();
        } else {
            break;
        }
    }
    s
}

/// Database named by a `USE db` statement (the name right after the keyword,
/// not a `use` inside a comment or a word such as REUSE).
pub fn use_target(sql: &str) -> Option<String> {
    let s = skip_leading_trivia(sql);
    if first_keyword(s) != "USE" {
        return None;
    }
    let rest = s[3..].trim_start();
    let mut chars = rest.chars();
    match chars.next()? {
        q @ ('`' | '"') => {
            // Quoted: up to the closing quote (doubled or backslash escaped inside).
            let mut name = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    name.push(chars.next()?);
                } else if c == q {
                    if chars.as_str().starts_with(q) {
                        chars.next();
                        name.push(q);
                    } else {
                        return Some(name);
                    }
                } else {
                    name.push(c);
                }
            }
            None
        }
        _ => {
            let name: String = rest.chars().take_while(|c| !c.is_whitespace() && *c != ';').collect();
            (!name.is_empty() && !name.starts_with("--") && !name.starts_with("/*")).then_some(name)
        }
    }
}

impl ClickHouseSession {
    /// Connect and probe the server.
    pub async fn connect(params: &ConnectParams) -> DbResult<ClickHouseSession> {
        let params = params.clone();
        tokio::task::spawn_blocking(move || Self::connect_blocking(&params)).await.map_err(|e| DbError::internal(format!("connect task failed: {e}")))?
    }

    fn connect_blocking(params: &ConnectParams) -> DbResult<ClickHouseSession> {
        let (https, verify) = tls_plan(params.ssl.mode);
        let session_id = uuid::Uuid::new_v4().to_string();
        let attempt = |https: bool| -> DbResult<(Transport, ProbeResult)> {
            let transport = Transport::new(params, https, verify, session_id.clone())?;
            let probe = probe(&transport, params)?;
            Ok((transport, probe))
        };
        let (transport, probe) = match attempt(https) {
            Ok(r) => r,
            // `prefer`: fall back to plain HTTP when the port does not speak TLS.
            Err(e) if params.ssl.mode == SslMode::Prefer && matches!(e.kind, ErrorKind::Tls | ErrorKind::Connection) => {
                tracing::debug!("ClickHouse HTTPS attempt failed ({e}); retrying with HTTP");
                attempt(false)?
            }
            Err(e) => return Err(e),
        };
        let info = ServerInfo {
            product: "ClickHouse".into(),
            version_number: parse_version_number(&probe.version),
            version: probe.version.clone(),
            session_id: Some(transport.session_id.clone()),
        };
        // The `time_zone` property: DateTime values in that zone (session_timezone
        // came with 23.6; older servers refuse the setting, so it is not sent).
        let mut timezone = probe.timezone;
        if let Some(tz) = params.property("time_zone").filter(|z| !z.is_empty())
            && !probe.minimal
            && (info.version_number.0, info.version_number.1) >= (23, 6)
        {
            let mut settings = transport.settings();
            settings.push(("session_timezone".into(), tz.to_string()));
            transport.set_settings(settings);
            timezone = Some(tz.to_string());
        }
        let transport = Arc::new(transport);
        let active = Arc::new(ActiveQuery::default());
        let cancel = Arc::new(ClickHouseCancelHandle { transport: transport.clone(), active: active.clone() });
        Ok(ClickHouseSession {
            transport,
            dialect: Arc::new(ClickHouseDialect::new()),
            info,
            database: Arc::new(Mutex::new(Some(probe.database))),
            active,
            gate: Arc::new(Semaphore::new(1)),
            cursor_open: Arc::new(AtomicU64::new(0)),
            cancel,
            closed: false,
            minimal: probe.minimal,
            timezone,
        })
    }

    /// The user may not change settings (`readonly = 1`): the driver sends
    /// no per-query settings in this mode.
    pub fn is_minimal_mode(&self) -> bool {
        self.minimal
    }

    /// Time zone of the server session (used for `DateTime` columns without
    /// an explicit zone).
    pub fn server_timezone(&self) -> Option<&str> {
        self.timezone.as_deref()
    }

    fn current_db(&self) -> Option<String> {
        self.database.lock().ok().and_then(|g| g.clone())
    }

    /// Start a statement and return its cursor.
    async fn start(&mut self, sql: String, timeout: Option<Duration>, use_session: bool) -> DbResult<Box<dyn QueryCursor>> {
        if self.closed {
            return Err(DbError::connection("The session is closed"));
        }
        if self.cursor_open.load(Ordering::SeqCst) != 0 {
            return Err(DbError::internal("A result set is still open on this session; close it before running another statement"));
        }
        let permit = tokio::time::timeout(Duration::from_secs(60), self.gate.clone().acquire_owned())
            .await
            .map_err(|_| DbError::timeout("The previous statement on this session did not finish"))?
            .map_err(|_| DbError::internal("session gate closed"))?;
        let keyword = first_keyword(&sql);
        let use_database = use_target(&sql);
        let request =
            QueryRequest { sql, query_id: uuid::Uuid::new_v4().to_string(), database: self.current_db(), use_session, extra_settings: Vec::new(), skip_default_settings: false };
        let job =
            Job { transport: self.transport.clone(), request, keyword, use_database, database: self.database.clone(), active: self.active.clone(), timeout, permit: Some(permit) };
        let c = cursor::start(job, self.cursor_open.clone())?;
        Ok(Box::new(c))
    }

    /// Run an internal query and collect all rows of its first result.
    pub(crate) async fn rows(&mut self, sql: &str) -> DbResult<Vec<Row>> {
        let mut c = self.start(sql.to_string(), None, true).await?;
        let mut out = Vec::new();
        let res = async {
            while let Some(h) = c.next_result().await? {
                if let ResultHeader::Rows(_) = h {
                    loop {
                        let batch = c.fetch(1024).await?;
                        if batch.is_empty() {
                            break;
                        }
                        out.extend(batch);
                    }
                    break;
                }
            }
            Ok::<(), DbError>(())
        }
        .await;
        c.close().await?;
        res.map(|_| out)
    }

    /// Run an internal statement, ignoring any result.
    pub(crate) async fn run(&mut self, sql: &str) -> DbResult<()> {
        let mut c = self.start(sql.to_string(), None, true).await?;
        let res = async {
            while let Some(h) = c.next_result().await? {
                if let ResultHeader::Rows(_) = h {
                    while !c.fetch(1024).await?.is_empty() {}
                }
            }
            Ok::<(), DbError>(())
        }
        .await;
        c.close().await?;
        res
    }

    /// First column of the first row.
    pub(crate) async fn scalar(&mut self, sql: &str) -> DbResult<Value> {
        Ok(self.rows(sql).await?.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
    }

    /// The tracked current database, falling back to `default`.
    pub(crate) fn db_or_default(&self) -> String {
        self.current_db().unwrap_or_else(|| "default".to_string())
    }

    async fn explain_impl(&mut self, sql: &str) -> DbResult<QueryPlan> {
        let body = dbiji_core::dialect::ansi::strip_trailing_semicolon(sql).to_string();
        let json_rows = self.rows(&format!("EXPLAIN json = 1, actions = 1, indexes = 1 {body}")).await;
        let json_text = match json_rows {
            Ok(rows) => Some(rows.iter().filter_map(|r| r.first().map(|v| v.to_text().into_owned())).collect::<Vec<_>>().join("\n")),
            Err(e) if e.kind == ErrorKind::Syntax || e.kind == ErrorKind::Query => {
                tracing::debug!("EXPLAIN json failed ({e}); falling back to EXPLAIN PLAN");
                None
            }
            Err(e) => return Err(e),
        };
        let text_rows = self.rows(&format!("EXPLAIN PLAN indexes = 1 {body}")).await;
        let plain = match text_rows {
            Ok(rows) => Some(rows.iter().filter_map(|r| r.first().map(|v| v.to_text().into_owned())).collect::<Vec<_>>().join("\n")),
            Err(e) if json_text.is_some() => {
                tracing::debug!("EXPLAIN PLAN failed: {e}");
                None
            }
            Err(e) => return Err(e),
        };
        Ok(plan::build_plan(json_text.as_deref(), plain.as_deref()))
    }
}

/// Result of the connection probe.
struct ProbeResult {
    version: String,
    database: String,
    timezone: Option<String>,
    minimal: bool,
}

fn probe(transport: &Transport, params: &ConnectParams) -> DbResult<ProbeResult> {
    transport.set_settings(default_settings(params));
    let sql = "SELECT version(), currentDatabase(), timezone(), getSetting('readonly')";
    let mut minimal = false;
    let mut tries = 0;
    loop {
        tries += 1;
        let req = QueryRequest {
            sql: sql.to_string(),
            query_id: uuid::Uuid::new_v4().to_string(),
            database: params.database.clone(),
            use_session: true,
            extra_settings: Vec::new(),
            skip_default_settings: minimal,
        };
        match transport.send_text(&req, 1 << 20) {
            Ok((head, text)) => {
                let mut lines = text.lines().filter(|l| !l.trim().is_empty());
                let _names = lines.next();
                let _types = lines.next();
                let row = lines.next().ok_or_else(|| DbError::connection("Unexpected response from the ClickHouse server"))?;
                let cells = crate::json::parse_row(row.as_bytes()).map_err(|e| DbError::connection(format!("Unexpected response from the ClickHouse server: {e}")))?;
                let text_at = |i: usize| cells.get(i).and_then(|c| c.scalar_text()).unwrap_or("").to_string();
                if minimal {
                    transport.set_settings(Vec::new());
                }
                return Ok(ProbeResult { version: text_at(0), database: text_at(1), timezone: head.timezone.or_else(|| Some(text_at(2)).filter(|t| !t.is_empty())), minimal });
            }
            Err(e) if tries < 4 => {
                let code = e.code.as_deref().unwrap_or("");
                if code.starts_with("164") || code.starts_with("115") {
                    // readonly = 1 user or an old server without some setting
                    minimal = true;
                    continue;
                }
                if code.starts_with("374") {
                    // "Session timeout 'N' is larger than max_session_timeout: M"
                    if let Some(max) = e
                        .message
                        .rsplit("max_session_timeout:")
                        .next()
                        .and_then(|t| t.trim().trim_end_matches('.').split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|d| d.parse::<u32>().ok())
                        .filter(|m| *m > 0)
                    {
                        transport.session_timeout.store(max, Ordering::Relaxed);
                        continue;
                    }
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
}

#[async_trait]
impl Session for ClickHouseSession {
    fn kind(&self) -> DbKind {
        DbKind::ClickHouse
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(self.cancel.clone())
    }

    async fn ping(&mut self) -> DbResult<()> {
        let v = self.scalar("SELECT 1").await?;
        if v.as_i64() == Some(1) { Ok(()) } else { Err(DbError::connection("Unexpected ping response")) }
    }

    async fn close(&mut self) -> DbResult<()> {
        self.closed = true;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        let sql = crate::params::bind(sql, params, self.dialect.as_ref())?;
        self.start(sql, opts.timeout, true).await
    }

    fn transaction_mode(&self) -> TransactionMode {
        TransactionMode::Auto
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        match mode {
            TransactionMode::Auto => Ok(()),
            TransactionMode::Manual => Err(DbError::unsupported("ClickHouse does not support transactions; every statement is applied immediately")),
        }
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&mut self) -> DbResult<()> {
        Ok(())
    }

    async fn rollback(&mut self) -> DbResult<()> {
        Ok(())
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        let v = self.scalar("SELECT currentDatabase()").await?;
        let db = v.as_str().map(str::to_string);
        if let (Some(d), Ok(mut g)) = (&db, self.database.lock()) {
            *g = Some(d.clone());
        }
        Ok(db)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        self.current_database().await
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        self.set_current_database(schema).await
    }

    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        self.run(&format!("USE {}", ident(database))).await?;
        if let Ok(mut g) = self.database.lock() {
            *g = Some(database.to_string());
        }
        Ok(())
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        catalog::list_databases(self).await
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        catalog::list_schemas(self).await
    }

    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        catalog::list_objects(self, schema, kind).await
    }

    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        catalog::list_children(self, parent, kind).await
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        catalog::table_definition(self, table).await
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        catalog::view_definition(self, view).await
    }

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        catalog::routine_definition(self, routine).await
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        catalog::object_ddl(self, obj).await
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        catalog::object_properties(self, obj).await
    }

    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        catalog::object_dependencies(self, obj).await
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        catalog::object_statistics(self, obj).await
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        catalog::search_metadata(self, search).await
    }

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        catalog::schema_snapshot(self, schema, opts).await
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        if analyze {
            return Err(DbError::unsupported("ClickHouse has no EXPLAIN ANALYZE; use the query log (system.query_log) for actual statistics"));
        }
        self.explain_impl(sql).await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        let sql = "SELECT query_id, user, toString(address) AS address, elapsed, read_rows, read_bytes, written_rows, \
                   memory_usage, peak_memory_usage, current_database, client_name, http_user_agent, \
                   is_initial_query, query \
                   FROM system.processes WHERE query_id != queryID() ORDER BY elapsed DESC";
        let cols = [
            "query_id",
            "user",
            "address",
            "elapsed",
            "read_rows",
            "read_bytes",
            "written_rows",
            "memory_usage",
            "peak_memory_usage",
            "current_database",
            "client_name",
            "http_user_agent",
            "is_initial_query",
            "query",
        ];
        let rows = match self.rows(sql).await {
            Ok(r) => r,
            // queryID() is missing on old servers (UNKNOWN_FUNCTION)
            Err(e) if e.code.as_deref().is_some_and(|c| c.starts_with("46 ")) => self.rows(&sql.replace(" WHERE query_id != queryID()", "")).await?,
            Err(e) => return Err(e),
        };
        Ok(SessionList {
            data: TabularData { columns: cols.iter().map(|c| c.to_string()).collect(), rows },
            id_column: 0,
            query_column: Some(13),
            state_column: None,
            blocked_by_column: None,
        })
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        let sync = mode == KillMode::Terminate;
        let transport = self.transport.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || transport.kill_query(&id, sync)).await.map_err(|e| DbError::internal(format!("kill task failed: {e}")))?
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        catalog::server_metrics(self).await
    }
}

/// `'text'` helper re-exported for catalog queries.
pub(crate) fn lit(s: &str) -> String {
    sql_string(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_and_use() {
        assert_eq!(first_keyword("  -- c\n /* x */ select 1"), "SELECT");
        assert_eq!(first_keyword("# hash\nINSERT INTO t VALUES (1)"), "INSERT");
        assert_eq!(first_keyword("(SELECT 1) UNION ALL (SELECT 2)"), "SELECT");
        assert_eq!(use_target("USE analytics"), Some("analytics".into()));
        assert_eq!(use_target("use `my db`;"), Some("my db".into()));
        assert_eq!(use_target("SELECT 1"), None);
        assert_eq!(use_target("-- reuse old db\nUSE analytics"), Some("analytics".into()));
        assert_eq!(use_target("USE `my db` -- x"), Some("my db".into()));
        assert_eq!(use_target("USE analytics; -- done"), Some("analytics".into()));
    }

    #[test]
    fn settings_from_properties() {
        let mut cfg = dbiji_core::ConnectionConfig::new(DbKind::ClickHouse, "x");
        cfg.read_only = true;
        cfg.properties.insert("setting.max_threads".into(), "4".into());
        cfg.properties.insert("setting.bad name".into(), "4".into());
        let p = ConnectParams::from_config(&cfg, None);
        let s = default_settings(&p);
        assert!(s.contains(&("readonly".to_string(), "2".to_string())));
        assert!(s.contains(&("max_threads".to_string(), "4".to_string())));
        assert!(!s.iter().any(|(k, _)| k.contains(' ')));
    }
}
