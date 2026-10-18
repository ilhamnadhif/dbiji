//! Blocking HTTP(S) transport to the ClickHouse HTTP interface, built on
//! `ureq`. All calls block and must run on a dedicated thread (see
//! [`crate::cursor`]) or inside `spawn_blocking`.

use crate::error::{exception_to_error, transport_error};
use dbiji_core::config::SslMode;
use dbiji_core::{ConnectParams, DbError, DbResult, Secret};
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ureq::Agent;
use ureq::config::Config;
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// Output format requested for every query without an explicit `FORMAT`.
pub const ROW_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

/// Default `session_timeout` (seconds) requested for HTTP sessions.
pub const DEFAULT_SESSION_TIMEOUT: u32 = 3600;

/// Maximum number of bytes read from an error response body.
const MAX_ERROR_BODY: u64 = 1024 * 1024;

/// Static per-session HTTP configuration.
pub struct Transport {
    agent: Agent,
    /// `http(s)://host:port/` (path from the `path` property appended).
    pub base_url: String,
    /// Same URL for messages (no credentials).
    pub display_url: String,
    user: Option<String>,
    password: Option<Secret>,
    /// Session id for SET / USE / temporary tables.
    pub session_id: String,
    /// Requested session timeout in seconds.
    pub session_timeout: AtomicU32,
    /// Settings sent with every query (`key=value`), empty in minimal mode.
    settings: Mutex<Vec<(String, String)>>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport").field("url", &self.display_url).field("user", &self.user).field("session_id", &self.session_id).finish_non_exhaustive()
    }
}

/// One query request.
#[derive(Debug, Clone, Default)]
pub struct QueryRequest {
    /// SQL text, sent as the POST body.
    pub sql: String,
    /// `query_id` for cancellation via `KILL QUERY`.
    pub query_id: String,
    /// Current database (`database` parameter).
    pub database: Option<String>,
    /// Attach the HTTP session (`session_id`).
    pub use_session: bool,
    /// Extra settings for this query.
    pub extra_settings: Vec<(String, String)>,
    /// Skip the per-connection settings (used while probing).
    pub skip_default_settings: bool,
}

/// Interesting response headers.
#[derive(Debug, Clone, Default)]
pub struct ResponseHead {
    /// HTTP status code.
    pub status: u16,
    /// `X-ClickHouse-Format`: present when the statement produced a result set.
    pub format: Option<String>,
    /// `X-ClickHouse-Summary` JSON.
    pub summary: Option<Summary>,
    /// `X-ClickHouse-Exception-Code`.
    pub exception_code: Option<String>,
    /// `X-ClickHouse-Exception-Tag` (mid-stream exception marker).
    pub exception_tag: Option<String>,
    /// `X-ClickHouse-Query-Id`.
    pub query_id: Option<String>,
    /// `X-ClickHouse-Timezone`.
    pub timezone: Option<String>,
}

/// Parsed `X-ClickHouse-Summary` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Rows read.
    pub read_rows: u64,
    /// Rows written (INSERT).
    pub written_rows: u64,
    /// Rows in the result.
    pub result_rows: u64,
    /// Elapsed nanoseconds at the time the header was sent.
    pub elapsed_ns: u64,
}

impl Summary {
    /// Parse the header value (numbers are JSON strings).
    pub fn parse(text: &str) -> Option<Summary> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let num = |k: &str| -> u64 {
            match v.get(k) {
                Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
                Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
                _ => 0,
            }
        };
        Some(Summary { read_rows: num("read_rows"), written_rows: num("written_rows"), result_rows: num("result_rows"), elapsed_ns: num("elapsed_ns") })
    }
}

/// Response body reader (owned, `'static`).
pub type BodyReader = ureq::BodyReader<'static>;

/// Resolver sending every lookup to a fixed address (SSH tunnels: the URL
/// carries the real server name for TLS while the socket goes to the
/// local forwarder).
#[derive(Debug)]
struct FixedResolver {
    addrs: Vec<SocketAddr>,
}

impl Resolver for FixedResolver {
    fn resolve(&self, _uri: &ureq::http::Uri, _config: &Config, _timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut out = self.empty();
        for a in self.addrs.iter().take(16) {
            out.push(*a);
        }
        if out.is_empty() { Err(ureq::Error::HostNotFound) } else { Ok(out) }
    }
}

fn read_file(path: &str, what: &str) -> DbResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| DbError::config(format!("Cannot read {what} file '{path}': {e}")))
}

fn load_pem_certs(path: &str) -> DbResult<Vec<Certificate<'static>>> {
    let pem = read_file(path, "CA certificate")?;
    let mut out = Vec::new();
    for item in ureq::tls::parse_pem(&pem) {
        match item {
            Ok(PemItem::Certificate(c)) => out.push(c),
            Ok(_) => {}
            Err(e) => return Err(DbError::config(format!("Invalid PEM in '{path}': {e}"))),
        }
    }
    if out.is_empty() {
        return Err(DbError::config(format!("No certificate found in '{path}'")));
    }
    Ok(out)
}

