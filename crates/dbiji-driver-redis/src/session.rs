//! [`RedisSession`]: multiplexed async connections (one per database
//! index), commands from the editor, keys for the navigator.
//!
//! Large values never come back in one reply: `SCAN`/`HSCAN`/`SSCAN`/`ZSCAN`
//! from cursor 0, `LRANGE`, `ZRANGE` (by rank) and `XRANGE` are read page by
//! page while the grid fetches, so a hash of millions of fields neither blocks
//! the server (as `HGETALL` would) nor fills the memory.

use crate::dialect::{RedisDialect, arg, split_args};
use async_trait::async_trait;
use dbiji_core::admin::{KillMode, Metric, SessionList, TabularData};
use dbiji_core::config::SslMode;
use dbiji_core::ddl::{ColumnDef, KeyDef, SchemaSnapshot, SnapshotOptions, TableDef};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::value::{ColumnMeta, DataKind};
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor,
    ResultHeader, Row, ServerMessage, Session, TransactionMode, Value,
};
use redis::aio::MultiplexedConnection;
use redis::{
    AsyncConnectionConfig, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo,
    Value as Reply,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// Elements read per round trip when paging a reply.
const PAGE: i64 = 1000;
/// Keys listed per database in the navigator.
const MAX_KEYS: usize = 10_000;

fn err(e: redis::RedisError) -> DbError {
    let text = e.to_string();
    let kind = if e.is_timeout() {
        ErrorKind::Timeout
    } else if e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() {
        ErrorKind::Connection
    } else if e
        .code()
        .is_some_and(|c| matches!(c, "NOAUTH" | "WRONGPASS" | "NOPERM"))
        || text.contains("WRONGPASS")
        || text.contains("NOAUTH")
    {
        ErrorKind::Auth
    } else if text.contains("syntax error")
        || text.contains("unknown command")
        || text.contains("wrong number of arguments")
    {
        ErrorKind::Syntax
    } else {
        ErrorKind::Query
    };
    let mut d = DbError::new(kind, text);
    if let Some(c) = e.code() {
        d = d.with_code(c);
    }
    d
}

fn text_of(v: &Reply) -> Option<String> {
    match v {
        Reply::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Reply::SimpleString(s) => Some(s.clone()),
        Reply::VerbatimString { text, .. } => Some(text.clone()),
        Reply::Int(i) => Some(i.to_string()),
        Reply::Double(d) => Some(d.to_string()),
        Reply::Okay => Some("OK".into()),
        _ => None,
    }
}

fn json(v: &Reply) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Reply::Nil => J::Null,
        Reply::Int(i) => J::from(*i),
        Reply::Double(d) => serde_json::Number::from_f64(*d).map_or(J::Null, J::Number),
        Reply::Boolean(b) => J::Bool(*b),
        Reply::Array(a) | Reply::Set(a) => J::Array(a.iter().map(json).collect()),
        Reply::Map(m) => J::Object(
            m.iter()
                .map(|(k, v)| (text_of(k).unwrap_or_default(), json(v)))
                .collect(),
        ),
        Reply::Push { data, .. } => J::Array(data.iter().map(json).collect()),
        Reply::Attribute { data, .. } => json(data),
        other => text_of(other).map_or(J::Null, J::String),
    }
}

/// A reply element as a grid value.
fn value(v: &Reply) -> Value {
    match v {
        Reply::Nil => Value::Null,
        Reply::Int(i) => Value::Int(*i),
        Reply::Double(d) => Value::Float(*d),
        Reply::Boolean(b) => Value::Bool(*b),
        Reply::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.into()),
            Err(_) => Value::Bytes(b.clone().into()),
        },
        Reply::SimpleString(s) => Value::Text(s.as_str().into()),
        Reply::VerbatimString { text, .. } => Value::Text(text.as_str().into()),
        Reply::Okay => Value::Text("OK".into()),
        Reply::Attribute { data, .. } => value(data),
        other => Value::Json(json(other).to_string().into()),
    }
}

/// A score read as a number when it is one.
fn number(v: &Reply) -> Value {
    match text_of(v).and_then(|s| s.parse::<f64>().ok()) {
        Some(f) => Value::Float(f),
        None => value(v),
    }
}

fn col(name: &str, kind: DataKind) -> ColumnMeta {
    ColumnMeta::new(
        name,
        if kind == DataKind::Float {
            "double"
        } else {
            "string"
        },
        kind,
    )
}

/// `[k1, v1, k2, v2 …]` (or a RESP3 map) as rows.
fn pairs(v: &Reply, score: bool) -> Vec<Vec<Value>> {
    match v {
        Reply::Map(m) => m
            .iter()
            .map(|(k, v)| vec![value(k), if score { number(v) } else { value(v) }])
            .collect(),
        Reply::Array(a)
            if a.iter()
                .all(|x| matches!(x, Reply::Array(p) if p.len() == 2))
                && !a.is_empty() =>
        {
            a.iter()
                .filter_map(|x| match x {
                    Reply::Array(p) => Some(vec![
                        value(&p[0]),
                        if score { number(&p[1]) } else { value(&p[1]) },
                    ]),
                    _ => None,
                })
                .collect()
        }
        Reply::Array(a) => a
            .chunks(2)
            .map(|c| {
                vec![
                    value(&c[0]),
                    c.get(1)
                        .map(|x| if score { number(x) } else { value(x) })
                        .unwrap_or(Value::Null),
                ]
            })
            .collect(),
        other => vec![vec![value(other), Value::Null]],
    }
}

