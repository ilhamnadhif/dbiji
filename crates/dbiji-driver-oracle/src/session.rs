//! [`OracleSession`]: one thin-client connection. `oracledb` is a blocking
//! client whose `Connection` is `Send + Sync` (it locks internally), so every
//! call runs on a blocking thread with a shared `Arc<Connection>`.

use crate::catalog;
use crate::convert::{self, Read};
use crate::dialect::{OracleDialect, qualified, quote};
use crate::error::OraResultExt;
use async_trait::async_trait;
use dbiji_core::admin::{KillMode, SessionList, TabularData};
use dbiji_core::config::SslMode;
use dbiji_core::ddl::{
    RoutineDef, SchemaSnapshot, SequenceDef, SnapshotOptions, TableDef, ViewDef,
};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::value::ColumnMeta;
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor,
    ResultHeader, Row, ServerMessage, Session, TransactionMode, Value,
};
use oracledb::{Connection, ToDbValue};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Rows the server sends per round trip.
const FETCH_ARRAY: u32 = 500;

/// Connect descriptor for `params`: a service name (the database field) or a
/// `sid` property, over TCP or TCPS, or the `connect_descriptor` property as is.
pub(crate) fn descriptor(params: &ConnectParams) -> String {
    if let Some(d) = params
        .property("connect_descriptor")
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        return d.to_string();
    }
    // TCPS is a listener of its own: "prefer" cannot fall back, so it means TCP.
    let tls = matches!(
        params.ssl.mode,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
    );
    let protocol = if tls { "TCPS" } else { "TCP" };
    let target = match params
        .property("sid")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sid) => format!("(SID={sid})"),
        None => format!(
            "(SERVICE_NAME={})",
            params
                .database
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .unwrap_or("FREEPDB1")
        ),
    };
    let mut security = String::new();
    if tls {
        let verify = if params.ssl.mode == SslMode::VerifyFull {
            "TRUE"
        } else {
            "FALSE"
        };
        security = format!("(SECURITY=(SSL_SERVER_DN_MATCH={verify}))");
    }
    format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={})(PORT={})){security}(CONNECT_DATA={target}))",
        params.host, params.port
    )
}

/// Statement text as the server wants it: SQL without a trailing `;`, a
/// PL/SQL block with its final `;` (statement splitters remove it).
pub(crate) fn normalize(sql: &str) -> String {
    let mut s = sql.trim();
    // A trailing `/` line (SQL*Plus) ends a block.
    if let Some(rest) = s.strip_suffix('/')
        && rest.ends_with(['\n', '\r'])
    {
        s = rest.trim_end();
    }
    if is_plsql(s) {
        let mut s = s.to_string();
        if !s.ends_with(';') {
            s.push(';');
        }
        s
    } else {
        s.trim_end_matches(';').trim_end().to_string()
    }
}

/// The first words of a statement, upper case, comments skipped.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map(|(_, r)| r).unwrap_or("");
        } else {
            break;
        }
    }
    for w in rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
    {
        words.push(w.to_ascii_uppercase());
        if words.len() == n {
            break;
        }
    }
    words
}

/// PL/SQL: anonymous blocks and stored code (`CREATE [OR REPLACE]
/// [EDITIONABLE] PROCEDURE | FUNCTION | PACKAGE | TRIGGER | TYPE ...`).
pub(crate) fn is_plsql(sql: &str) -> bool {
    let w = leading_words(sql, 6);
    match w.first().map(String::as_str) {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => w
            .iter()
            .skip(1)
            .find(|w| {
                !matches!(
                    w.as_str(),
                    "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING"
                )
            })
            .is_some_and(|w| {
                matches!(
                    w.as_str(),
                    "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY"
                )
            }),
        _ => false,
    }
}

/// Stored code compiled by `sql`: (type, name) for reading its errors.
fn compiled_object(sql: &str) -> Option<(String, String)> {
    let w = leading_words(sql, 8);
    if w.first().map(String::as_str) != Some("CREATE") {
        return None;
    }
    let mut it = w.iter().skip(1).skip_while(|w| {
        matches!(
            w.as_str(),
            "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE"
        )
    });
    let mut ty = it.next()?.clone();
    let mut next = it.next()?.clone();
    if next == "BODY" {
        ty = format!("{ty} BODY");
        next = it.next()?.clone();
    }
    // `schema.name`: the words were split on the dot.
    let name = it
        .next()
        .filter(|_| sql.to_ascii_uppercase().contains(&format!("{next}.")))
        .cloned()
        .unwrap_or(next);
    Some((ty, name))
}

