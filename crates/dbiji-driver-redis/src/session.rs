//! Redis [`Session`]: command execution, keys as tables, catalog and metrics.
//!
//! One session is one multiplexed connection. Redis databases are shown as
//! schemas (as MySQL databases are): `SELECT n` switches, `CONFIG GET
//! databases` says how many exist. A key is a table; its fields (a hash's
//! fields, otherwise one `value` column) are the table's columns.

use crate::command::{self, parse_command};
use crate::dialect::RedisDialect;
use crate::error::map_error;
use crate::types::{self, Table};
use async_trait::async_trait;
use dbiji_core::admin::*;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::{CancelHandle, ColumnMeta, ConnectParams, DbError, DbKind, DbResult, ExecOptions, QueryCursor, Session, TransactionMode, Value};
use redis::{Client, Cmd, Value as Rv};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Keys a scan returns at most (the navigator stays responsive).
pub const MAX_KEYS: usize = 10_000;
/// Elements written into the DDL of one key.
pub const MAX_DDL_ELEMENTS: usize = 1_000;

/// One Redis session.
pub struct RedisSession {
    client: Client,
    /// The multiplexed connection, opened on first use and cloned afterwards
    /// (a clone shares the socket; the driver reconnects on its own).
    conn: std::sync::Mutex<Option<redis::aio::MultiplexedConnection>>,
    /// Timeout of a connection attempt, kept for the reconnect in [`Self::conn`].
    connect_timeout: Duration,
    dialect: Arc<RedisDialect>,
    info: ServerInfo,
    database: i64,
    databases: i64,
    read_only: bool,
    closed: bool,
}

/// Settings of the multiplexed connection.
///
/// `AsyncConnectionConfig::default()` caps every response at 500 ms
/// (`DEFAULT_RESPONSE_TIMEOUT`), which failed any command that took longer —
/// a large `HGETALL` / `SMEMBERS`, a paused or busy server — with a bare
/// "timed out". DBiji bounds a statement with `ExecOptions::timeout` instead,
/// and a key read has no bound, so the client's cap is off.
fn connection_config(connect_timeout: Duration) -> redis::AsyncConnectionConfig {
    redis::AsyncConnectionConfig::new()
        .set_connection_timeout(Some(connect_timeout.max(Duration::from_millis(500))))
        .set_response_timeout(None)
}