/// Stream entries `[[id, [f, v …]] …]` as (id, fields as JSON) rows.
fn stream_rows(v: &Reply) -> Vec<Vec<Value>> {
    let Reply::Array(entries) = v else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let Reply::Array(p) = e else { return None };
            let id = value(p.first()?);
            let fields = match p.get(1) {
                Some(Reply::Array(f)) => serde_json::Value::Object(
                    f.chunks(2)
                        .map(|c| {
                            (
                                text_of(&c[0]).unwrap_or_default(),
                                c.get(1).map(json).unwrap_or(serde_json::Value::Null),
                            )
                        })
                        .collect(),
                ),
                Some(other) => json(other),
                None => serde_json::Value::Null,
            };
            Some(vec![id, Value::Json(fields.to_string().into())])
        })
        .collect()
}

/// `INFO` text as (section, field, value) rows.
fn info_rows(text: &str) -> Vec<Vec<Value>> {
    let mut section = String::new();
    let mut rows = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(s) = line.strip_prefix('#') {
            section = s.trim().to_string();
        } else if let Some((k, v)) = line.split_once(':') {
            let val = v
                .parse::<i64>()
                .map(Value::Int)
                .or_else(|_| v.parse::<f64>().map(Value::Float))
                .unwrap_or_else(|_| Value::Text(v.into()));
            rows.push(vec![
                Value::Text(section.as_str().into()),
                Value::Text(k.into()),
                val,
            ]);
        }
    }
    rows
}