/// Command tag of a statement without rows: `INSERT`, `CREATE TABLE` ...
fn tag(sql: &str) -> String {
    let w = leading_words(sql, 2);
    match w.first().map(String::as_str) {
        Some("CREATE" | "ALTER" | "DROP" | "TRUNCATE") => w.join(" "),
        Some(first) => first.to_string(),
        None => String::new(),
    }
}

struct Cancel {
    params: ConnectParams,
    /// `sid,serial#` of the session.
    target: String,
    requested: AtomicBool,
}

#[async_trait]
impl CancelHandle for Cancel {
    /// `ALTER SYSTEM CANCEL SQL` from a second connection (needs the ALTER
    /// SYSTEM privilege); an open cursor stops fetching in any case.
    async fn cancel(&self) -> DbResult<()> {
        self.requested.store(true, Ordering::SeqCst);
        let params = self.params.clone();
        let target = self.target.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open(&params)?;
            let r = conn
                .execute(&format!("ALTER SYSTEM CANCEL SQL '{target}'"), &[])
                .db();
            let _ = Arc::new(conn);
            r.map(|_| ()).map_err(|e| {
                if e.code.as_deref() == Some("ORA-01031") {
                    DbError::unsupported("Cancelling a statement needs the ALTER SYSTEM privilege")
                } else {
                    e
                }
            })
        })
        .await
        .map_err(|e| DbError::internal(e.to_string()))?
    }
}

fn open(params: &ConnectParams) -> DbResult<Connection> {
    let user = params.user.as_deref().unwrap_or("");
    let password = params.password.as_ref().map(|p| p.expose()).unwrap_or("");
    let mut cfg = oracledb::Config::default()
        .set_credentials(user, password)
        .set_connect_string(&descriptor(params))
        .db()?
        .set_transport_connect_timeout(Some(params.connect_timeout))
        .set_program(&params.application_name)
        .db()?;
    if let Some(dir) = params.property("wallet_location").filter(|d| !d.is_empty()) {
        cfg = cfg.set_wallet_location(dir);
        if let Some(pw) = params.property("wallet_password") {
            cfg = cfg.set_wallet_password(pw);
        }
    }
    let mode = match params
        .property("role")
        .map(|r| r.trim().to_ascii_uppercase())
    {
        Some(r) if r == "SYSDBA" => oracledb::AUTH_MODE_SYSDBA,
        Some(r) if r == "SYSOPER" => oracledb::AUTH_MODE_SYSOPER,
        Some(r) if r == "SYSDG" => oracledb::AUTH_MODE_SYSDGD,
        Some(r) if r == "SYSBACKUP" => oracledb::AUTH_MODE_SYSBKP,
        Some(r) if r == "SYSKM" => oracledb::AUTH_MODE_SYSKMT,
        _ => oracledb::AUTH_MODE_DEFAULT,
    };
    cfg = cfg.set_auth_mode(mode);
    oracledb::connect(cfg).db()
}

pub struct OracleSession {
    conn: Arc<Connection>,
    info: ServerInfo,
    dialect: Arc<OracleDialect>,
    mode: TransactionMode,
    in_tx: bool,
    closed: Arc<AtomicBool>,
    cancel: Arc<Cancel>,
    service: Option<String>,
}