impl RedisSession {
    /// Connect with the parameters resolved by the application.
    pub async fn connect(params: &ConnectParams) -> DbResult<RedisSession> {
        let url = connection_url(params);
        let client = Client::open(url).map_err(|e| map_error(&e).with_context("Opening the Redis connection"))?;
        let database = params.database.as_deref().and_then(|d| d.trim().parse::<i64>().ok()).unwrap_or(0);
        let connect_timeout = params.connect_timeout.max(Duration::from_millis(500));
        let mut conn = client
            .get_multiplexed_async_connection_with_config(&connection_config(connect_timeout))
            .await
            .map_err(|e| map_error(&e).with_context("Connecting to Redis"))?;
        let info_reply: String = Cmd::new().arg("INFO").arg("server").query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("Reading INFO"))?;
        let version = info_field(&info_reply, "redis_version").unwrap_or_default();
        // Valkey and KeyDB answer the same INFO; the product name stays Redis
        // because the command language is identical.
        let product = "Redis";
        let databases = databases_of(&mut conn).await.unwrap_or(16);
        Ok(RedisSession {
            client,
            conn: std::sync::Mutex::new(Some(conn)),
            connect_timeout,
            dialect: Arc::new(RedisDialect::new()),
            info: ServerInfo { product: product.into(), version_number: parse_version_number(&version), version, session_id: None },
            database,
            databases,
            read_only: params.read_only,
            closed: false,
        })
    }

    async fn conn(&self) -> DbResult<redis::aio::MultiplexedConnection> {
        if let Some(c) = self.conn.lock().ok().and_then(|c| c.clone()) {
            return Ok(c);
        }
        let c = self
            .client
            .get_multiplexed_async_connection_with_config(&connection_config(self.connect_timeout))
            .await
            .map_err(|e| map_error(&e).with_context("Reconnecting to Redis"))?;
        if let Ok(mut slot) = self.conn.lock() {
            *slot = Some(c.clone());
        }
        Ok(c)
    }

    /// Run a command on the session's connection.
    async fn run(&self, args: &[String]) -> DbResult<Rv> {
        let mut conn = self.conn().await?;
        run_on(&mut conn, args).await
    }

    /// Run a command with a timeout.
    async fn run_timeout(&self, args: &[String], timeout: Option<Duration>) -> DbResult<Rv> {
        let fut = self.run(args);
        match timeout {
            Some(t) => tokio::time::timeout(t, fut).await.map_err(|_| DbError::timeout("The command exceeded its time limit"))?,
            None => fut.await,
        }
    }

    /// `SELECT n`, remembering the database.
    async fn select(&mut self, n: i64) -> DbResult<()> {
        if n == self.database {
            return Ok(());
        }
        if n < 0 || n >= self.databases.max(1) {
            return Err(DbError::config(format!("Redis database {n} does not exist (0–{})", self.databases - 1)));
        }
        let args = vec!["SELECT".to_string(), n.to_string()];
        self.run(&args).await.map_err(|e| e.with_context("Selecting the database"))?;
        self.database = n;
        Ok(())
    }

    /// Scan the keys of the current database.
    async fn scan_keys(&self, pattern: &str, limit: usize) -> DbResult<Vec<String>> {
        let mut conn = self.conn().await?;
        let mut cursor = 0u64;
        let mut out = Vec::new();
        loop {
            let mut cmd = Cmd::new();
            cmd.arg("SCAN").arg(cursor).arg("COUNT").arg(1000);
            if !pattern.is_empty() && pattern != "*" {
                cmd.arg("MATCH").arg(pattern);
            }
            let reply: Rv = cmd.query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("SCAN"))?;
            if let Rv::Array(parts) = reply
                && parts.len() == 2
            {
                cursor = types::scalar_text(&parts[0]).parse().unwrap_or(0);
                if let Rv::Array(keys) = &parts[1] {
                    for k in keys {
                        out.push(types::scalar_text(k));
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
            if cursor == 0 {
                return Ok(out);
            }
        }
    }

    /// Type of a key (`none` when it does not exist).
    async fn key_type(&self, key: &str) -> DbResult<String> {
        let mut conn = self.conn().await?;
        let t: String = Cmd::new().arg("TYPE").arg(key).query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("TYPE"))?;
        Ok(t)
    }

    /// Length of a key (elements or bytes).
    async fn key_len(&self, key: &str, kind: &str) -> Option<i64> {
        let cmd = match kind {
            "string" => "STRLEN",
            "list" => "LLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "hash" => "HLEN",
            "stream" => "XLEN",
            _ => return None,
        };
        let mut conn = self.conn().await.ok()?;
        Cmd::new().arg(cmd).arg(key).query_async(&mut conn).await.ok()
    }

    /// Elements of a key, up to `MAX_DDL_ELEMENTS`.
    async fn elements(&self, key: &str, kind: &str) -> DbResult<Vec<Rv>> {
        let mut conn = self.conn().await?;
        let mut cmd = Cmd::new();
        match kind {
            "list" => {
                cmd.arg("LRANGE").arg(key).arg(0).arg(MAX_DDL_ELEMENTS - 1);
            }
            "set" => {
                cmd.arg("SMEMBERS").arg(key);
            }
            "zset" => {
                cmd.arg("ZRANGE").arg(key).arg(0).arg(MAX_DDL_ELEMENTS - 1).arg("WITHSCORES");
            }
            "hash" => {
                cmd.arg("HGETALL").arg(key);
            }
            "stream" => {
                cmd.arg("XRANGE").arg(key).arg("-").arg("+").arg("COUNT").arg(MAX_DDL_ELEMENTS);
            }
            _ => return Ok(Vec::new()),
        }
        let reply: Rv = cmd.query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("Reading the key"))?;
        Ok(match reply {
            Rv::Array(a) | Rv::Set(a) => a,
            Rv::Map(entries) => entries.into_iter().flat_map(|(k, v)| [k, v]).collect(),
            Rv::Nil => Vec::new(),
            other => vec![other],
        })
    }

    /// The command that writes the key as it is now.
    async fn key_ddl(&self, key: &str) -> DbResult<String> {
        let kind = self.key_type(key).await?;
        let q = crate::ddl::quote;
        match kind.as_str() {
            "none" => Ok(format!("# {} does not exist\n", q(key))),
            "string" => {
                let value = self.run(&["GET".into(), key.to_string()]).await?;
                Ok(format!("SET {} {}\n", q(key), q(&types::scalar_text(&value))))
            }
            "list" => {
                let items = self.elements(key, "list").await?;
                let values = items.iter().map(|i| q(&types::scalar_text(i))).collect::<Vec<_>>().join(" ");
                Ok(format!("DEL {}\nRPUSH {} {values}\n", q(key), q(key)))
            }
            "set" => {
                let items = self.elements(key, "set").await?;
                let values = items.iter().map(|i| q(&types::scalar_text(i))).collect::<Vec<_>>().join(" ");
                Ok(format!("DEL {}\nSADD {} {values}\n", q(key), q(key)))
            }
            "zset" => {
                let items = self.elements(key, "zset").await?;
                let mut pairs = Vec::new();
                for chunk in items.chunks(2) {
                    if let (Some(member), Some(score)) = (chunk.first(), chunk.get(1)) {
                        pairs.push(format!("{} {}", types::scalar_text(score), q(&types::scalar_text(member))));
                    }
                }
                Ok(format!("DEL {}\nZADD {} {}\n", q(key), q(key), pairs.join(" ")))
            }
            "hash" => {
                let items = self.elements(key, "hash").await?;
                let mut pairs = Vec::new();
                for chunk in items.chunks(2) {
                    if let (Some(field), Some(value)) = (chunk.first(), chunk.get(1)) {
                        pairs.push(format!("{} {}", q(&types::scalar_text(field)), q(&types::scalar_text(value))));
                    }
                }
                Ok(format!("DEL {}\nHSET {} {}\n", q(key), q(key), pairs.join(" ")))
            }
            "stream" => {
                let items = self.elements(key, "stream").await?;
                let mut out = format!("DEL {}\n", q(key));
                for entry in items {
                    if let Rv::Array(pair) = entry
                        && pair.len() == 2
                    {
                        let id = types::scalar_text(&pair[0]);
                        if let Rv::Array(fields) = &pair[1] {
                            let mut args = Vec::new();
                            for chunk in fields.chunks(2) {
                                if let (Some(f), Some(v)) = (chunk.first(), chunk.get(1)) {
                                    args.push(format!("{} {}", q(&types::scalar_text(f)), q(&types::scalar_text(v))));
                                }
                            }
                            out.push_str(&format!("XADD {} {} {}\n", q(key), id, args.join(" ")));
                        }
                    }
                }
                Ok(out)
            }
            other => Ok(format!("# {other} keys cannot be recreated as commands\n")),
        }
    }

    /// Rows of a key's value: its fields, elements or entries.
    async fn key_rows(&self, key: &str) -> DbResult<Box<dyn QueryCursor>> {
        let kind = self.key_type(key).await?;
        let text = |v: &Rv| Value::Text(types::scalar_text(v).into());
        let (columns, rows): (Vec<ColumnMeta>, Vec<Vec<Value>>) = match kind.as_str() {
            "none" => (vec![ColumnMeta::new("value", "nil", dbiji_core::DataKind::Other)], Vec::new()),
            "string" => {
                let reply = self.run(&["GET".to_string(), key.to_string()]).await?;
                (vec![ColumnMeta::new("value", "string", dbiji_core::DataKind::Text)], vec![vec![types::to_value(&reply)]])
            }
            "list" | "set" => {
                let items = self.elements(key, &kind).await?;
                (vec![ColumnMeta::new("value", &kind, dbiji_core::DataKind::Text)], items.iter().map(|i| vec![types::to_value(i)]).collect())
            }
            "hash" => {
                let items = self.elements(key, "hash").await?;
                let rows = items.chunks(2).map(|c| vec![c.first().map(text).unwrap_or(Value::Null), c.get(1).map(types::to_value).unwrap_or(Value::Null)]).collect();
                (vec![ColumnMeta::new("field", "string", dbiji_core::DataKind::Text), ColumnMeta::new("value", "string", dbiji_core::DataKind::Text)], rows)
            }
            "zset" => {
                let items = self.elements(key, "zset").await?;
                let rows = items.chunks(2).map(|c| vec![c.first().map(text).unwrap_or(Value::Null), c.get(1).map(types::to_value).unwrap_or(Value::Null)]).collect();
                (vec![ColumnMeta::new("member", "string", dbiji_core::DataKind::Text), ColumnMeta::new("score", "double", dbiji_core::DataKind::Float)], rows)
            }
            "stream" => {
                let items = self.elements(key, "stream").await?;
                let rows = items
                    .iter()
                    .filter_map(|e| match e {
                        Rv::Array(pair) if pair.len() == 2 => {
                            let fields = match &pair[1] {
                                Rv::Array(f) => f
                                    .chunks(2)
                                    .filter_map(|c| Some(format!("{}: {}", types::scalar_text(c.first()?), types::scalar_text(c.get(1)?))))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => types::scalar_text(other),
                            };
                            Some(vec![text(&pair[0]), Value::Text(fields.into())])
                        }
                        _ => None,
                    })
                    .collect();
                (vec![ColumnMeta::new("id", "stream id", dbiji_core::DataKind::Text), ColumnMeta::new("fields", "object", dbiji_core::DataKind::Json)], rows)
            }
            other => {
                let reply = self.run(&["DUMP".to_string(), key.to_string()]).await?;
                (vec![ColumnMeta::new("value", other, dbiji_core::DataKind::Binary)], vec![vec![types::to_value(&reply)]])
            }
        };
        Ok(Box::new(dbiji_core::driver::MemoryCursor::rows(columns, rows)))
    }

    /// Fields shown as the columns of a key.
    async fn fields(&self, key: &str, kind: &str) -> DbResult<Vec<(String, String)>> {
        match kind {
            "hash" => {
                let mut conn = self.conn().await?;
                let fields: Vec<String> = Cmd::new().arg("HKEYS").arg(key).query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("HKEYS"))?;
                Ok(fields.into_iter().map(|f| (f, "string".to_string())).collect())
            }
            "stream" => Ok(vec![("id".into(), "stream id".into()), ("fields".into(), "object".into())]),
            "list" | "set" => Ok(vec![("value".into(), kind.into())]),
            "zset" => Ok(vec![("member".into(), "string".into()), ("score".into(), "double".into())]),
            "none" => Ok(Vec::new()),
            _ => Ok(vec![("value".into(), "string".into())]),
        }
    }
}

