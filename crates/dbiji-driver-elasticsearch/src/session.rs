//! [`SearchSession`]: Elasticsearch and OpenSearch over HTTP(S). SQL goes to
//! the SQL endpoint and pages with its cursor; REST requests are passed
//! through and their hits shown as rows. The blocking HTTP client runs on
//! blocking threads.

use crate::dialect::{SearchDialect, is_rest};
use async_trait::async_trait;
use base64::Engine;
use dbiji_core::admin::{KillMode, Metric, SessionList, TabularData};
use dbiji_core::config::SslMode;
use dbiji_core::ddl::{ColumnDef, SchemaSnapshot, SnapshotOptions, TableDef};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::value::{ColumnMeta, DataKind};
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor,
    ResultHeader, Row, ServerMessage, Session, TransactionMode, Value,
};
use serde_json::{Value as J, json};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use ureq::Agent;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

/// Rows per SQL page.
const FETCH: u64 = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Product {
    Elasticsearch,
    OpenSearch,
}

/// The HTTP client of a session (cheap to clone, shared by its cursors).
#[derive(Clone)]
struct Http {
    agent: Agent,
    base: String,
    auth: Option<String>,
}

impl Http {
    fn new(params: &ConnectParams) -> DbResult<Self> {
        let https = matches!(
            params.ssl.mode,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
        ) || params.property("https") == Some("true");
        let mut cfg = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(params.connect_timeout))
            .timeout_resolve(Some(params.connect_timeout))
            .timeout_global(Some(Duration::from_secs(600)))
            .user_agent(format!(
                "{} (Elasticsearch driver)",
                params.application_name
            ));
        if https {
            let mut b = TlsConfig::builder()
                .provider(TlsProvider::Rustls)
                .unversioned_rustls_crypto_provider(Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ));
            if params.ssl.mode.verifies_chain() {
                let roots: Vec<Certificate<'static>> = rustls_native_certs::load_native_certs()
                    .certs
                    .into_iter()
                    .map(|c| Certificate::from_der(c.as_ref()).to_owned())
                    .collect();
                b = if roots.is_empty() {
                    b.root_certs(RootCerts::WebPki)
                } else {
                    b.root_certs(RootCerts::new_with_certs(&roots))
                };
            } else {
                b = b.disable_verification(true);
            }
            cfg = cfg.tls_config(b.build());
        }
        let host = params.tls_host();
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let path = params
            .property("path")
            .unwrap_or("")
            .trim()
            .trim_end_matches('/')
            .to_string();
        let base = format!(
            "{}://{host}:{}{}",
            if https { "https" } else { "http" },
            params.port,
            path
        );
        let auth = if let Some(key) = params.property("api_key").filter(|k| !k.is_empty()) {
            Some(format!("ApiKey {key}"))
        } else {
            params.user.as_deref().filter(|u| !u.is_empty()).map(|u| {
                let pw = params.password.as_ref().map(|p| p.expose()).unwrap_or("");
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(format!("{u}:{pw}"))
                )
            })
        };
        Ok(Http {
            agent: Agent::new_with_config(cfg.build()),
            base,
            auth,
        })
    }

    /// One request; the JSON reply (or `{"text": …}`), errors as DbError.
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> DbResult<J> {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let url = format!("{}{path}", self.base);
        let ndjson = path
            .split('?')
            .next()
            .is_some_and(|p| p.ends_with("_bulk") || p.ends_with("_msearch"));
        let ctype = if ndjson {
            "application/x-ndjson"
        } else {
            "application/json"
        };
        let body_text = body.map(|b| {
            if ndjson && !b.ends_with('\n') {
                format!("{b}\n")
            } else {
                b.to_string()
            }
        });
        macro_rules! go {
            ($rb:expr) => {{
                let mut rb = $rb.header("Accept", "application/json");
                if let Some(a) = &self.auth {
                    rb = rb.header("Authorization", a);
                }
                rb
            }};
        }
        let result = match (method, body_text) {
            ("GET", None) => go!(self.agent.get(&url)).call(),
            ("HEAD", _) => go!(self.agent.head(&url)).call(),
            ("DELETE", None) => go!(self.agent.delete(&url)).call(),
            ("GET", Some(b)) => go!(self.agent.get(&url))
                .header("Content-Type", ctype)
                .force_send_body()
                .send(b.as_bytes()),
            ("DELETE", Some(b)) => go!(self.agent.delete(&url))
                .header("Content-Type", ctype)
                .force_send_body()
                .send(b.as_bytes()),
            ("PUT", b) => go!(self.agent.put(&url))
                .header("Content-Type", ctype)
                .send(b.unwrap_or_default().as_bytes()),
            (_, b) => go!(self.agent.post(&url))
                .header("Content-Type", ctype)
                .send(b.unwrap_or_default().as_bytes()),
        };
        let response = result.map_err(|e| match e {
            ureq::Error::Timeout(_) => DbError::timeout(e.to_string()),
            ureq::Error::Tls(_) | ureq::Error::Rustls(_) => DbError::tls(e.to_string()),
            _ => DbError::connection(format!("{}: {e}", self.base)),
        })?;
        let status = response.status().as_u16();
        let mut text = String::new();
        response
            .into_body()
            .into_reader()
            .read_to_string(&mut text)
            .map_err(|e| DbError::connection(e.to_string()))?;
        let json: J = if text.trim().is_empty() {
            json!({"status": status})
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"text": text}))
        };
        if status >= 400 {
            let reason = json
                .pointer("/error/root_cause/0/reason")
                .or_else(|| json.pointer("/error/reason"))
                .or_else(|| json.pointer("/error/details"))
                .or_else(|| json.get("error"))
                .map(|r| {
                    r.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| r.to_string())
                })
                .unwrap_or_else(|| text.chars().take(500).collect());
            let kind = match status {
                401 | 403 => ErrorKind::Auth,
                400 if reason.contains("parsing_exception")
                    || reason.to_ascii_lowercase().contains("syntax")
                    || reason.contains("line ") =>
                {
                    ErrorKind::Syntax
                }
                _ => ErrorKind::Query,
            };
            let etype = json
                .pointer("/error/type")
                .and_then(J::as_str)
                .map(str::to_string);
            let mut e = DbError::new(kind, reason).with_code(status.to_string());
            if let Some(t) = etype {
                e = e.with_detail(t);
            }
            return Err(e);
        }
        Ok(json)
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> DbResult<T> + Send + 'static,
) -> DbResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DbError::internal(e.to_string()))?
}