impl OracleSession {
    pub async fn connect(params: &ConnectParams) -> DbResult<Self> {
        let p = params.clone();
        let (conn, info, service) = tokio::task::spawn_blocking(move || -> DbResult<_> {
            let conn = open(&p)?;
            let version = conn.version().db()?;
            let sid = conn.session_id().db()?;
            let serial = conn.serial_num().db()?;
            // ISO dates for implicit conversions (text typed into the grid, literals).
            conn.execute(
                "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
                 NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' NLS_NUMERIC_CHARACTERS = '.,'",
                &[],
            )
            .db()?;
            if let Some(tz) = p.property("time_zone").filter(|t| !t.is_empty() && *t != "local" && *t != "utc") {
                // Unknown region names fail: the server's zone is kept.
                let _ = conn.execute(&format!("ALTER SESSION SET TIME_ZONE = '{}'", tz.replace('\'', "")), &[]);
            }
            let _ = conn.execute("BEGIN DBMS_OUTPUT.ENABLE(NULL); END;", &[]);
            let v = version.to_string();
            let info = ServerInfo { product: "Oracle Database".into(), version_number: parse_version_number(&v), version: v, session_id: Some(format!("{sid},{serial}")) };
            let service = conn.service_name().ok().map(str::to_string).filter(|s| !s.is_empty());
            Ok((conn, info, service))
        })
        .await
        .map_err(|e| DbError::internal(e.to_string()))??;
        let cancel = Arc::new(Cancel {
            params: params.clone(),
            target: info.session_id.clone().unwrap_or_default(),
            requested: AtomicBool::new(false),
        });
        Ok(OracleSession {
            conn: Arc::new(conn),
            info,
            dialect: Arc::new(OracleDialect::new()),
            mode: if params.auto_commit {
                TransactionMode::Auto
            } else {
                TransactionMode::Manual
            },
            in_tx: false,
            closed: Arc::new(AtomicBool::new(false)),
            cancel,
            service,
        })
    }

    /// Run `f` with the connection on a blocking thread.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::connection("The connection is closed"));
        }
        let conn = self.conn.clone();
        let closed = self.closed.clone();
        let r = tokio::task::spawn_blocking(move || f(&conn))
            .await
            .map_err(|e| DbError::internal(e.to_string()))?;
        if let Err(e) = &r
            && e.kind == ErrorKind::Connection
        {
            closed.store(true, Ordering::SeqCst);
        }
        r
    }
}

/// Lines written with DBMS_OUTPUT by the last block.
fn dbms_output(conn: &Connection) -> Vec<ServerMessage> {
    let mut out = Vec::new();
    for _ in 0..10_000 {
        let Ok(mut r) = conn.execute(
            "BEGIN DBMS_OUTPUT.GET_LINE(:1, :2); END;",
            &[&oracledb::DB_TYPE_VARCHAR, &oracledb::DB_TYPE_NUMBER],
        ) else {
            break;
        };
        let row = r.out_bind_data();
        let status: i64 = row.get::<Option<i64>>(1).ok().flatten().unwrap_or(1);
        if status != 0 {
            break;
        }
        let line: String = row
            .get::<Option<String>>(0)
            .ok()
            .flatten()
            .unwrap_or_default();
        out.push(ServerMessage::new("OUTPUT", line));
    }
    out
}

/// Compilation errors of stored code just created.
fn compile_errors(conn: &Connection, sql: &str) -> Vec<ServerMessage> {
    let Some((ty, name)) = compiled_object(sql) else {
        return Vec::new();
    };
    let Ok(rows) = catalog::query(
        conn,
        "SELECT line, position, text, attribute FROM user_errors WHERE name = :1 AND type = :2 ORDER BY sequence",
        &[&name, &ty],
    ) else {
        return Vec::new();
    };
    rows.iter()
        .map(|r| {
            let at = format!(
                "{}/{}",
                catalog::text(&r[0]).unwrap_or_default(),
                catalog::text(&r[1]).unwrap_or_default()
            );
            let severity = catalog::text(&r[3]).unwrap_or_else(|| "ERROR".into());
            let mut m = ServerMessage::new(
                severity,
                format!("{at} {}", catalog::text(&r[2]).unwrap_or_default().trim()),
            );
            m.detail = Some(format!("{ty} {name}"));
            m
        })
        .collect()
}

/// What `execute` produced.
enum Outcome {
    Rows {
        columns: Vec<ColumnMeta>,
        reads: Vec<Read>,
        cursor: Box<oracledb::Cursor>,
    },
    Done {
        rows: u64,
        tag: String,
        messages: Vec<ServerMessage>,
        dml: bool,
        plsql: bool,
    },
}