/// `redis://user:pass@host:port/db`, `rediss://` with TLS.
///
/// Redis has no "use TLS when offered" negotiation: `prefer` stays plain
/// unless the connection asks for TLS with the `tls` property.
fn connection_url(params: &ConnectParams) -> String {
    let mode = params.ssl.mode;
    let tls = matches!(mode, dbiji_core::config::SslMode::Require | dbiji_core::config::SslMode::VerifyCa | dbiji_core::config::SslMode::VerifyFull)
        || params.property("tls").is_some_and(|v| v == "true" || v == "1");
    let scheme = if tls { "rediss" } else { "redis" };
    let mut url = String::from(scheme);
    url.push_str("://");
    // Redis 6 ACLs allow a user name, but a password-only server (the `default`
    // user) is the common case: `redis://:password@host` is the URL form for
    // it. Requiring a user name would silently drop the password and fail
    // every command with NOAUTH.
    let user = params.user.as_deref().filter(|u| !u.is_empty());
    let password = params.password.as_ref().filter(|p| !p.is_empty());
    if user.is_some() || password.is_some() {
        if let Some(user) = user {
            url.push_str(&percent_encode(user));
        }
        if let Some(password) = password {
            url.push(':');
            url.push_str(&percent_encode(password.expose()));
        }
        url.push('@');
    }
    let host = if params.host.contains(':') { format!("[{}]", params.host) } else { params.host.clone() };
    url.push_str(&host);
    url.push(':');
    url.push_str(&params.port.to_string());
    url.push('/');
    if let Some(db) = params.database.as_deref().and_then(|d| d.trim().parse::<i64>().ok()) {
        url.push_str(&db.to_string());
    }
    // Without a verified chain (or with `tls=true` on `prefer`) the certificate
    // is accepted as it is, as the other drivers do for `require`.
    if tls && !mode.verifies_chain() {
        url.push_str("#insecure");
    }
    url
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn run_on(conn: &mut redis::aio::MultiplexedConnection, args: &[String]) -> DbResult<Rv> {
    let Some((name, rest)) = args.split_first() else {
        return Err(DbError::syntax("The command is empty"));
    };
    let mut cmd = Cmd::new();
    cmd.arg(name);
    for a in rest {
        cmd.arg(a);
    }
    cmd.query_async(conn).await.map_err(|e| map_error(&e))
}

async fn databases_of(conn: &mut redis::aio::MultiplexedConnection) -> Option<i64> {
    let reply: Rv = Cmd::new().arg("CONFIG").arg("GET").arg("databases").query_async(conn).await.ok()?;
    let text = match reply {
        Rv::Array(items) => items.get(1).map(types::scalar_text)?,
        Rv::Map(entries) => entries.iter().find(|(k, _)| types::scalar_text(k) == "databases").map(|(_, v)| types::scalar_text(v))?,
        other => types::scalar_text(&other),
    };
    text.parse().ok()
}

/// `key:value` line of an `INFO` reply.
fn info_field(info: &str, key: &str) -> Option<String> {
    info.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

fn table_of(t: Table) -> DbResult<Box<dyn QueryCursor>> {
    Ok(Box::new(dbiji_core::driver::MemoryCursor::rows(t.columns, t.rows)))
}

#[async_trait]
impl Session for RedisSession {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        None
    }

    async fn ping(&mut self) -> DbResult<()> {
        let mut conn = self.conn().await?;
        let reply: Rv = Cmd::new().arg("PING").query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("PING"))?;
        let _ = reply;
        Ok(())
    }

    async fn close(&mut self) -> DbResult<()> {
        self.closed = true;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        if !params.is_empty() {
            return Err(DbError::unsupported("Redis commands take their arguments inline; statement parameters are not supported"));
        }
        if self.closed {
            return Err(DbError::connection("The connection is closed"));
        }
        let mut args = parse_command(sql).map_err(DbError::syntax)?;
        // `DBIJI.KEYDATA key` (a DBiji extension, not a Redis command): the
        // rows of the key's value whatever its type — what the data editor shows.
        if command::command_name(&args) == "DBIJI.KEYDATA" {
            let key = args.get(1).cloned().ok_or_else(|| DbError::syntax("DBIJI.KEYDATA needs a key"))?;
            return self.key_rows(&key).await;
        }
        // `SELECT n` changes the session's database: remember it.
        if command::command_name(&args) == "SELECT"
            && let Some(n) = args.get(1).and_then(|n| n.trim().parse::<i64>().ok())
        {
            self.select(n).await?;
            return Ok(Box::new(dbiji_core::driver::MemoryCursor::update(None, Some(format!("switched to database {n}")))));
        }
        if self.read_only && !command::is_read(&args) {
            return Err(DbError::read_only("The connection is read-only; this command changes data"));
        }
        // Commands without a reply (SUBSCRIBE, MONITOR) would block the session.
        let name = command::command_name(&args);
        if matches!(name.as_str(), "SUBSCRIBE" | "PSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC") {
            return Err(DbError::unsupported(format!("{name} keeps the connection busy and is not supported")));
        }
        let reply = self.run_timeout(&args, opts.timeout).await?;
        args.truncate(1);
        table_of(types::shape(&args, reply))
    }

    fn transaction_mode(&self) -> TransactionMode {
        TransactionMode::Auto
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        match mode {
            TransactionMode::Auto => Ok(()),
            TransactionMode::Manual => Err(DbError::unsupported("Redis transactions (MULTI / EXEC) are written as commands, not toggled here")),
        }
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&mut self) -> DbResult<()> {
        Err(DbError::unsupported("Redis has no interactive transaction in DBiji; write MULTI / EXEC as commands"))
    }

    async fn rollback(&mut self) -> DbResult<()> {
        Err(DbError::unsupported("Redis has no interactive transaction in DBiji; write MULTI / DISCARD as commands"))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.database.to_string()))
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.database.to_string()))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let n = schema.trim().parse::<i64>().map_err(|_| DbError::config(format!("'{schema}' is not a Redis database number")))?;
        self.select(n).await
    }

    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        self.set_current_schema(database).await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        // Redis databases are presented as schemas (see `list_schemas`).
        Ok(Vec::new())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        let mut out = Vec::with_capacity(self.databases.max(1) as usize);
        for n in 0..self.databases.max(1) {
            out.push(SchemaInfo { name: n.to_string(), is_current: n == self.database, is_system: false, owner: None, comment: None });
        }
        Ok(out)
    }

    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        if !matches!(kind, ObjectKind::Table | ObjectKind::View) {
            return Ok(Vec::new());
        }
        let previous = self.database;
        if let Some(db) = schema.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let keys = self.scan_keys("*", MAX_KEYS).await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        let keys = keys?;
        let mut out = Vec::with_capacity(keys.len());
        let mut conn = self.conn().await?;
        for key in keys {
            let kind: String = Cmd::new().arg("TYPE").arg(&key).query_async(&mut conn).await.unwrap_or_default();
            let mut info = ObjectInfo::new(ObjectRef { kind: ObjectKind::Table, database: None, schema: schema.schema.clone(), table: None, name: key, signature: None });
            info.detail = Some(kind);
            out.push(info);
        }
        Ok(out)
    }

    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        if !matches!(kind, ObjectKind::Column) {
            return Ok(Vec::new());
        }
        let previous = self.database;
        if let Some(db) = parent.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let result = async {
            let key_type = self.key_type(&parent.name).await?;
            self.fields(&parent.name, &key_type).await
        }
        .await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        let fields = result?;
        Ok(fields
            .into_iter()
            .map(|(name, ty)| {
                let mut info = ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, name));
                info.detail = Some(ty);
                info
            })
            .collect())
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let previous = self.database;
        if let Some(db) = table.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let result = async {
            let key_type = self.key_type(&table.name).await?;
            let fields = self.fields(&table.name, &key_type).await?;
            Ok::<_, DbError>((key_type, fields))
        }
        .await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        let (key_type, fields) = result?;
        let mut def = TableDef { schema: table.schema.clone(), name: table.name.clone(), ..Default::default() };
        def.options.insert("type".into(), key_type);
        for (i, (name, ty)) in fields.into_iter().enumerate() {
            let mut c = ColumnDef::new(name, ty);
            c.ordinal = i as u32 + 1;
            c.nullable = true;
            def.columns.push(c);
        }
        Ok(def)
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let previous = self.database;
        if let Some(db) = obj.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let out = self.key_ddl(&obj.name).await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        out
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let previous = self.database;
        if let Some(db) = obj.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let result = async {
            let key_type = self.key_type(&obj.name).await?;
            let mut out = vec![Property::new("name", obj.name.clone()), Property::new("type", key_type.clone())];
            let mut conn = self.conn().await?;
            let ttl: i64 = Cmd::new().arg("TTL").arg(&obj.name).query_async(&mut conn).await.unwrap_or(-1);
            out.push(Property::new("ttl (seconds)", if ttl < 0 { "none".to_string() } else { ttl.to_string() }));
            if let Some(len) = self.key_len(&obj.name, &key_type).await {
                let label = if key_type == "string" { "length (bytes)" } else { "elements" };
                out.push(Property::new(label, len.to_string()));
            }
            if let Ok(encoding) = Cmd::new().arg("OBJECT").arg("ENCODING").arg(&obj.name).query_async::<String>(&mut conn).await {
                out.push(Property::new("encoding", encoding));
            }
            if let Ok(usage) = Cmd::new().arg("MEMORY").arg("USAGE").arg(&obj.name).query_async::<i64>(&mut conn).await {
                out.push(Property::new("memory (bytes)", usage.to_string()));
            }
            Ok::<_, DbError>(out)
        }
        .await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        result
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        if search.kinds.iter().all(|k| !matches!(k, ObjectKind::Table | ObjectKind::View)) {
            return Ok(Vec::new());
        }
        let pattern = if search.pattern.contains('*') || search.pattern.contains('?') { search.pattern.clone() } else { format!("*{}*", search.pattern) };
        let pattern = if search.case_sensitive { pattern } else { pattern.to_ascii_lowercase() };
        let mut out = Vec::new();
        let dbs: Vec<String> = if search.schemas.is_empty() { vec![self.database.to_string()] } else { search.schemas.clone() };
        for db in dbs {
            let previous = self.database;
            self.set_current_schema(&db).await?;
            let keys = self.scan_keys(&pattern, search.limit.max(1)).await;
            if previous != self.database {
                let _ = self.set_current_schema(&previous.to_string()).await;
            }
            for key in keys? {
                if out.len() >= search.limit.max(1) {
                    return Ok(out);
                }
                let matches = if search.case_sensitive { key.contains(&search.pattern) } else { key.to_ascii_lowercase().contains(&search.pattern.to_ascii_lowercase()) };
                if matches || pattern != "*" {
                    out.push(ObjectInfo::new(ObjectRef { kind: ObjectKind::Table, database: None, schema: Some(db.clone()), table: None, name: key, signature: None }));
                }
            }
        }
        Ok(out)
    }

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        let previous = self.database;
        if let Some(db) = schema.schema.as_deref() {
            self.set_current_schema(db).await?;
        }
        let result = async {
            let keys = self.scan_keys("*", MAX_KEYS).await?;
            let mut snap = SchemaSnapshot { kind: Some(DbKind::Redis), database: None, schema: schema.schema.clone(), ..Default::default() };
            for key in keys {
                let key_type = self.key_type(&key).await?;
                let mut def = TableDef { schema: schema.schema.clone(), name: key.clone(), ..Default::default() };
                def.options.insert("type".into(), key_type.clone());
                if opts.columns {
                    for (i, (field, ty)) in self.fields(&key, &key_type).await?.into_iter().enumerate() {
                        let mut c = ColumnDef::new(field, ty);
                        c.ordinal = i as u32 + 1;
                        c.nullable = true;
                        def.columns.push(c);
                    }
                }
                snap.tables.push(def);
            }
            Ok::<_, DbError>(snap)
        }
        .await;
        if previous != self.database {
            let _ = self.set_current_schema(&previous.to_string()).await;
        }
        result
    }

    async fn explain(&mut self, _sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        Err(DbError::unsupported("Redis has no query plans"))
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        let mut conn = self.conn().await?;
        let text: String = Cmd::new().arg("CLIENT").arg("LIST").query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("CLIENT LIST"))?;
        let mut columns = vec!["id".to_string(), "addr".to_string(), "name".to_string(), "db".to_string(), "age".to_string(), "idle".to_string(), "cmd".to_string()];
        let mut rows = Vec::new();
        for line in text.lines() {
            let fields: BTreeMap<&str, &str> = line.split_whitespace().filter_map(|p| p.split_once('=')).collect();
            rows.push(vec![
                Value::Text(fields.get("id").copied().unwrap_or("").into()),
                Value::Text(fields.get("addr").copied().unwrap_or("").into()),
                Value::Text(fields.get("name").copied().unwrap_or("").into()),
                Value::Text(fields.get("db").copied().unwrap_or("").into()),
                Value::Int(fields.get("age").and_then(|v| v.parse().ok()).unwrap_or(0)),
                Value::Int(fields.get("idle").and_then(|v| v.parse().ok()).unwrap_or(0)),
                Value::Text(fields.get("cmd").copied().unwrap_or("").into()),
            ]);
        }
        columns.truncate(columns.len());
        Ok(SessionList { data: TabularData { columns, rows }, id_column: 0, query_column: Some(6), state_column: Some(5), blocked_by_column: None })
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        let mut conn = self.conn().await?;
        let mut cmd = Cmd::new();
        cmd.arg("CLIENT").arg("KILL").arg("ID").arg(id);
        if mode == KillMode::CancelQuery {
            cmd.arg("TYPE").arg("normal");
        }
        let _: i64 = cmd.query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("CLIENT KILL"))?;
        Ok(())
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        let mut conn = self.conn().await?;
        let info: String = Cmd::new().arg("INFO").query_async(&mut conn).await.map_err(|e| map_error(&e).with_context("INFO"))?;
        let num = |key: &str| info_field(&info, key).and_then(|v| v.parse::<f64>().ok());
        let mut out = Vec::new();
        if let Some(v) = num("connected_clients") {
            out.push(Metric::gauge("Clients", "clients.connected", "Connected clients", v, ""));
        }
        if let Some(v) = num("blocked_clients") {
            out.push(Metric::gauge("Clients", "clients.blocked", "Blocked clients", v, ""));
        }
        if let Some(v) = num("used_memory") {
            out.push(Metric::gauge("Memory", "memory.used", "Used memory", v, "bytes"));
        }
        if let Some(v) = num("used_memory_rss") {
            out.push(Metric::gauge("Memory", "memory.rss", "Resident memory", v, "bytes"));
        }
        if let Some(v) = num("keyspace_hits") {
            out.push(Metric::counter("Keys", "keyspace.hits", "Keyspace hits", v, ""));
        }
        if let Some(v) = num("keyspace_misses") {
            out.push(Metric::counter("Keys", "keyspace.misses", "Keyspace misses", v, ""));
        }
        if let Some(v) = num("total_commands_processed") {
            out.push(Metric::counter("Commands", "commands.total", "Commands processed", v, ""));
        }
        if let Some(v) = num("expired_keys") {
            out.push(Metric::counter("Keys", "keys.expired", "Expired keys", v, ""));
        }
        if let Some(v) = num("evicted_keys") {
            out.push(Metric::counter("Keys", "keys.evicted", "Evicted keys", v, ""));
        }
        // Keys of every database from the `dbN:keys=…` lines.
        for line in info.lines().filter(|l| l.starts_with("db")) {
            if let Some((db, rest)) = line.split_once(':')
                && let Some(keys) = rest.split(',').find_map(|p| p.strip_prefix("keys="))
                && let Ok(v) = keys.parse::<f64>()
            {
                out.push(Metric::gauge("Keys", &format!("keys.{}", db.trim_start_matches("db")), &format!("Keys in {db}"), v, ""));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_carries_the_database_and_the_tls_mode() {
        use dbiji_core::config::{SslConfig, SslMode};
        let mut params = ConnectParams {
            kind: DbKind::Redis,
            host: "cache".into(),
            port: 6379,
            tls_server_name: None,
            database: Some("3".into()),
            user: Some("app".into()),
            password: Some("p@ss word".into()),
            file_path: None,
            ssl: SslConfig::default(),
            properties: Default::default(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(5),
            read_only: false,
            auto_commit: true,
        };
        assert_eq!(connection_url(&params), "redis://app:p%40ss%20word@cache:6379/3");
        params.ssl.mode = SslMode::Require;
        assert_eq!(connection_url(&params), "rediss://app:p%40ss%20word@cache:6379/3#insecure");
        params.ssl.mode = SslMode::VerifyFull;
        assert_eq!(connection_url(&params), "rediss://app:p%40ss%20word@cache:6379/3");
    }

    /// A password-only server (no ACL user) must still authenticate: the URL
    /// keeps the empty user name and redis-rs parses it as `AUTH <password>`.
    #[test]
    fn a_password_without_a_user_is_kept() {
        use dbiji_core::config::SslConfig;
        let params = ConnectParams {
            kind: DbKind::Redis,
            host: "cache".into(),
            port: 6379,
            tls_server_name: None,
            database: Some("3".into()),
            user: None,
            password: Some("p@ss word".into()),
            file_path: None,
            ssl: SslConfig::default(),
            properties: Default::default(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(5),
            read_only: false,
            auto_commit: true,
        };
        assert_eq!(connection_url(&params), "redis://:p%40ss%20word@cache:6379/3");
        let client = Client::open(connection_url(&params)).expect("the URL is valid");
        let info = client.get_connection_info().redis_settings();
        assert_eq!(info.username(), None);
        assert_eq!(info.password(), Some("p@ss word"));
        assert_eq!(info.protocol(), redis::ProtocolVersion::RESP2);
    }

    #[test]
    fn info_fields_are_read_by_name() {
        let info = "# Server\r\nredis_version:7.2.4\r\nconnected_clients:3\r\ndb0:keys=2,expires=0\r\n";
        assert_eq!(info_field(info, "redis_version").as_deref(), Some("7.2.4"));
        assert_eq!(info_field(info, "connected_clients").as_deref(), Some("3"));
        assert_eq!(info_field(info, "missing"), None);
    }
}