fn load_client_cert(cert_path: &str, key_path: &str) -> DbResult<ClientCert> {
    let certs = load_pem_certs(cert_path)?;
    let key_pem = read_file(key_path, "client key")?;
    let key = PrivateKey::from_pem(&key_pem).map_err(|e| DbError::config(format!("Invalid client key '{key_path}': {e}")))?;
    Ok(ClientCert::new_with_certs(&certs, key))
}

fn tls_config(params: &ConnectParams, verify: bool) -> DbResult<TlsConfig> {
    let mut b = TlsConfig::builder().provider(TlsProvider::Rustls).unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    if verify {
        let mut roots: Vec<Certificate<'static>> = Vec::new();
        if !params.ssl.only_custom_ca || params.ssl.ca_cert.is_none() {
            let native = rustls_native_certs::load_native_certs();
            for c in native.certs {
                roots.push(Certificate::from_der(c.as_ref()).to_owned());
            }
        }
        if let Some(ca) = params.ssl.ca_cert.as_deref().filter(|c| !c.is_empty()) {
            roots.extend(load_pem_certs(ca)?);
        }
        b = if roots.is_empty() { b.root_certs(RootCerts::WebPki) } else { b.root_certs(RootCerts::new_with_certs(&roots)) };
    } else {
        b = b.disable_verification(true);
    }
    if let (Some(cert), Some(key)) = (params.ssl.client_cert.as_deref().filter(|c| !c.is_empty()), params.ssl.client_key.as_deref().filter(|c| !c.is_empty())) {
        b = b.client_cert(Some(load_client_cert(cert, key)?));
    }
    Ok(b.build())
}

/// Should the connection use HTTPS, and verify certificates?
pub fn tls_plan(mode: SslMode) -> (bool, bool) {
    match mode {
        SslMode::Disable => (false, false),
        SslMode::Prefer | SslMode::Require => (true, false),
        // ureq cannot verify the chain without checking the host name, so
        // verify-ca is as strict as verify-full.
        SslMode::VerifyCa | SslMode::VerifyFull => (true, true),
    }
}

impl Transport {
    /// Build the transport for `params`. `https` selects the scheme (callers
    /// implement the `prefer` fallback by trying both).
    pub fn new(params: &ConnectParams, https: bool, verify: bool, session_id: String) -> DbResult<Transport> {
        let mut cfg = Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .timeout_connect(Some(params.connect_timeout))
            .timeout_resolve(Some(params.connect_timeout))
            .max_idle_connections_per_host(4)
            // below the 10 s keep-alive timeout of older ClickHouse versions
            .max_idle_age(Duration::from_secs(8))
            .user_agent(format!("{} (ClickHouse HTTP driver)", params.application_name));
        if https {
            cfg = cfg.tls_config(tls_config(params, verify)?);
        }
        let config = cfg.build();

        // Host used in the URL: the TLS name when tunnelled.
        let url_host = if https { params.tls_host().to_string() } else { params.host.clone() };
        let tunnelled = https && params.tls_server_name.as_deref().is_some_and(|n| n != params.host);
        let agent = if tunnelled {
            let addrs: Vec<SocketAddr> =
                (params.host.as_str(), params.port).to_socket_addrs().map_err(|e| DbError::connection(format!("Cannot resolve {}: {e}", params.host)))?.collect();
            Agent::with_parts(config, DefaultConnector::default(), FixedResolver { addrs })
        } else {
            Agent::new_with_config(config)
        };

        let host_part = if url_host.contains(':') && !url_host.starts_with('[') { format!("[{url_host}]") } else { url_host };
        let mut path = params.property("path").unwrap_or("/").trim().to_string();
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        let scheme = if https { "https" } else { "http" };
        let base_url = format!("{scheme}://{host_part}:{}{path}", params.port);
        let session_timeout = params.property("session_timeout").and_then(|s| s.trim().parse().ok()).unwrap_or(DEFAULT_SESSION_TIMEOUT);
        Ok(Transport {
            agent,
            display_url: base_url.clone(),
            base_url,
            user: params.user.clone(),
            password: params.password.clone(),
            session_id,
            session_timeout: AtomicU32::new(session_timeout),
            settings: Mutex::new(Vec::new()),
        })
    }