fn run(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    opts: &ExecOptions,
    auto_commit: bool,
) -> DbResult<Outcome> {
    let binds: Vec<Box<dyn ToDbValue + Send + Sync>> = params.iter().map(convert::bind).collect();
    let refs: Vec<&dyn ToDbValue> = binds.iter().map(|b| b.as_ref() as &dyn ToDbValue).collect();
    if let Some(t) = opts.timeout {
        let _ = conn.set_call_timeout(Some(t));
    }
    let result = (|| -> DbResult<Outcome> {
        let mut stmt = conn
            .statement(sql)
            .db()?
            .fetch_array_size(FETCH_ARRAY)
            .build()
            .db()?;
        if stmt.is_query() {
            let cursor = stmt.query(&refs).db()?;
            let (columns, reads): (Vec<ColumnMeta>, Vec<Read>) =
                cursor.columns().iter().map(convert::column).unzip();
            return Ok(Outcome::Rows {
                columns,
                reads,
                cursor: Box::new(cursor),
            });
        }
        let (dml, plsql, ddl) = (stmt.is_dml(), stmt.is_plsql(), stmt.is_ddl());
        let result = stmt.execute(&refs).db()?;
        let rows = result.rows_affected();
        if auto_commit && (dml || plsql) {
            conn.commit().db()?;
        }
        let mut messages = Vec::new();
        if plsql && !sql.trim_start().to_ascii_uppercase().starts_with("CREATE") {
            messages.extend(dbms_output(conn));
        }
        if ddl || plsql {
            if let Ok(Some(w)) = conn.last_warning() {
                messages.push(ServerMessage::new(
                    "WARNING",
                    w.lines().next().unwrap_or(&w).to_string(),
                ));
            }
            if messages.iter().any(|m| m.severity == "WARNING") {
                messages.extend(compile_errors(conn, sql));
            }
        }
        Ok(Outcome::Done {
            rows,
            tag: tag(sql),
            messages,
            dml,
            plsql,
        })
    })();
    if opts.timeout.is_some() {
        let _ = conn.set_call_timeout(None);
    }
    result
}

struct OracleCursor {
    header: Option<ResultHeader>,
    cursor: Option<Box<oracledb::Cursor>>,
    reads: Arc<Vec<Read>>,
    messages: Vec<ServerMessage>,
    cancel: Arc<Cancel>,
    done: bool,
}

#[async_trait]
impl QueryCursor for OracleCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        Ok(self.header.take())
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        if self.done {
            return Ok(Vec::new());
        }
        let Some(mut cursor) = self.cursor.take() else {
            return Ok(Vec::new());
        };
        let reads = self.reads.clone();
        let cancel = self.cancel.clone();
        let (cursor, rows, done) = tokio::task::spawn_blocking(move || -> DbResult<_> {
            let mut rows = Vec::with_capacity(max_rows.min(4096));
            let mut done = false;
            while rows.len() < max_rows.max(1) {
                if cancel.requested.load(Ordering::SeqCst) {
                    return Err(DbError::cancelled());
                }
                match cursor.next() {
                    Some(r) => rows.push(Row::from(convert::row(&r.db()?, &reads)?)),
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            Ok((cursor, rows, done))
        })
        .await
        .map_err(|e| DbError::internal(e.to_string()))??;
        self.done = done;
        if !done {
            self.cursor = Some(cursor);
        }
        Ok(rows)
    }

    async fn close(&mut self) -> DbResult<()> {
        self.cursor = None;
        self.done = true;
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.messages)
    }
}

#[async_trait]
impl Session for OracleSession {
    fn kind(&self) -> DbKind {
        DbKind::Oracle
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
        self.blocking(|c| c.ping().db()).await.map_err(|e| {
            if e.kind == ErrorKind::Connection {
                e
            } else {
                DbError::connection(e.message)
            }
        })
    }