// ------------------------------------------------------------- conversion

fn kind_of(es_type: &str) -> DataKind {
    match es_type {
        "long" | "integer" | "short" | "byte" | "token_count" => DataKind::Int,
        "double" | "float" | "half_float" | "scaled_float" => DataKind::Float,
        "unsigned_long" => DataKind::Decimal,
        "boolean" => DataKind::Bool,
        "date" | "datetime" | "date_nanos" | "timestamp" => DataKind::DateTimeTz,
        "ip" => DataKind::Network,
        "object" | "nested" | "geo_point" | "geo_shape" | "flattened" | "struct" | "array" => {
            DataKind::Json
        }
        "binary" => DataKind::Binary,
        _ => DataKind::Text,
    }
}

fn value(v: &J, kind: DataKind) -> Value {
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_u64().map(|u| Value::Decimal(u.to_string().into())))
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        J::String(s) if kind == DataKind::DateTimeTz => chrono::DateTime::parse_from_rfc3339(s)
            .map(Value::DateTimeTz)
            .unwrap_or_else(|_| Value::Text(s.as_str().into())),
        J::String(s) => Value::Text(s.as_str().into()),
        other => Value::Json(other.to_string().into()),
    }
}

/// Objects as rows: their keys in order of appearance are the columns.
fn object_rows(objs: &[&J]) -> (Vec<ColumnMeta>, Vec<Row>) {
    let mut cols: Vec<(String, DataKind)> = Vec::new();
    for o in objs {
        if let J::Object(m) = o {
            for (k, v) in m {
                if !cols.iter().any(|(c, _)| c == k) {
                    let kind = match v {
                        J::Number(n) if n.is_i64() => DataKind::Int,
                        J::Number(_) => DataKind::Float,
                        J::Bool(_) => DataKind::Bool,
                        J::Object(_) | J::Array(_) => DataKind::Json,
                        _ => DataKind::Text,
                    };
                    cols.push((k.clone(), kind));
                }
            }
        }
    }
    let rows = objs
        .iter()
        .map(|o| {
            Row::from(
                cols.iter()
                    .map(|(c, k)| o.get(c).map_or(Value::Null, |v| value(v, *k)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    (
        cols.into_iter()
            .map(|(n, k)| ColumnMeta::new(n, "", k))
            .collect(),
        rows,
    )
}

/// A REST reply as rows: its hits (`_index`, `_id`, `_score` and the source
/// fields), the objects of an array, or (key, value) pairs.
fn rest_rows(reply: &J) -> (Vec<ColumnMeta>, Vec<Row>) {
    if let Some(hits) = reply.pointer("/hits/hits").and_then(J::as_array)
        && (!hits.is_empty() || reply.get("aggregations").is_none())
    {
        let flat: Vec<J> = hits
            .iter()
            .map(|h| {
                let mut m = serde_json::Map::new();
                for k in ["_index", "_id", "_score"] {
                    m.insert(k.into(), h.get(k).cloned().unwrap_or(J::Null));
                }
                if let Some(J::Object(src)) = h.get("_source") {
                    for (k, v) in src {
                        m.insert(k.clone(), v.clone());
                    }
                }
                J::Object(m)
            })
            .collect();
        return object_rows(&flat.iter().collect::<Vec<_>>());
    }
    if let J::Array(items) = reply
        && items.iter().all(J::is_object)
    {
        return object_rows(&items.iter().collect::<Vec<_>>());
    }
    if let Some(text) = reply.get("text").and_then(J::as_str) {
        let rows = text
            .lines()
            .map(|l| Row::from(vec![Value::Text(l.into())]))
            .collect();
        return (vec![ColumnMeta::new("response", "", DataKind::Text)], rows);
    }
    let rows = match reply {
        J::Object(m) => m
            .iter()
            .map(|(k, v)| {
                Row::from(vec![
                    Value::Text(k.as_str().into()),
                    value(v, DataKind::Other),
                ])
            })
            .collect(),
        other => vec![Row::from(vec![
            Value::Text("response".into()),
            value(other, DataKind::Other),
        ])],
    };
    (
        vec![
            ColumnMeta::new("key", "", DataKind::Text),
            ColumnMeta::new("value", "", DataKind::Other),
        ],
        rows,
    )
}

/// Mapping properties flattened to `a.b.c` paths with their types.
fn mapping_fields(props: &J, prefix: &str, out: &mut Vec<(String, String)>) {
    let Some(m) = props.as_object() else { return };
    for (name, def) in m {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let ty =
            def.get("type")
                .and_then(J::as_str)
                .unwrap_or(if def.get("properties").is_some() {
                    "object"
                } else {
                    "unknown"
                });
        out.push((path.clone(), ty.to_string()));
        if let Some(p) = def.get("properties") {
            mapping_fields(p, &path, out);
        }
    }
}

struct SqlPager {
    http: Http,
    product: Product,
    cursor: Option<String>,
    kinds: Vec<DataKind>,
}

impl SqlPager {
    fn rows_of(&self, reply: &J) -> Vec<Row> {
        let rows = reply
            .get("rows")
            .or_else(|| reply.get("datarows"))
            .and_then(J::as_array)
            .cloned()
            .unwrap_or_default();
        rows.iter()
            .map(|r| {
                Row::from(
                    r.as_array()
                        .map(|a| {
                            a.iter()
                                .enumerate()
                                .map(|(i, v)| {
                                    value(v, self.kinds.get(i).copied().unwrap_or(DataKind::Other))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
            })
            .collect()
    }
    fn endpoint(&self) -> &'static str {
        match self.product {
            Product::Elasticsearch => "/_sql?format=json",
            Product::OpenSearch => "/_plugins/_sql?format=jdbc",
        }
    }
    fn close_endpoint(&self) -> &'static str {
        match self.product {
            Product::Elasticsearch => "/_sql/close",
            Product::OpenSearch => "/_plugins/_sql/close",
        }
    }
}

struct SearchCursor {
    header: Option<ResultHeader>,
    rows: VecDeque<Row>,
    pager: Option<SqlPager>,
}

#[async_trait]
impl QueryCursor for SearchCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        Ok(self.header.take())
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let want = max_rows.max(1);
        while self.rows.len() < want {
            let Some(pager) = self.pager.as_mut() else {
                break;
            };
            let Some(cursor) = pager.cursor.take() else {
                self.pager = None;
                break;
            };
            let http = pager.http.clone();
            let endpoint = pager.endpoint();
            let reply = blocking(move || {
                http.call(
                    "POST",
                    endpoint,
                    Some(&json!({"cursor": cursor}).to_string()),
                )
            })
            .await?;
            let rows = pager.rows_of(&reply);
            pager.cursor = reply
                .get("cursor")
                .and_then(J::as_str)
                .map(str::to_string)
                .filter(|c| !c.is_empty() && !rows.is_empty());
            self.rows.extend(rows);
        }
        let n = want.min(self.rows.len());
        Ok(self.rows.drain(..n).collect())
    }

    async fn close(&mut self) -> DbResult<()> {
        self.rows.clear();
        if let Some(p) = self.pager.take()
            && let Some(c) = p.cursor.clone()
        {
            let endpoint = p.close_endpoint();
            let http = p.http.clone();
            let _ = blocking(move || {
                http.call("POST", endpoint, Some(&json!({"cursor": c}).to_string()))
            })
            .await;
        }
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

pub struct SearchSession {
    http: Http,
    product: Product,
    cluster: String,
    info: ServerInfo,
    dialect: Arc<SearchDialect>,
    mode: TransactionMode,
    closed: bool,
}

/// `METHOD /path` and the JSON body after it.
fn split_request(text: &str) -> (String, String, Option<String>) {
    let text = text.trim();
    let (first, body) = match text.split_once('\n') {
        Some((f, b)) => (
            f.trim(),
            Some(b.trim().to_string()).filter(|b| !b.is_empty()),
        ),
        None => (text, None),
    };
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    // A body on the request line: `POST /i/_search {"query": …}`.
    let rest: String = parts.collect::<Vec<_>>().join(" ");
    let body = if rest.is_empty() {
        body
    } else {
        Some(match body {
            Some(b) => format!("{rest}\n{b}"),
            None => rest,
        })
    };
    (method, path, body)
}

impl SearchSession {
    pub async fn connect(params: &ConnectParams) -> DbResult<Self> {
        let http = Http::new(params)?;
        let h = http.clone();
        let root = blocking(move || h.call("GET", "/", None)).await?;
        let version = root
            .pointer("/version/number")
            .and_then(J::as_str)
            .unwrap_or_default()
            .to_string();
        let product =
            if root.pointer("/version/distribution").and_then(J::as_str) == Some("opensearch") {
                Product::OpenSearch
            } else {
                Product::Elasticsearch
            };
        let cluster = root
            .get("cluster_name")
            .and_then(J::as_str)
            .unwrap_or("cluster")
            .to_string();
        let name = if product == Product::OpenSearch {
            "OpenSearch"
        } else {
            "Elasticsearch"
        };
        let info = ServerInfo {
            product: name.into(),
            version_number: parse_version_number(&version),
            version,
            session_id: None,
        };
        Ok(SearchSession {
            http,
            product,
            cluster,
            info,
            dialect: Arc::new(SearchDialect::new()),
            mode: TransactionMode::Auto,
            closed: false,
        })
    }

    async fn call(&self, method: &str, path: &str, body: Option<String>) -> DbResult<J> {
        let http = self.http.clone();
        let (method, path) = (method.to_string(), path.to_string());
        blocking(move || http.call(&method, &path, body.as_deref())).await
    }

    async fn sql(&self, sql: &str, params: &[Value]) -> DbResult<Box<dyn QueryCursor>> {
        let mut body = json!({"query": sql, "fetch_size": FETCH});
        if !params.is_empty() && self.product == Product::Elasticsearch {
            body["params"] = J::Array(
                params
                    .iter()
                    .map(|v| match v {
                        Value::Null => J::Null,
                        Value::Bool(b) => J::Bool(*b),
                        Value::Int(i) => json!(i),
                        Value::Float(f) => json!(f),
                        other => J::String(other.to_text().into_owned()),
                    })
                    .collect(),
            );
        }
        let pager = SqlPager {
            http: self.http.clone(),
            product: self.product,
            cursor: None,
            kinds: Vec::new(),
        };
        let reply = self
            .call("POST", pager.endpoint(), Some(body.to_string()))
            .await?;
        let cols = reply
            .get("columns")
            .or_else(|| reply.get("schema"))
            .and_then(J::as_array)
            .cloned()
            .unwrap_or_default();
        let columns: Vec<ColumnMeta> = cols
            .iter()
            .map(|c| {
                let name = c
                    .get("alias")
                    .or_else(|| c.get("name"))
                    .and_then(J::as_str)
                    .unwrap_or("?");
                let ty = c.get("type").and_then(J::as_str).unwrap_or("");
                ColumnMeta::new(name, ty, kind_of(ty))
            })
            .collect();
        let mut pager = SqlPager {
            kinds: columns.iter().map(|c| c.kind).collect(),
            ..pager
        };
        let rows = pager.rows_of(&reply);
        pager.cursor = reply
            .get("cursor")
            .and_then(J::as_str)
            .map(str::to_string)
            .filter(|c| !c.is_empty());
        Ok(Box::new(SearchCursor {
            header: Some(ResultHeader::Rows(columns)),
            rows: rows.into(),
            pager: Some(pager),
        }))
    }

    async fn indices(&self) -> DbResult<Vec<J>> {
        let r = self.call("GET", "/_cat/indices?format=json&bytes=b&s=index&h=index,health,status,docs.count,store.size,pri,rep", None).await?;
        Ok(r.as_array().cloned().unwrap_or_default())
    }

    async fn mapping(&self, index: &str) -> DbResult<Vec<(String, String)>> {
        let r = self
            .call("GET", &format!("/{index}/_mapping"), None)
            .await?;
        let mut out = Vec::new();
        if let Some(m) = r.as_object() {
            for (_, def) in m.iter().take(1) {
                if let Some(p) = def.pointer("/mappings/properties") {
                    mapping_fields(p, "", &mut out);
                }
            }
        }
        Ok(out)
    }
}

fn num(v: Option<&J>) -> Option<i64> {
    v.and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

#[async_trait]
impl Session for SearchSession {
    fn kind(&self) -> DbKind {
        DbKind::Elasticsearch
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
        self.call("GET", "/", None)
            .await
            .map(|_| ())
            .map_err(|e| DbError::connection(e.message))
    }

    async fn close(&mut self) -> DbResult<()> {
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
        let text = sql.trim().trim_end_matches(';').trim();
        if !is_rest(text) {
            return self.sql(text, params).await;
        }
        let (method, path, body) = split_request(text);
        let reply = self.call(&method, &path, body).await?;
        let write = matches!(method.as_str(), "PUT" | "DELETE")
            || method == "POST"
                && !path.contains("_search")
                && !path.contains("_count")
                && !path.contains("_sql")
                && !path.contains("_msearch");
        if write && reply.pointer("/hits/hits").is_none() {
            let n = num(reply.get("deleted"))
                .or_else(|| num(reply.get("updated")))
                .or_else(|| {
                    num(reply.pointer("/items")).or_else(|| {
                        reply
                            .get("items")
                            .and_then(J::as_array)
                            .map(|a| a.len() as i64)
                    })
                });
            let result = reply.get("result").and_then(J::as_str).map(str::to_string);
            let tag = match result {
                Some(r) => format!("{method} {path} ({r})"),
                None => format!("{method} {path}"),
            };
            return Ok(Box::new(SearchCursor {
                header: Some(ResultHeader::Update {
                    rows_affected: n.map(|n| n.max(0) as u64),
                    last_insert_id: None,
                    tag: Some(tag),
                }),
                rows: VecDeque::new(),
                pager: None,
            }));
        }
        let (cols, rows) = rest_rows(&reply);
        Ok(Box::new(SearchCursor {
            header: Some(ResultHeader::Rows(cols)),
            rows: rows.into(),
            pager: None,
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
            "Rolling back (every request is applied at once)",
        ))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(None)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.cluster.clone()))
    }

    async fn set_current_schema(&mut self, _schema: &str) -> DbResult<()> {
        Ok(())
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(Vec::new())
    }

    /// The cluster (the navigator shows its indices directly).
    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        Ok(vec![SchemaInfo {
            name: self.cluster.clone(),
            is_current: true,
            is_system: false,
            owner: None,
            comment: None,
        }])
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let s = schema.schema.clone().or(Some(self.cluster.clone()));
        match kind {
            ObjectKind::Table => Ok(self
                .indices()
                .await?
                .iter()
                .filter_map(|i| {
                    let name = i.get("index")?.as_str()?.to_string();
                    let mut o =
                        ObjectInfo::new(ObjectRef::new(ObjectKind::Table, s.clone(), name.clone()));
                    o.is_system = name.starts_with('.');
                    o.row_estimate = num(i.get("docs.count"));
                    o.size_bytes = num(i.get("store.size"));
                    o.detail = i.get("health").and_then(J::as_str).map(str::to_string);
                    Some(o)
                })
                .collect()),
            ObjectKind::View => {
                let r = self
                    .call(
                        "GET",
                        "/_cat/aliases?format=json&h=alias,index&s=alias",
                        None,
                    )
                    .await?;
                let mut out: Vec<ObjectInfo> = Vec::new();
                for a in r.as_array().cloned().unwrap_or_default() {
                    let (Some(alias), Some(index)) = (
                        a.get("alias").and_then(J::as_str),
                        a.get("index").and_then(J::as_str),
                    ) else {
                        continue;
                    };
                    match out.iter_mut().find(|o| o.obj.name == alias) {
                        Some(o) => {
                            o.detail =
                                Some(format!("{}, {index}", o.detail.clone().unwrap_or_default()))
                        }
                        None => {
                            let mut o =
                                ObjectInfo::new(ObjectRef::new(ObjectKind::View, s.clone(), alias));
                            o.is_system = alias.starts_with('.');
                            o.detail = Some(index.to_string());
                            out.push(o);
                        }
                    }
                }
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        if kind != ObjectKind::Column {
            return Ok(Vec::new());
        }
        Ok(self
            .mapping(&parent.name)
            .await?
            .into_iter()
            .map(|(name, ty)| {
                ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, name)).with_detail(ty)
            })
            .collect())
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let mut t = TableDef::new(table.schema.clone(), table.name.clone());
        t.columns = self
            .mapping(&table.name)
            .await?
            .into_iter()
            .filter(|(_, ty)| ty != "object")
            .enumerate()
            .map(|(i, (name, ty))| ColumnDef {
                ordinal: i as u32 + 1,
                kind: kind_of(&ty),
                ..ColumnDef::new(name, ty)
            })
            .collect();
        Ok(t)
    }

    /// `PUT /index` with its settings and mappings.
    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let r = self.call("GET", &format!("/{}", obj.name), None).await?;
        let Some((name, def)) = r.as_object().and_then(|m| m.iter().next()) else {
            return Err(DbError::query(format!("Index {} not found", obj.name)));
        };
        let mut settings = serde_json::Map::new();
        if let Some(idx) = def.pointer("/settings/index") {
            for k in [
                "number_of_shards",
                "number_of_replicas",
                "refresh_interval",
                "analysis",
            ] {
                if let Some(v) = idx.get(k) {
                    settings.insert(k.into(), v.clone());
                }
            }
        }
        let mut body = json!({"settings": {"index": settings}, "mappings": def.get("mappings").cloned().unwrap_or(J::Null)});
        if let Some(a) = def
            .get("aliases")
            .filter(|a| a.as_object().is_some_and(|m| !m.is_empty()))
        {
            body["aliases"] = a.clone();
        }
        Ok(format!(
            "PUT /{name}\n{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        ))
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let r = self
            .call(
                "GET",
                &format!("/_cat/indices/{}?format=json&bytes=b", obj.name),
                None,
            )
            .await?;
        let Some(i) = r.as_array().and_then(|a| a.first()).cloned() else {
            return Ok(Vec::new());
        };
        let get = |k: &str| {
            i.get(k)
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default()
        };
        Ok(vec![
            Property::new("Health", get("health")),
            Property::new("Status", get("status")),
            Property::new("Documents", get("docs.count")),
            Property::new("Deleted documents", get("docs.deleted")),
            Property::new("Size (bytes)", get("store.size")),
            Property::new("Primary shards", get("pri")),
            Property::new("Replicas", get("rep")),
        ])
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let needle = search.pattern.trim().trim_matches('*').to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = self
            .list_objects(&SchemaRef::schema(self.cluster.clone()), ObjectKind::Table)
            .await?;
        out.retain(|o| !o.is_system && o.obj.name.to_lowercase().contains(&needle));
        out.truncate(search.limit);
        Ok(out)
    }

    /// Indices with their fields, from one `GET /_mapping`.
    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let mut tables = Vec::new();
        if opts.columns {
            let r = self.call("GET", "/_mapping", None).await?;
            for (index, def) in r.as_object().cloned().unwrap_or_default() {
                if index.starts_with('.') {
                    continue;
                }
                let mut fields = Vec::new();
                if let Some(p) = def.pointer("/mappings/properties") {
                    mapping_fields(p, "", &mut fields);
                }
                let mut t = TableDef::new(schema.schema.clone(), index);
                t.columns = fields
                    .into_iter()
                    .filter(|(_, ty)| ty != "object")
                    .enumerate()
                    .map(|(i, (n, ty))| ColumnDef {
                        ordinal: i as u32 + 1,
                        kind: kind_of(&ty),
                        ..ColumnDef::new(n, ty)
                    })
                    .collect();
                tables.push(t);
            }
        }
        Ok(SchemaSnapshot {
            kind: Some(DbKind::Elasticsearch),
            schema: schema.schema.clone(),
            tables,
            ..Default::default()
        })
    }

    /// The query DSL the SQL becomes.
    async fn explain(&mut self, sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        let text = sql.trim().trim_end_matches(';');
        if is_rest(text) {
            return Err(DbError::unsupported(
                "Explain works on SQL; add \"profile\": true to a search request",
            ));
        }
        let path = match self.product {
            Product::Elasticsearch => "/_sql/translate",
            Product::OpenSearch => "/_plugins/_sql/_explain",
        };
        let r = self
            .call("POST", path, Some(json!({"query": text}).to_string()))
            .await?;
        let mut root = PlanNode::new("Query DSL");
        root.condition = r.get("query").map(|q| q.to_string());
        Ok(QueryPlan {
            roots: vec![root],
            text: serde_json::to_string_pretty(&r).unwrap_or_default(),
            raw: Some(r.to_string()),
            planning_ms: None,
            execution_ms: None,
            analyzed: false,
        })
    }

    /// Running search and write tasks.
    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        let r = self
            .call("GET", "/_tasks?detailed=true&group_by=none", None)
            .await?;
        let mut data = TabularData::new(
            [
                "id",
                "node",
                "action",
                "running (ms)",
                "cancellable",
                "description",
            ]
            .map(String::from)
            .to_vec(),
        );
        for t in r
            .get("tasks")
            .and_then(J::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let id = format!(
                "{}:{}",
                t.get("node").and_then(J::as_str).unwrap_or(""),
                num(t.get("id")).unwrap_or(0)
            );
            data.rows.push(vec![
                Value::Text(id.into()),
                Value::Text(t.get("node").and_then(J::as_str).unwrap_or("").into()),
                Value::Text(t.get("action").and_then(J::as_str).unwrap_or("").into()),
                num(t.get("running_time_in_nanos"))
                    .map_or(Value::Null, |n| Value::Int(n / 1_000_000)),
                t.get("cancellable")
                    .and_then(J::as_bool)
                    .map_or(Value::Null, Value::Bool),
                Value::Text(
                    t.get("description")
                        .and_then(J::as_str)
                        .unwrap_or("")
                        .into(),
                ),
            ]);
        }
        Ok(SessionList {
            data,
            id_column: 0,
            query_column: Some(5),
            state_column: None,
            blocked_by_column: None,
        })
    }

    async fn kill_session(&mut self, id: &str, _mode: KillMode) -> DbResult<()> {
        self.call("POST", &format!("/_tasks/{id}/_cancel"), None)
            .await
            .map(|_| ())
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        let health = self.call("GET", "/_cluster/health", None).await?;
        let stats = self.call("GET", "/_nodes/stats/jvm,indices", None).await?;
        let mut out = Vec::new();
        let status = match health.get("status").and_then(J::as_str) {
            Some("green") => 2.0,
            Some("yellow") => 1.0,
            _ => 0.0,
        };
        out.push(Metric::gauge(
            "Cluster",
            "status",
            "Status (2 green, 1 yellow, 0 red)",
            status,
            "",
        ));
        for (k, label) in [
            ("number_of_nodes", "Nodes"),
            ("active_shards", "Active shards"),
            ("unassigned_shards", "Unassigned shards"),
        ] {
            if let Some(v) = num(health.get(k)) {
                out.push(Metric::gauge("Cluster", k, label, v as f64, ""));
            }
        }
        let nodes = stats
            .get("nodes")
            .and_then(J::as_object)
            .cloned()
            .unwrap_or_default();
        let sum = |p: &str| -> f64 {
            nodes
                .values()
                .filter_map(|n| n.pointer(p).and_then(J::as_f64))
                .sum()
        };
        out.push(Metric::gauge(
            "Data",
            "docs",
            "Documents",
            sum("/indices/docs/count"),
            "",
        ));
        out.push(Metric::gauge(
            "Data",
            "store",
            "Store size",
            sum("/indices/store/size_in_bytes"),
            "bytes",
        ));
        out.push(Metric::gauge(
            "JVM",
            "heap",
            "Heap used",
            sum("/jvm/mem/heap_used_in_bytes"),
            "bytes",
        ));
        out.push(Metric::counter(
            "Operations",
            "search",
            "Searches",
            sum("/indices/search/query_total"),
            "",
        ));
        out.push(Metric::counter(
            "Operations",
            "index",
            "Indexed documents",
            sum("/indices/indexing/index_total"),
            "",
        ));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_requests_and_replies() {
        assert_eq!(
            split_request("GET /logs/_search\n{\"size\": 1}"),
            (
                "GET".into(),
                "/logs/_search".into(),
                Some("{\"size\": 1}".into())
            )
        );
        assert_eq!(
            split_request("delete /old"),
            ("DELETE".into(), "/old".into(), None)
        );
        let reply = json!({"hits": {"hits": [{"_index": "i", "_id": "1", "_score": 1.0, "_source": {"msg": "hi", "n": 3, "tags": ["a"]}}]}});
        let (cols, rows) = rest_rows(&reply);
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["_index", "_id", "_score", "msg", "n", "tags"]
        );
        assert_eq!(rows[0].to_vec()[4], Value::Int(3));
        let (cols, _) = rest_rows(&json!([{"index": "a", "health": "green"}]));
        assert_eq!(cols.len(), 2);
        let mut f = Vec::new();
        mapping_fields(
            &json!({"user": {"properties": {"name": {"type": "keyword"}}}, "at": {"type": "date"}}),
            "",
            &mut f,
        );
        assert_eq!(
            f,
            vec![
                ("user".to_string(), "object".to_string()),
                ("user.name".into(), "keyword".into()),
                ("at".into(), "date".into())
            ]
        );
    }
}