    /// Settings currently sent with every query.
    pub fn settings(&self) -> Vec<(String, String)> {
        self.settings.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Replace the per-connection settings.
    pub fn set_settings(&self, settings: Vec<(String, String)>) {
        if let Ok(mut g) = self.settings.lock() {
            *g = settings;
        }
    }

    /// Send a query and return the response head plus the streaming body.
    /// Non-200 responses are turned into errors here.
    pub fn send(&self, req: &QueryRequest) -> DbResult<(ResponseHead, BodyReader)> {
        let mut rb = self.agent.post(&self.base_url).header("Content-Type", "text/plain; charset=UTF-8");
        if let Some(u) = self.user.as_deref().filter(|u| !u.is_empty()) {
            rb = rb.header("X-ClickHouse-User", u);
            if let Some(p) = &self.password {
                rb = rb.header("X-ClickHouse-Key", p.expose());
            }
        } else if let Some(p) = self.password.as_ref().filter(|p| !p.is_empty()) {
            rb = rb.header("X-ClickHouse-User", "default").header("X-ClickHouse-Key", p.expose());
        }
        rb = rb.query("default_format", ROW_FORMAT);
        if !req.query_id.is_empty() {
            rb = rb.query("query_id", &req.query_id);
        }
        if req.use_session {
            rb = rb.query("session_id", &self.session_id).query("session_timeout", self.session_timeout.load(Ordering::Relaxed).to_string());
        }
        if let Some(db) = req.database.as_deref().filter(|d| !d.is_empty()) {
            rb = rb.query("database", db);
        }
        if !req.skip_default_settings {
            for (k, v) in self.settings() {
                rb = rb.query(k, v);
            }
        }
        for (k, v) in &req.extra_settings {
            rb = rb.query(k, v);
        }
        let response = rb.send(req.sql.as_bytes()).map_err(|e| transport_error(e, &self.display_url))?;
        let header = |name: &str| -> Option<String> { response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string) };
        let head = ResponseHead {
            status: response.status().as_u16(),
            format: header("X-ClickHouse-Format"),
            summary: header("X-ClickHouse-Summary").and_then(|s| Summary::parse(&s)),
            exception_code: header("X-ClickHouse-Exception-Code"),
            exception_tag: header("X-ClickHouse-Exception-Tag"),
            query_id: header("X-ClickHouse-Query-Id"),
            timezone: header("X-ClickHouse-Timezone"),
        };
        let body = response.into_body();
        if head.status != 200 {
            let mut text = String::new();
            let mut reader = body.into_with_config().limit(MAX_ERROR_BODY).lossy_utf8(true).reader();
            let _ = reader.read_to_string(&mut text);
            let mut err = if text.trim().is_empty() {
                DbError::query(format!("ClickHouse returned HTTP status {}", head.status))
            } else {
                exception_to_error(&text, head.exception_code.as_deref())
            };
            if (head.status == 401 || head.status == 403) && err.kind == dbiji_core::ErrorKind::Query {
                err.kind = dbiji_core::ErrorKind::Auth;
            }
            return Err(err);
        }
        Ok((head, body.into_reader()))
    }

    /// Run a statement and read the whole (small) response body as text.
    pub fn send_text(&self, req: &QueryRequest, limit: u64) -> DbResult<(ResponseHead, String)> {
        let (head, body) = self.send(req)?;
        let mut text = String::new();
        let mut limited = body.take(limit);
        limited.read_to_string(&mut text).map_err(|e| DbError::connection(format!("Error reading the ClickHouse response: {e}")))?;
        Ok((head, text))
    }

    /// `KILL QUERY` for `query_id` (separate request, outside the session).
    pub fn kill_query(&self, query_id: &str, sync: bool) -> DbResult<()> {
        let sql = format!("KILL QUERY WHERE query_id = {} {}", sql_string(query_id), if sync { "SYNC" } else { "ASYNC" });
        let req = QueryRequest { sql, use_session: false, skip_default_settings: true, ..Default::default() };
        let (_, text) = self.send_text(&req, 1 << 20)?;
        if let Some(i) = crate::error::find_exception_start(&text) {
            return Err(exception_to_error(&text[i..], None));
        }
        Ok(())
    }
}

/// Single-quoted ClickHouse string literal.
pub fn sql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::DbKind;
    use dbiji_core::config::ConnectionConfig;

    #[test]
    fn summary_parses() {
        let s = Summary::parse(
            r#"{"read_rows":"3","read_bytes":"11","written_rows":"3","written_bytes":"11","total_rows_to_read":"0","result_rows":"3","result_bytes":"11","elapsed_ns":"62601416"}"#,
        )
        .unwrap();
        assert_eq!(s.written_rows, 3);
        assert_eq!(s.elapsed_ns, 62_601_416);
        assert!(Summary::parse("garbage").is_none());
    }

    #[test]
    fn tls_modes() {
        assert_eq!(tls_plan(SslMode::Disable), (false, false));
        assert_eq!(tls_plan(SslMode::Require), (true, false));
        assert_eq!(tls_plan(SslMode::VerifyFull), (true, true));
    }

    #[test]
    fn urls() {
        let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "x");
        cfg.host = "::1".into();
        let p = ConnectParams::from_config(&cfg, None);
        let t = Transport::new(&p, false, false, "s".into()).unwrap();
        assert_eq!(t.base_url, "http://[::1]:8123/");
        let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "x");
        cfg.properties.insert("path".into(), "proxy/ch".into());
        let p = ConnectParams::from_config(&cfg, None);
        let t = Transport::new(&p, true, false, "s".into()).unwrap();
        assert_eq!(t.base_url, "https://localhost:8123/proxy/ch");
        assert!(!format!("{t:?}").contains("password"));
    }

    #[test]
    fn string_literal() {
        assert_eq!(sql_string("a'b\\c"), "'a\\'b\\\\c'");
    }
}