    async fn close(&mut self) -> DbResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        // The connection closes when the last reference (a cursor) is dropped.
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        let sql = normalize(sql);
        if sql.is_empty() {
            return Ok(Box::new(OracleCursor {
                header: None,
                cursor: None,
                reads: Arc::new(Vec::new()),
                messages: Vec::new(),
                cancel: self.cancel.clone(),
                done: true,
            }));
        }
        self.cancel.requested.store(false, Ordering::SeqCst);
        let words = leading_words(&sql, 1);
        let first = words.first().map(String::as_str).unwrap_or("");
        let params = params.to_vec();
        let opts = opts.clone();
        let auto = self.mode == TransactionMode::Auto;
        let text = sql.clone();
        let outcome = self
            .blocking(move |c| run(c, &text, &params, &opts, auto))
            .await?;
        let cancel = self.cancel.clone();
        match outcome {
            Outcome::Rows {
                columns,
                reads,
                cursor,
            } => {
                if first == "SELECT" && sql.to_ascii_uppercase().contains("FOR UPDATE") && !auto {
                    self.in_tx = true;
                }
                Ok(Box::new(OracleCursor {
                    header: Some(ResultHeader::Rows(columns)),
                    cursor: Some(cursor),
                    reads: Arc::new(reads),
                    messages: Vec::new(),
                    cancel,
                    done: false,
                }))
            }
            Outcome::Done {
                rows,
                tag,
                messages,
                dml,
                plsql,
            } => {
                match first {
                    "COMMIT" | "ROLLBACK" if !words.is_empty() => self.in_tx = false,
                    // DDL commits implicitly.
                    "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE" | "RENAME"
                    | "COMMENT" => self.in_tx = false,
                    _ if !auto && (dml || plsql) => self.in_tx = true,
                    _ => {}
                }
                let rows_affected = if dml { Some(rows) } else { None };
                Ok(Box::new(OracleCursor {
                    header: Some(ResultHeader::Update {
                        rows_affected,
                        last_insert_id: None,
                        tag: Some(tag),
                    }),
                    cursor: None,
                    reads: Arc::new(Vec::new()),
                    messages,
                    cancel,
                    done: true,
                }))
            }
        }
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        if mode == TransactionMode::Auto && self.in_tx {
            self.commit().await?;
        }
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        self.in_tx
    }

    async fn commit(&mut self) -> DbResult<()> {
        self.blocking(|c| c.commit().db()).await?;
        self.in_tx = false;
        Ok(())
    }

    async fn rollback(&mut self) -> DbResult<()> {
        self.blocking(|c| c.rollback().db()).await?;
        self.in_tx = false;
        Ok(())
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(self.service.clone())
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        self.blocking(catalog::current_schema).await
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let sql = format!("ALTER SESSION SET CURRENT_SCHEMA = {}", quote(schema));
        self.blocking(move |c| c.execute(&sql, &[]).db().map(|_| ()))
            .await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(vec![DatabaseInfo {
            name: self.service.clone().unwrap_or_else(|| "ORACLE".into()),
            is_current: true,
            ..Default::default()
        }])
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        self.blocking(catalog::list_schemas).await
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let schema = match schema.schema.clone() {
            Some(s) => s,
            None => self
                .blocking(catalog::current_schema)
                .await?
                .unwrap_or_default(),
        };
        self.blocking(move |c| catalog::list_objects(c, &schema, kind))
            .await
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let parent = parent.clone();
        self.blocking(move |c| catalog::list_children(c, &parent, kind))
            .await
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let table = table.clone();
        self.blocking(move |c| catalog::table_definition(c, &table))
            .await
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let view = view.clone();
        self.blocking(move |c| catalog::view_definition(c, &view))
            .await
    }

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        let routine = routine.clone();
        self.blocking(move |c| catalog::routine_definition(c, &routine))
            .await
    }

    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        let sequence = sequence.clone();
        self.blocking(move |c| catalog::sequence_definition(c, &sequence))
            .await
    }

    /// DBMS_METADATA when allowed (it needs SELECT_CATALOG_ROLE for other
    /// users' objects), else DDL generated from the dictionary.
    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let o = obj.clone();
        if let Ok(ddl) = self.blocking(move |c| catalog::metadata_ddl(c, &o)).await
            && !ddl.trim().is_empty()
        {
            return Ok(ddl);
        }
        let dialect = self.dialect.clone();
        let ddl = dialect.ddl();
        Ok(match obj.kind {
            ObjectKind::Table => {
                let t = self.table_definition(obj).await?;
                ddl.create_table(&t, true)
                    .iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let v = self.view_definition(obj).await?;
                ddl.create_view(&v, true).join(";\n") + ";"
            }
            ObjectKind::Sequence => {
                let s = self.sequence_definition(obj).await?;
                format!("{};", ddl.create_sequence(&s))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                self.routine_definition(obj).await?.definition
            }
            _ => return Err(DbError::unsupported("DDL of this object")),
        })
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let search = search.clone();
        self.blocking(move |c| catalog::search(c, &search)).await
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let name = match schema.schema.clone() {
            Some(s) => s,
            None => self
                .blocking(catalog::current_schema)
                .await?
                .unwrap_or_default(),
        };
        let o = *opts;
        let n = name.clone();
        let (tables, views, sequences) = self
            .blocking(move |c| {
                let tables = catalog::table_definitions(c, &n, None, &o)?;
                let mut views = Vec::new();
                if o.views {
                    for v in catalog::list_objects(c, &n, ObjectKind::View)? {
                        if o.view_definitions || o.columns {
                            views.push(catalog::view_definition(c, &v.obj)?);
                        } else {
                            views.push(ViewDef {
                                schema: Some(n.clone()),
                                name: v.obj.name,
                                ..Default::default()
                            });
                        }
                    }
                }
                let mut sequences = Vec::new();
                if o.sequences {
                    for s in catalog::list_objects(c, &n, ObjectKind::Sequence)? {
                        sequences.push(catalog::sequence_definition(c, &s.obj)?);
                    }
                }
                Ok((tables, views, sequences))
            })
            .await?;
        Ok(SchemaSnapshot {
            kind: Some(DbKind::Oracle),
            database: self.service.clone(),
            schema: Some(name),
            tables,
            views,
            sequences,
            routines: Vec::new(),
            types: Vec::new(),
        })
    }

    /// `EXPLAIN PLAN` into PLAN_TABLE, read back as a tree and as DBMS_XPLAN text.
    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        if analyze {
            return Err(DbError::unsupported("EXPLAIN ANALYZE"));
        }
        let sql = normalize(sql);
        let id = format!(
            "DBIJI{}",
            std::process::id() as u64 * 1000
                + (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_millis())
                    .unwrap_or(0) as u64)
        );
        self.blocking(move |c| {
            c.execute(&format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {sql}"), &[]).db()?;
            let rows = catalog::query(
                c,
                "SELECT id, parent_id, operation, options, object_owner, object_name, object_alias, cost, cardinality, bytes, access_predicates, filter_predicates \
                 FROM plan_table WHERE statement_id = :1 ORDER BY id",
                &[&id.as_str()],
            )?;
            let text = catalog::query(c, "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', :1, 'TYPICAL'))", &[&id.as_str()])
                .map(|r| r.iter().filter_map(|r| r.first().and_then(catalog::text)).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            let _ = c.execute("DELETE FROM plan_table WHERE statement_id = :1", &[&id.as_str()]);
            let _ = c.commit();
            Ok(QueryPlan { roots: plan_tree(&rows), text, raw: None, planning_ms: None, execution_ms: None, analyzed: false })
        })
        .await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        self.blocking(|c| {
            let rows = catalog::query(
                c,
                "SELECT s.sid || ',' || s.serial# id, s.username, s.status, s.osuser, s.machine, s.program, s.logon_time, s.event, s.blocking_session, q.sql_text \
                 FROM v$session s LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number \
                 WHERE s.type = 'USER' ORDER BY s.sid",
                &[],
            )?;
            let mut data = TabularData::new(["ID", "User", "Status", "OS user", "Machine", "Program", "Logon", "Wait event", "Blocked by", "Query"].map(String::from).to_vec());
            data.rows = rows;
            Ok(SessionList { data, id_column: 0, query_column: Some(9), state_column: Some(2), blocked_by_column: Some(8) })
        })
        .await
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        let id = id.replace('\'', "");
        let sql = match mode {
            KillMode::CancelQuery => format!("ALTER SYSTEM CANCEL SQL '{id}'"),
            KillMode::Terminate => format!("ALTER SYSTEM KILL SESSION '{id}' IMMEDIATE"),
        };
        self.blocking(move |c| c.execute(&sql, &[]).db().map(|_| ()))
            .await
    }
}