fn info_map(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.trim().split_once(':'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `CLIENT LIST` lines (`id=1 addr=… cmd=…`) as a table.
fn client_rows(text: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut cols: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<(&str, &str)> = line
            .split_whitespace()
            .filter_map(|f| f.split_once('='))
            .collect();
        if cols.is_empty() {
            cols = fields.iter().map(|(k, _)| k.to_string()).collect();
        }
        rows.push(
            cols.iter()
                .map(|c| {
                    fields
                        .iter()
                        .find(|(k, _)| k == c)
                        .map_or(Value::Null, |(_, v)| {
                            v.parse::<i64>()
                                .map(Value::Int)
                                .unwrap_or_else(|_| Value::Text((*v).into()))
                        })
                })
                .collect(),
        );
    }
    (cols, rows)
}

/// Commands that change data: their reply is a status, not a result to browse.
fn is_write(name: &str) -> bool {
    matches!(
        name,
        "SET"
            | "SETEX"
            | "PSETEX"
            | "SETNX"
            | "MSET"
            | "MSETNX"
            | "APPEND"
            | "SETRANGE"
            | "DEL"
            | "UNLINK"
            | "EXPIRE"
            | "PEXPIRE"
            | "EXPIREAT"
            | "PEXPIREAT"
            | "PERSIST"
            | "RENAME"
            | "RENAMENX"
            | "HSET"
            | "HSETNX"
            | "HMSET"
            | "HDEL"
            | "HINCRBY"
            | "HINCRBYFLOAT"
            | "LPUSH"
            | "RPUSH"
            | "LPUSHX"
            | "RPUSHX"
            | "LSET"
            | "LREM"
            | "LTRIM"
            | "LINSERT"
            | "SADD"
            | "SREM"
            | "SMOVE"
            | "ZADD"
            | "ZREM"
            | "ZINCRBY"
            | "ZREMRANGEBYRANK"
            | "ZREMRANGEBYSCORE"
            | "XADD"
            | "XDEL"
            | "XTRIM"
            | "INCR"
            | "INCRBY"
            | "INCRBYFLOAT"
            | "DECR"
            | "DECRBY"
            | "FLUSHDB"
            | "FLUSHALL"
            | "COPY"
            | "MOVE"
            | "SWAPDB"
            | "JSON.SET"
            | "JSON.DEL"
            | "PFADD"
            | "PUBLISH"
            | "SELECT"
    )
}

/// How a reply is paged while the grid fetches.
enum Paging {
    Done,
    /// `SCAN`/`HSCAN`/`SSCAN`/`ZSCAN`: the cursor is `args[at]`.
    Scan {
        args: Vec<Vec<u8>>,
        at: usize,
        pairs: Option<bool>,
        finished: bool,
    },
    /// `LRANGE` / `ZRANGE … [WITHSCORES]` by index.
    Range {
        cmd: &'static str,
        key: Vec<u8>,
        next: i64,
        stop: i64,
        scores: bool,
        index: bool,
        finished: bool,
    },
    /// `XRANGE key start end`: after the last id read.
    Stream {
        key: Vec<u8>,
        start: String,
        end: String,
        finished: bool,
    },
}

struct RedisCursor {
    header: Option<ResultHeader>,
    rows: VecDeque<Vec<Value>>,
    conn: MultiplexedConnection,
    paging: Paging,
}

impl RedisCursor {
    /// The next page of a paged reply into `rows`; false when there is none.
    async fn next_page(&mut self) -> DbResult<bool> {
        match &mut self.paging {
            Paging::Done => Ok(false),
            Paging::Scan { finished: true, .. }
            | Paging::Range { finished: true, .. }
            | Paging::Stream { finished: true, .. } => Ok(false),
            Paging::Scan {
                args,
                at,
                pairs: pair,
                finished,
            } => {
                let mut c = redis::Cmd::new();
                for a in args.iter() {
                    c.arg(a.as_slice());
                }
                let reply: Reply = c.query_async(&mut self.conn).await.map_err(err)?;
                let Reply::Array(parts) = reply else {
                    return Err(DbError::query("Unexpected SCAN reply"));
                };
                let cursor = parts
                    .first()
                    .and_then(text_of)
                    .unwrap_or_else(|| "0".into());
                let items = parts.get(1).cloned().unwrap_or(Reply::Array(Vec::new()));
                match pair {
                    Some(score) => self.rows.extend(pairs(&items, *score)),
                    None => {
                        if let Reply::Array(a) = &items {
                            self.rows.extend(a.iter().map(|x| vec![value(x)]));
                        }
                    }
                }
                *finished = cursor == "0";
                args[*at] = cursor.into_bytes();
                Ok(true)
            }
            Paging::Range {
                cmd,
                key,
                next,
                stop,
                scores,
                index,
                finished,
            } => {
                let last = if *stop < 0 {
                    *next + PAGE - 1
                } else {
                    (*next + PAGE - 1).min(*stop)
                };
                let mut c = redis::cmd(cmd);
                c.arg(key.as_slice()).arg(*next).arg(last);
                if *scores {
                    c.arg("WITHSCORES");
                }
                let reply: Reply = c.query_async(&mut self.conn).await.map_err(err)?;
                let start = *next;
                let got = if *scores {
                    let rows = pairs(&reply, true);
                    let n = rows.len();
                    self.rows.extend(rows);
                    n
                } else {
                    let Reply::Array(a) = reply else {
                        return Ok(false);
                    };
                    let n = a.len();
                    if *index {
                        self.rows.extend(
                            a.iter()
                                .enumerate()
                                .map(|(i, x)| vec![Value::Int(start + i as i64), value(x)]),
                        );
                    } else {
                        self.rows.extend(a.iter().map(|x| vec![value(x)]));
                    }
                    n
                };
                *next += got as i64;
                *finished = (got as i64) < PAGE || (*stop >= 0 && *next > *stop);
                Ok(got > 0)
            }
            Paging::Stream {
                key,
                start,
                end,
                finished,
            } => {
                let reply: Reply = redis::cmd("XRANGE")
                    .arg(key.as_slice())
                    .arg(start.as_str())
                    .arg(end.as_str())
                    .arg("COUNT")
                    .arg(PAGE)
                    .query_async(&mut self.conn)
                    .await
                    .map_err(err)?;
                let rows = stream_rows(&reply);
                let n = rows.len();
                if let Some(last) = rows
                    .last()
                    .and_then(|r| r.first())
                    .map(|v| v.to_text().into_owned())
                {
                    *start = format!("({last}");
                }
                self.rows.extend(rows);
                *finished = (n as i64) < PAGE;
                Ok(n > 0)
            }
        }
    }
}

#[async_trait]
impl QueryCursor for RedisCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        Ok(self.header.take())
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let want = max_rows.max(1);
        while self.rows.len() < want {
            if !self.next_page().await? {
                break;
            }
        }
        let n = want.min(self.rows.len());
        Ok(self.rows.drain(..n).collect())
    }

    async fn close(&mut self) -> DbResult<()> {
        self.rows.clear();
        self.paging = Paging::Done;
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

pub struct RedisSession {
    base: ConnectionInfo,
    settings: RedisConnectionInfo,
    config: AsyncConnectionConfig,
    conns: HashMap<i64, MultiplexedConnection>,
    db: i64,
    info: ServerInfo,
    dialect: Arc<RedisDialect>,
    mode: TransactionMode,
    closed: bool,
}

impl RedisSession {
    pub async fn connect(params: &ConnectParams) -> DbResult<Self> {
        let tls = matches!(
            params.ssl.mode,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
        );
        let addr = if tls {
            ConnectionAddr::TcpTls {
                host: params.host.clone(),
                port: params.port,
                insecure: params.ssl.mode != SslMode::VerifyFull,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(params.host.clone(), params.port)
        };
        let base = addr.into_connection_info().map_err(err)?;
        let db = params
            .database
            .as_deref()
            .map(|d| d.trim().trim_start_matches("db"))
            .filter(|d| !d.is_empty())
            .map(|d| {
                d.parse::<i64>()
                    .map_err(|_| DbError::config(format!("'{d}' is not a database number")))
            })
            .transpose()?
            .unwrap_or(0);
        let mut settings = RedisConnectionInfo::default();
        if let Some(u) = params.user.as_deref().filter(|u| !u.is_empty()) {
            settings = settings.set_username(u);
        }
        if let Some(p) = params
            .password
            .as_ref()
            .map(|p| p.expose())
            .filter(|p| !p.is_empty())
        {
            settings = settings.set_password(p);
        }
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(params.connect_timeout))
            .set_response_timeout(Some(Duration::from_secs(60)));
        let mut s = RedisSession {
            base,
            settings,
            config,
            conns: HashMap::new(),
            db,
            info: ServerInfo::default(),
            dialect: Arc::new(RedisDialect::new()),
            mode: TransactionMode::Auto,
            closed: false,
        };
        let mut conn = s.conn(db).await?;
        let text: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut conn)
            .await
            .map_err(err)?;
        let m = info_map(&text);
        let (product, version) = if let Some(v) = m.get("valkey_version") {
            ("Valkey", v.clone())
        } else if let Some(v) = m.get("dragonfly_version") {
            ("Dragonfly", v.clone())
        } else {
            ("Redis", m.get("redis_version").cloned().unwrap_or_default())
        };
        s.info = ServerInfo {
            product: product.into(),
            version_number: parse_version_number(&version),
            version,
            session_id: None,
        };
        Ok(s)
    }

    /// The connection of database `db` (one per database: SELECT would change it for everyone).
    async fn conn(&mut self, db: i64) -> DbResult<MultiplexedConnection> {
        if let Some(c) = self.conns.get(&db) {
            return Ok(c.clone());
        }
        let info = self
            .base
            .clone()
            .set_redis_settings(self.settings.clone().set_db(db));
        let client = redis::Client::open(info).map_err(err)?;
        let c = client
            .get_multiplexed_async_connection_with_config(&self.config)
            .await
            .map_err(err)?;
        self.conns.insert(db, c.clone());
        Ok(c)
    }

    fn db_of(schema: Option<&str>) -> Option<i64> {
        schema?.trim().trim_start_matches("db").parse().ok()
    }

    /// Up to `limit` keys matching `pattern` with their types.
    async fn keys(
        &mut self,
        db: i64,
        pattern: Option<&str>,
        limit: usize,
    ) -> DbResult<Vec<(String, String)>> {
        let mut conn = self.conn(db).await?;
        let mut cursor = "0".to_string();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut c = redis::cmd("SCAN");
            c.arg(&cursor);
            if let Some(p) = pattern {
                c.arg("MATCH").arg(p);
            }
            c.arg("COUNT").arg(PAGE);
            let (next, batch): (String, Vec<Vec<u8>>) =
                c.query_async(&mut conn).await.map_err(err)?;
            keys.extend(batch);
            cursor = next;
            if cursor == "0" || keys.len() >= limit {
                break;
            }
        }
        keys.truncate(limit);
        keys.sort();
        // The types in pipelines: one round trip per thousand keys.
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(PAGE as usize) {
            let mut p = redis::pipe();
            for k in chunk {
                p.cmd("TYPE").arg(k.as_slice());
            }
            let types: Vec<String> = p.query_async(&mut conn).await.map_err(err)?;
            out.extend(
                chunk
                    .iter()
                    .zip(types)
                    .map(|(k, t)| (String::from_utf8_lossy(k).into_owned(), t)),
            );
        }
        Ok(out)
    }

    async fn key_type(&mut self, db: i64, key: &str) -> DbResult<String> {
        let mut conn = self.conn(db).await?;
        redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(err)
    }

    async fn info_text(&mut self, section: Option<&str>) -> DbResult<String> {
        let mut conn = self.conn(self.db).await?;
        let mut c = redis::cmd("INFO");
        if let Some(s) = section {
            c.arg(s);
        }
        c.query_async(&mut conn).await.map_err(err)
    }
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
        let mut conn = self.conn(self.db).await?;
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map(|_| ())
            .map_err(|e| DbError::connection(e.to_string()))
    }

    async fn close(&mut self) -> DbResult<()> {
        self.conns.clear();
        self.closed = true;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        _opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        let mut args = split_args(sql).map_err(DbError::syntax)?;
        // `?` placeholders take the parameters in order.
        if !params.is_empty() {
            let mut p = params.iter();
            for a in args.iter_mut().filter(|a| a.as_slice() == b"?") {
                if let Some(v) = p.next() {
                    *a = v.to_text().into_owned().into_bytes();
                }
            }
        }
        let Some(first) = args.first() else {
            return Ok(Box::new(RedisCursor {
                header: None,
                rows: VecDeque::new(),
                conn: self.conn(self.db).await?,
                paging: Paging::Done,
            }));
        };
        let name = String::from_utf8_lossy(first).to_ascii_uppercase();
        let word = |i: usize| {
            args.get(i)
                .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
                .unwrap_or_default()
        };
        if matches!(
            name.as_str(),
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR"
        ) {
            return Err(DbError::unsupported(format!(
                "{name} streams forever; it cannot run in the editor"
            )));
        }
        if name == "SELECT" {
            let n: i64 = word(1)
                .parse()
                .map_err(|_| DbError::syntax("SELECT takes a database number"))?;
            self.conn(n).await?;
            self.db = n;
            return Ok(Box::new(RedisCursor {
                header: Some(ResultHeader::Update {
                    rows_affected: None,
                    last_insert_id: None,
                    tag: Some(format!("SELECT {n}")),
                }),
                rows: VecDeque::new(),
                conn: self.conn(n).await?,
                paging: Paging::Done,
            }));
        }
        let conn = self.conn(self.db).await?;
        let header = |cols: Vec<ColumnMeta>| Some(ResultHeader::Rows(cols));
        let text = |s: &str| col(s, DataKind::Text);
        // Paged replies.
        let scan_at = match name.as_str() {
            "SCAN" => Some(1),
            "HSCAN" | "SSCAN" | "ZSCAN" => Some(2),
            _ => None,
        };
        if let Some(at) = scan_at
            && args.get(at).is_some_and(|c| c.as_slice() == b"0")
        {
            let (cols, pair) = match name.as_str() {
                "SCAN" => (vec![text("key")], None),
                "HSCAN" => (vec![text("field"), text("value")], Some(false)),
                "ZSCAN" => (
                    vec![text("member"), col("score", DataKind::Float)],
                    Some(true),
                ),
                _ => (vec![text("member")], None),
            };
            return Ok(Box::new(RedisCursor {
                header: header(cols),
                rows: VecDeque::new(),
                conn,
                paging: Paging::Scan {
                    args,
                    at,
                    pairs: pair,
                    finished: false,
                },
            }));
        }
        let plain_range = |from: usize| {
            args.len() <= from
                && args
                    .iter()
                    .skip(4)
                    .all(|a| a.eq_ignore_ascii_case(b"WITHSCORES"))
        };
        if (name == "LRANGE" && args.len() == 4)
            || (name == "ZRANGE" && plain_range(5) && args.len() >= 4)
        {
            let next: i64 = word(2).parse().unwrap_or(0);
            let stop: i64 = word(3).parse().unwrap_or(-1);
            if next >= 0 && (stop == -1 || stop >= next) {
                let scores = name == "ZRANGE" && word(4) == "WITHSCORES";
                let cols = if name == "LRANGE" {
                    vec![col("index", DataKind::Int), text("value")]
                } else if scores {
                    vec![text("member"), col("score", DataKind::Float)]
                } else {
                    vec![text("member")]
                };
                let cmd = if name == "LRANGE" { "LRANGE" } else { "ZRANGE" };
                let paging = Paging::Range {
                    cmd,
                    key: args[1].clone(),
                    next,
                    stop,
                    scores,
                    index: cmd == "LRANGE",
                    finished: false,
                };
                return Ok(Box::new(RedisCursor {
                    header: header(cols),
                    rows: VecDeque::new(),
                    conn,
                    paging,
                }));
            }
        }
        if name == "XRANGE" && args.len() == 4 {
            return Ok(Box::new(RedisCursor {
                header: header(vec![text("id"), col("fields", DataKind::Json)]),
                rows: VecDeque::new(),
                conn,
                paging: Paging::Stream {
                    key: args[1].clone(),
                    start: word(2).to_lowercase(),
                    end: word(3).to_lowercase(),
                    finished: false,
                },
            }));
        }
        // Everything else: one reply.
        let mut c = redis::Cmd::new();
        for a in &args {
            c.arg(a.as_slice());
        }
        let mut conn = conn;
        let reply: Reply = c.query_async(&mut conn).await.map_err(err)?;
        if is_write(&name)
            && matches!(
                reply,
                Reply::Okay
                    | Reply::Int(_)
                    | Reply::SimpleString(_)
                    | Reply::Nil
                    | Reply::Boolean(_)
            )
        {
            let rows = match reply {
                Reply::Int(n) => Some(n.max(0) as u64),
                _ => None,
            };
            return Ok(Box::new(RedisCursor {
                header: Some(ResultHeader::Update {
                    rows_affected: rows,
                    last_insert_id: None,
                    tag: Some(name),
                }),
                rows: VecDeque::new(),
                conn,
                paging: Paging::Done,
            }));
        }
        let withscores = args.iter().any(|a| a.eq_ignore_ascii_case(b"WITHSCORES"));
        let (cols, rows): (Vec<ColumnMeta>, Vec<Vec<Value>>) = match name.as_str() {
            "INFO" => (
                vec![text("section"), text("field"), col("value", DataKind::Text)],
                info_rows(&text_of(&reply).unwrap_or_default()),
            ),
            "CLIENT" if word(1) == "LIST" => {
                let (names, rows) = client_rows(&text_of(&reply).unwrap_or_default());
                (names.iter().map(|n| text(n)).collect(), rows)
            }
            "HGETALL" => (vec![text("field"), text("value")], pairs(&reply, false)),
            "CONFIG" if word(1) == "GET" => {
                (vec![text("name"), text("value")], pairs(&reply, false))
            }
            "HRANDFIELD" if args.iter().any(|a| a.eq_ignore_ascii_case(b"WITHVALUES")) => {
                (vec![text("field"), text("value")], pairs(&reply, false))
            }
            "ZPOPMIN" | "ZPOPMAX" => (
                vec![text("member"), col("score", DataKind::Float)],
                pairs(&reply, true),
            ),
            n if n.starts_with('Z') && withscores => (
                vec![text("member"), col("score", DataKind::Float)],
                pairs(&reply, true),
            ),
            "XRANGE" | "XREVRANGE" => (
                vec![text("id"), col("fields", DataKind::Json)],
                stream_rows(&reply),
            ),
            _ => match &reply {
                Reply::Map(_) => (vec![text("key"), text("value")], pairs(&reply, false)),
                Reply::Array(a) | Reply::Set(a) => {
                    let label = match name.as_str() {
                        "KEYS" => "key",
                        "SMEMBERS" | "SINTER" | "SUNION" | "SDIFF" | "SRANDMEMBER" | "SPOP" => {
                            "member"
                        }
                        "HKEYS" => "field",
                        _ => "value",
                    };
                    (
                        vec![text(label)],
                        a.iter().map(|x| vec![value(x)]).collect(),
                    )
                }
                other => (vec![text("reply")], vec![vec![value(other)]]),
            },
        };
        Ok(Box::new(RedisCursor {
            header: header(cols),
            rows: rows.into(),
            conn,
            paging: Paging::Done,
        }))
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&mut self) -> DbResult<()> {
        Ok(())
    }

    async fn rollback(&mut self) -> DbResult<()> {
        Err(DbError::unsupported(
            "Rolling back (Redis applies every command at once)",
        ))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(None)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some(format!("db{}", self.db)))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let n = Self::db_of(Some(schema))
            .ok_or_else(|| DbError::query(format!("'{schema}' is not a Redis database")))?;
        self.conn(n).await?;
        self.db = n;
        Ok(())
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(Vec::new())
    }

    /// The databases holding keys (`INFO keyspace`), and the current one.
    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        let text = self.info_text(Some("keyspace")).await?;
        let mut dbs: Vec<(i64, String)> = text
            .lines()
            .filter_map(|l| {
                let (name, rest) = l.trim().split_once(':')?;
                let n: i64 = name.strip_prefix("db")?.parse().ok()?;
                let keys = rest
                    .split(',')
                    .find_map(|kv| kv.strip_prefix("keys="))
                    .unwrap_or("0")
                    .to_string();
                Some((n, keys))
            })
            .collect();
        if !dbs.iter().any(|(n, _)| *n == self.db) {
            dbs.push((self.db, "0".into()));
        }
        dbs.sort();
        Ok(dbs
            .into_iter()
            .map(|(n, keys)| SchemaInfo {
                name: format!("db{n}"),
                is_current: n == self.db,
                is_system: false,
                owner: None,
                comment: Some(format!("{keys} keys")),
            })
            .collect())
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        if kind != ObjectKind::Table {
            return Ok(Vec::new());
        }
        let db = Self::db_of(schema.schema.as_deref()).unwrap_or(self.db);
        let keys = self.keys(db, None, MAX_KEYS).await?;
        Ok(keys
            .into_iter()
            .map(|(k, t)| {
                let mut o = ObjectInfo::new(
                    ObjectRef::new(ObjectKind::Table, Some(format!("db{db}")), k)
                        .with_signature(t.clone()),
                );
                o.detail = Some(t);
                o
            })
            .collect())
    }

    async fn list_children(
        &mut self,
        _parent: &ObjectRef,
        _kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }

    /// The columns its read command returns, by type.
    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let db = Self::db_of(table.schema.as_deref()).unwrap_or(self.db);
        let ty = match &table.signature {
            Some(t) => t.clone(),
            None => self.key_type(db, &table.name).await?,
        };
        let cols: &[(&str, &str)] = match ty.as_str() {
            "hash" => &[("field", "string"), ("value", "string")],
            "list" => &[("index", "integer"), ("value", "string")],
            "set" => &[("member", "string")],
            "zset" => &[("member", "string"), ("score", "double")],
            "stream" => &[("id", "string"), ("fields", "json")],
            _ => &[("value", "string")],
        };
        let mut t = TableDef::new(table.schema.clone(), table.name.clone());
        t.columns = cols
            .iter()
            .enumerate()
            .map(|(i, (n, ty))| ColumnDef {
                ordinal: i as u32 + 1,
                ..ColumnDef::new(*n, *ty)
            })
            .collect();
        if matches!(ty.as_str(), "hash" | "set" | "zset") {
            t.primary_key = Some(KeyDef {
                name: None,
                columns: vec![t.columns[0].name.clone()],
            });
        }
        t.options.insert("type".into(), ty);
        Ok(t)
    }

    /// Commands recreating the key (small keys), with its expiry.
    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let db = Self::db_of(obj.schema.as_deref()).unwrap_or(self.db);
        let mut conn = self.conn(db).await?;
        let key = obj.name.as_str();
        let k = arg(key);
        let ty = self.key_type(db, key).await?;
        let mut out = vec![format!("# {ty} key in db{db}")];
        let quoted = |v: &Reply| arg(&text_of(v).unwrap_or_default());
        match ty.as_str() {
            "string" => {
                let v: Reply = redis::cmd("GET")
                    .arg(key)
                    .query_async(&mut conn)
                    .await
                    .map_err(err)?;
                out.push(format!("SET {k} {}", quoted(&v)));
            }
            "hash" | "set" | "zset" | "list" => {
                let (len_cmd, read): (&str, &str) = match ty.as_str() {
                    "hash" => ("HLEN", "HGETALL"),
                    "set" => ("SCARD", "SMEMBERS"),
                    "zset" => ("ZCARD", "ZRANGE"),
                    _ => ("LLEN", "LRANGE"),
                };
                let len: i64 = redis::cmd(len_cmd)
                    .arg(key)
                    .query_async(&mut conn)
                    .await
                    .map_err(err)?;
                if len > 1000 {
                    out.push(format!("# {len} elements: too many to list"));
                } else {
                    let mut c = redis::cmd(read);
                    c.arg(key);
                    if matches!(read, "ZRANGE" | "LRANGE") {
                        c.arg(0).arg(-1);
                    }
                    if read == "ZRANGE" {
                        c.arg("WITHSCORES");
                    }
                    let v: Reply = c.query_async(&mut conn).await.map_err(err)?;
                    let Reply::Array(items) = v else {
                        return Ok(out.join("\n"));
                    };
                    match ty.as_str() {
                        "hash" => out.extend(items.chunks(2).map(|p| {
                            format!(
                                "HSET {k} {} {}",
                                quoted(&p[0]),
                                p.get(1).map(quoted).unwrap_or_default()
                            )
                        })),
                        "zset" => out.extend(items.chunks(2).map(|p| {
                            format!(
                                "ZADD {k} {} {}",
                                p.get(1).map(quoted).unwrap_or_default(),
                                quoted(&p[0])
                            )
                        })),
                        "set" => {
                            out.extend(items.iter().map(|m| format!("SADD {k} {}", quoted(m))))
                        }
                        _ => out.extend(items.iter().map(|m| format!("RPUSH {k} {}", quoted(m)))),
                    }
                }
            }
            other => out.push(format!("# Use DUMP / RESTORE to copy a {other} key")),
        }
        let ttl: i64 = redis::cmd("PTTL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(err)?;
        if ttl > 0 {
            out.push(format!("PEXPIRE {k} {ttl}"));
        }
        Ok(out.join("\n"))
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let db = Self::db_of(obj.schema.as_deref()).unwrap_or(self.db);
        let mut conn = self.conn(db).await?;
        let key = obj.name.as_str();
        let ty = self.key_type(db, key).await?;
        let len_cmd = match ty.as_str() {
            "hash" => "HLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "list" => "LLEN",
            "stream" => "XLEN",
            _ => "STRLEN",
        };
        let mut p = redis::pipe();
        p.cmd(len_cmd)
            .arg(key)
            .cmd("PTTL")
            .arg(key)
            .cmd("OBJECT")
            .arg("ENCODING")
            .arg(key)
            .cmd("MEMORY")
            .arg("USAGE")
            .arg(key);
        let replies: Vec<Reply> = p.query_async(&mut conn).await.unwrap_or_default();
        let get = |i: usize| replies.get(i).and_then(text_of).unwrap_or_default();
        let ttl = get(1).parse::<i64>().unwrap_or(-1);
        Ok(vec![
            Property::new("Type", ty),
            Property::new(
                if len_cmd == "STRLEN" {
                    "Length (bytes)"
                } else {
                    "Elements"
                },
                get(0),
            ),
            Property::new(
                "TTL",
                if ttl < 0 {
                    "none".into()
                } else {
                    format!("{:.3} s", ttl as f64 / 1000.0)
                },
            ),
            Property::new("Encoding", get(2)),
            Property::new("Memory (bytes)", get(3)),
        ])
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let raw = search.pattern.trim();
        if raw.is_empty() || !search.kinds.contains(&ObjectKind::Table) {
            return Ok(Vec::new());
        }
        let pattern = if raw.contains('*') || raw.contains('?') {
            raw.to_string()
        } else {
            format!("*{raw}*")
        };
        let mut out = Vec::new();
        for s in self.list_schemas().await? {
            let Some(db) = Self::db_of(Some(&s.name)) else {
                continue;
            };
            for (k, t) in self
                .keys(db, Some(&pattern), search.limit.saturating_sub(out.len()))
                .await?
            {
                let mut o = ObjectInfo::new(
                    ObjectRef::new(ObjectKind::Table, Some(s.name.clone()), k)
                        .with_signature(t.clone()),
                );
                o.detail = Some(t);
                out.push(o);
            }
            if out.len() >= search.limit {
                break;
            }
        }
        Ok(out)
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        _opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        Ok(SchemaSnapshot {
            kind: Some(DbKind::Redis),
            schema: schema.schema.clone(),
            ..Default::default()
        })
    }

    async fn explain(&mut self, _sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        Err(DbError::unsupported("Explain"))
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        let mut conn = self.conn(self.db).await?;
        let text: String = redis::cmd("CLIENT")
            .arg("LIST")
            .query_async(&mut conn)
            .await
            .map_err(err)?;
        let (cols, rows) = client_rows(&text);
        let mut data = TabularData::new(cols);
        data.rows = rows;
        let query_column = data.column_index("cmd");
        Ok(SessionList {
            id_column: data.column_index("id").unwrap_or(0),
            query_column,
            state_column: data.column_index("flags"),
            blocked_by_column: None,
            data,
        })
    }

    async fn kill_session(&mut self, id: &str, _mode: KillMode) -> DbResult<()> {
        let mut conn = self.conn(self.db).await?;
        redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(id)
            .query_async::<Reply>(&mut conn)
            .await
            .map(|_| ())
            .map_err(err)
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        let m = info_map(&self.info_text(None).await?);
        let num = |k: &str| m.get(k).and_then(|v| v.trim().parse::<f64>().ok());
        let mut out = Vec::new();
        let mut gauge = |group: &str, key: &str, label: &str, unit: &str| {
            if let Some(v) = num(key) {
                out.push(Metric::gauge(group, key, label, v, unit));
            }
        };
        gauge("Connections", "connected_clients", "Connected clients", "");
        gauge("Connections", "blocked_clients", "Blocked clients", "");
        gauge("Memory", "used_memory", "Used memory", "bytes");
        gauge("Memory", "used_memory_rss", "Resident memory", "bytes");
        gauge(
            "Commands",
            "instantaneous_ops_per_sec",
            "Operations per second",
            "",
        );
        let keys: f64 = m
            .iter()
            .filter(|(k, _)| k.starts_with("db"))
            .filter_map(|(_, v)| {
                v.split(',')
                    .find_map(|kv| kv.strip_prefix("keys="))?
                    .parse::<f64>()
                    .ok()
            })
            .sum();
        out.push(Metric::gauge("Keys", "keys", "Keys", keys, ""));
        for (key, label, group, unit) in [
            ("total_commands_processed", "Commands", "Commands", ""),
            ("keyspace_hits", "Key hits", "Keys", ""),
            ("keyspace_misses", "Key misses", "Keys", ""),
            ("expired_keys", "Expired keys", "Keys", ""),
            ("evicted_keys", "Evicted keys", "Keys", ""),
            ("total_net_input_bytes", "Network in", "Network", "bytes"),
            ("total_net_output_bytes", "Network out", "Network", "bytes"),
        ] {
            if let Some(v) = num(key) {
                out.push(Metric::counter(group, key, label, v, unit));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_become_rows() {
        let flat = Reply::Array(vec![
            Reply::BulkString(b"a".to_vec()),
            Reply::BulkString(b"1".to_vec()),
            Reply::BulkString(b"b".to_vec()),
            Reply::BulkString(b"2.5".to_vec()),
        ]);
        assert_eq!(
            pairs(&flat, false)[1],
            vec![Value::Text("b".into()), Value::Text("2.5".into())]
        );
        assert_eq!(pairs(&flat, true)[1][1], Value::Float(2.5));
        assert_eq!(
            value(&Reply::BulkString(vec![0xff, 0x00])),
            Value::Bytes(vec![0xff, 0x00].into())
        );
        let entry = Reply::Array(vec![Reply::Array(vec![
            Reply::BulkString(b"1-0".to_vec()),
            Reply::Array(vec![
                Reply::BulkString(b"t".to_vec()),
                Reply::BulkString(b"21.5".to_vec()),
            ]),
        ])]);
        assert_eq!(
            stream_rows(&entry),
            vec![vec![
                Value::Text("1-0".into()),
                Value::Json("{\"t\":\"21.5\"}".into())
            ]]
        );
        let info = info_rows("# Server\r\nredis_version:7.2.4\r\nuptime_in_seconds:42\r\n");
        assert_eq!(
            info[1],
            vec![
                Value::Text("Server".into()),
                Value::Text("uptime_in_seconds".into()),
                Value::Int(42)
            ]
        );
        let (cols, rows) = client_rows("id=3 addr=127.0.0.1:5 name= db=0 cmd=client|list\n");
        assert_eq!(cols, ["id", "addr", "name", "db", "cmd"]);
        assert_eq!(rows[0][0], Value::Int(3));
    }
}