/// PLAN_TABLE rows (id, parent_id, ...) as a tree.
fn plan_tree(rows: &[Vec<Value>]) -> Vec<PlanNode> {
    let num = |v: &Value| match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(s) => s.parse().ok(),
        _ => None,
    };
    let mut nodes: Vec<(i64, Option<i64>, PlanNode)> = rows
        .iter()
        .map(|r| {
            let t = |i: usize| r.get(i).and_then(catalog::text);
            let mut n = PlanNode::new(match t(3) {
                Some(o) => format!("{} {}", t(2).unwrap_or_default(), o),
                None => t(2).unwrap_or_default(),
            });
            n.relation = match (t(4), t(5)) {
                (Some(o), Some(name)) => Some(qualified(Some(&o), &name)),
                (None, Some(name)) => Some(name),
                _ => None,
            };
            if n.node_type.contains("INDEX") {
                n.index = n.relation.take();
            }
            n.alias = t(6);
            n.total_cost = r.get(7).and_then(num);
            n.plan_rows = r.get(8).and_then(num);
            n.plan_width = r.get(9).and_then(num);
            n.condition = t(10).or(t(11));
            if let Some(f) = t(11) {
                n.properties.push(("Filter".into(), f));
            }
            if let Some(a) = t(10) {
                n.properties.push(("Access".into(), a));
            }
            let id = r.first().and_then(num).unwrap_or(0.0) as i64;
            let parent = r.get(1).and_then(num).map(|p| p as i64);
            (id, parent, n)
        })
        .collect();
    // Attach children bottom-up (ids are in depth-first order).
    let mut roots = Vec::new();
    while let Some((id, parent, node)) = nodes.pop() {
        let _ = id;
        match parent.and_then(|p| nodes.iter_mut().find(|(i, _, _)| *i == p)) {
            Some((_, _, p)) => p.children.insert(0, node),
            None => roots.insert(0, node),
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::config::ConnectionConfig;

    #[test]
    fn statements_are_sent_as_oracle_wants_them() {
        assert_eq!(normalize("SELECT 1 FROM dual;"), "SELECT 1 FROM dual");
        assert_eq!(normalize("BEGIN NULL; END"), "BEGIN NULL; END;");
        assert_eq!(
            normalize("create or replace procedure p as begin null; end;\n/"),
            "create or replace procedure p as begin null; end;"
        );
        assert_eq!(
            normalize("CREATE TABLE t (a NUMBER);"),
            "CREATE TABLE t (a NUMBER)"
        );
        assert!(is_plsql("-- hi\nDECLARE x NUMBER; BEGIN NULL; END;"));
        assert!(is_plsql(
            "CREATE OR REPLACE EDITIONABLE PACKAGE BODY p AS END;"
        ));
        assert!(!is_plsql("CREATE TABLE t (a NUMBER)"));
        assert_eq!(
            compiled_object("CREATE OR REPLACE PACKAGE BODY hr.pkg AS END;"),
            Some(("PACKAGE BODY".into(), "PKG".into()))
        );
        assert_eq!(
            compiled_object("create function f return number is begin return 1; end;"),
            Some(("FUNCTION".into(), "F".into()))
        );
        assert_eq!(tag("create table t (a number)"), "CREATE TABLE");
        assert_eq!(tag("insert into t values (1)"), "INSERT");
    }

    #[test]
    fn descriptors() {
        let mut cfg = ConnectionConfig::new(DbKind::Oracle, "x");
        cfg.host = "db".into();
        cfg.database = Some("ORCLPDB1".into());
        let p = ConnectParams::from_config(&cfg, None);
        assert_eq!(
            descriptor(&p),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=ORCLPDB1)))"
        );
        cfg.properties.insert("sid".into(), "ORCL".into());
        cfg.ssl.mode = SslMode::VerifyFull;
        let p = ConnectParams::from_config(&cfg, None);
        assert_eq!(
            descriptor(&p),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db)(PORT=1521))(SECURITY=(SSL_SERVER_DN_MATCH=TRUE))(CONNECT_DATA=(SID=ORCL)))"
        );
    }

    #[test]
    fn plan_rows_become_a_tree() {
        let row = |id: i64, parent: Option<i64>, op: &str| {
            vec![
                Value::Int(id),
                parent.map_or(Value::Null, Value::Int),
                Value::Text(op.into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Int(3),
                Value::Int(10),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        };
        let roots = plan_tree(&[
            row(0, None, "SELECT STATEMENT"),
            row(1, Some(0), "HASH JOIN"),
            row(2, Some(1), "TABLE ACCESS"),
            row(3, Some(1), "INDEX"),
        ]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].children[0].node_type, "HASH JOIN");
        assert_eq!(roots[0].children[0].children.len(), 2);
        assert_eq!(roots[0].children[0].children[0].node_type, "TABLE ACCESS");
    }
}
