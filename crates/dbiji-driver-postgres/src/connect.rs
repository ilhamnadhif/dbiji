//! Opening connections: tokio-postgres configuration from [`ConnectParams`],
//! our own TCP (or Unix socket) stream so TLS can be verified against
//! [`ConnectParams::tls_host`] while connecting to a tunnel endpoint, the
//! background task driving the connection (collecting notices and noticing
//! when the connection ends) and out-of-band cancellation.

use crate::error::pg;
use crate::tls::{self, TlsSetup};
use async_trait::async_trait;
use dbiji_core::config::SslMode;
use dbiji_core::{CancelHandle, ConnectParams, DbError, DbResult, ServerMessage};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_postgres::config::{ChannelBinding, Config};
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::{AsyncMessage, CancelToken, Client};

/// Maximum number of buffered server messages per session (oldest are dropped).
const MAX_NOTICES: usize = 10_000;

/// State shared between a session, its cursors and its connection task.
pub(crate) struct SessionShared {
    notices: Mutex<VecDeque<ServerMessage>>,
    closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
    /// A transaction block is open (explicit or implicit BEGIN).
    pub in_tx: AtomicBool,
    /// A cursor that has not been closed / dropped / exhausted exists.
    pub cursor_open: AtomicBool,
    /// Serialises server round trips; held by open cursors and background drains.
    pub gate: Arc<tokio::sync::Mutex<()>>,
}

impl SessionShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(SessionShared {
            notices: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            in_tx: AtomicBool::new(false),
            cursor_open: AtomicBool::new(false),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(crate) fn push_notice(&self, m: ServerMessage) {
        let mut q = self.notices.lock();
        if q.len() >= MAX_NOTICES {
            q.pop_front();
        }
        q.push_back(m);
    }

    pub(crate) fn take_notices(&self) -> Vec<ServerMessage> {
        self.notices.lock().drain(..).collect()
    }

    pub(crate) fn clear_notices(&self) {
        self.notices.lock().clear();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn set_closed(&self, reason: Option<String>) {
        if let Some(r) = reason {
            *self.close_reason.lock() = Some(r);
        }
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn closed_error(&self) -> DbError {
        match self.close_reason.lock().clone() {
            Some(r) => DbError::connection(format!("The connection to the server is closed: {r}")),
            None => DbError::connection("The connection to the server is closed"),
        }
    }

    pub(crate) fn in_tx(&self) -> bool {
        self.in_tx.load(Ordering::Acquire)
    }

    pub(crate) fn set_in_tx(&self, v: bool) {
        self.in_tx.store(v, Ordering::Release);
    }
}

/// Byte stream to the server: TCP or a Unix domain socket.
pub(crate) enum PgStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for PgStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PgStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            PgStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PgStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            PgStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            PgStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PgStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            PgStream::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PgStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            PgStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Stream used for cancel requests: shutting it down waits until the server
/// closes the connection, which the postmaster does after it has signalled
/// the backend (like libpq's `PQcancel`). This prevents a late cancel from
/// hitting the next statement.
struct CancelStream {
    inner: PgStream,
    shut: bool,
}

impl AsyncRead for CancelStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for CancelStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.shut {
            ready!(Pin::new(&mut self.inner).poll_shutdown(cx))?;
            self.shut = true;
        }
        let mut scratch = [0u8; 64];
        loop {
            let mut rb = ReadBuf::new(&mut scratch);
            match Pin::new(&mut self.inner).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) if rb.filled().is_empty() => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(())) => continue,
                // a reset also means the server has finished with the request
                Poll::Ready(Err(_)) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Connection related properties parsed from [`ConnectParams::properties`].
#[derive(Debug, Clone)]
pub(crate) struct Props {
    pub application_name: String,
    /// `-c name=value` options and raw `options`.
    pub options: Vec<String>,
    pub raw_options: Option<String>,
    pub target_session_attrs: Option<String>,
    pub channel_binding: Option<ChannelBinding>,
    pub keepalives: bool,
    pub keepalives_idle: Duration,
    pub keepalives_interval: Duration,
    pub keepalives_count: u32,
    pub connect_timeout: Option<Duration>,
    pub ssl_mode_override: Option<SslMode>,
}

/// GUCs accepted as plain properties and passed as `-c name=value` startup options.
const STARTUP_GUCS: &[&str] = &[
    "search_path",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "idle_session_timeout",
    "timezone",
    "work_mem",
    "maintenance_work_mem",
    "default_transaction_isolation",
    "default_transaction_read_only",
    "jit",
    "role",
];

/// Escape a value for the libpq `options` startup parameter.
fn escape_option(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        if c == '\\' || c.is_whitespace() {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

fn parse_secs(key: &str, v: &str) -> Option<Duration> {
    match v.trim().parse::<f64>() {
        Ok(s) if s >= 0.0 && s.is_finite() => Some(Duration::from_secs_f64(s)),
        _ => {
            tracing::warn!(property = key, "ignoring invalid numeric PostgreSQL connection property");
            None
        }
    }
}

impl Props {
    pub(crate) fn parse(params: &ConnectParams) -> Props {
        let mut p = Props {
            application_name: params.application_name.clone(),
            options: Vec::new(),
            raw_options: None,
            target_session_attrs: None,
            channel_binding: None,
            keepalives: true,
            keepalives_idle: Duration::from_secs(60),
            keepalives_interval: Duration::from_secs(15),
            keepalives_count: 4,
            connect_timeout: None,
            ssl_mode_override: None,
        };
        for (key, value) in &params.properties {
            let k = key.trim().to_ascii_lowercase();
            let v = value.trim();
            match k.as_str() {
                "application_name" | "applicationname" => p.application_name = v.to_string(),
                "options" => p.raw_options = Some(v.to_string()),
                "target_session_attrs" | "targetservertype" => {
                    let norm = v.to_ascii_lowercase().replace('_', "-");
                    let norm = match norm.as_str() {
                        "master" => "primary".to_string(),
                        "slave" | "secondary" => "standby".to_string(),
                        "preferslave" | "prefersecondary" | "preferstandby" => "prefer-standby".to_string(),
                        _ => norm,
                    };
                    p.target_session_attrs = Some(norm);
                }
                "channel_binding" => {
                    p.channel_binding = match v.to_ascii_lowercase().as_str() {
                        "disable" => Some(ChannelBinding::Disable),
                        "prefer" => Some(ChannelBinding::Prefer),
                        "require" => Some(ChannelBinding::Require),
                        _ => {
                            tracing::warn!(property = "channel_binding", "ignoring invalid value");
                            None
                        }
                    }
                }
                "keepalives" | "tcpkeepalive" => p.keepalives = !matches!(v, "0" | "false" | "off" | "no"),
                "keepalives_idle" => {
                    if let Some(d) = parse_secs(&k, v) {
                        p.keepalives_idle = d.max(Duration::from_secs(1));
                    }
                }
                "keepalives_interval" => {
                    if let Some(d) = parse_secs(&k, v) {
                        p.keepalives_interval = d.max(Duration::from_secs(1));
                    }
                }
                "keepalives_count" => match v.parse::<u32>() {
                    Ok(n) => p.keepalives_count = n.max(1),
                    Err(_) => tracing::warn!(property = "keepalives_count", "ignoring invalid value"),
                },
                "connect_timeout" | "connecttimeout" | "logintimeout" => {
                    if let Some(d) = parse_secs(&k, v)
                        && !d.is_zero()
                    {
                        p.connect_timeout = Some(d);
                    }
                }
                "sslmode" | "ssl_mode" => match SslMode::parse(v) {
                    Some(m) => p.ssl_mode_override = Some(m),
                    None => tracing::warn!(property = "sslmode", "ignoring invalid value"),
                },
                "client_encoding" | "charset" => {
                    if !v.eq_ignore_ascii_case("utf8") && !v.eq_ignore_ascii_case("utf-8") {
                        tracing::warn!("client_encoding is always UTF8 for DBiji; ignoring property");
                    }
                }
                g if STARTUP_GUCS.contains(&g) => {
                    let name = if g == "timezone" { "TimeZone" } else { g };
                    p.options.push(format!("-c {name}={}", escape_option(v)));
                }
                _ => tracing::warn!(property = %key, "ignoring unknown PostgreSQL connection property"),
            }
        }
        p
    }

    /// Value of the `options` startup parameter.
    fn options_string(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(r) = &self.raw_options
            && !r.is_empty()
        {
            parts.push(r.clone());
        }
        parts.extend(self.options.iter().cloned());
        if parts.is_empty() { None } else { Some(parts.join(" ")) }
    }
}

/// Open the byte stream (TCP or Unix socket when `host` is a directory path).
async fn open_stream(host: &str, port: u16, props: &Props) -> DbResult<PgStream> {
    #[cfg(unix)]
    if host.starts_with('/') {
        let path = format!("{}/.s.PGSQL.{port}", host.trim_end_matches('/'));
        let s = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| DbError::connection(format!("Cannot connect to the Unix socket {path}: {e}")))?;
        return Ok(PgStream::Unix(s));
    }
    let host = if host.trim().is_empty() { "localhost" } else { host.trim() };
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| {
            let mut err = DbError::from(e);
            err.message = format!("Cannot connect to {host}:{port}: {}", err.message);
            if err.kind == dbiji_core::ErrorKind::Io {
                err.kind = dbiji_core::ErrorKind::Connection;
            }
            err
        })?;
    tcp.set_nodelay(true).map_err(|e| DbError::connection(format!("Cannot configure the socket: {e}")))?;
    if props.keepalives {
        let ka = socket2::TcpKeepalive::new().with_time(props.keepalives_idle);
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows", target_os = "freebsd", target_os = "android", target_os = "ios"))]
        let ka = ka.with_interval(props.keepalives_interval).with_retries(props.keepalives_count);
        if let Err(e) = socket2::SockRef::from(&tcp).set_tcp_keepalive(&ka) {
            tracing::debug!("cannot enable TCP keepalive: {e}");
        }
    }
    Ok(PgStream::Tcp(tcp))
}

/// A freshly opened connection.
pub(crate) struct Connected {
    pub client: Client,
    pub shared: Arc<SessionShared>,
    pub cancel: Arc<PgCancelHandle>,
    pub task: tokio::task::JoinHandle<()>,
    pub props: Props,
}

/// Convert a notice into a [`ServerMessage`].
pub(crate) fn notice_message(n: &tokio_postgres::error::DbError) -> ServerMessage {
    let detail = match (n.detail(), n.hint()) {
        (Some(d), Some(h)) => Some(format!("{d}\nHint: {h}")),
        (Some(d), None) => Some(d.to_string()),
        (None, Some(h)) => Some(format!("Hint: {h}")),
        (None, None) => None,
    };
    ServerMessage {
        severity: n.severity().to_string(),
        message: n.message().to_string(),
        code: Some(n.code().code().to_string()),
        detail,
    }
}

/// Open a connection according to `params` and start its driver task.
pub(crate) async fn open(params: &ConnectParams) -> DbResult<Connected> {
    let props = Props::parse(params);
    let mut ssl = params.ssl.clone();
    if let Some(m) = props.ssl_mode_override {
        ssl.mode = m;
    }
    if params.host.starts_with('/') {
        // PostgreSQL never offers TLS on Unix domain sockets (libpq ignores sslmode there)
        ssl.mode = SslMode::Disable;
    }
    let tls = tls::build(&ssl, params.tls_host())?;
    let mut config = Config::new();
    let user = params
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .or_else(|| params.kind.default_user().map(str::to_string))
        .unwrap_or_else(|| "postgres".to_string());
    config.user(&user);
    if let Some(pw) = &params.password
        && !pw.is_empty()
    {
        // The secret is only exposed here, at the point of use.
        config.password(pw.expose());
    }
    if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
        config.dbname(db);
    }
    if !props.application_name.is_empty() {
        config.application_name(&props.application_name);
    }
    if let Some(o) = props.options_string() {
        config.options(o);
    }
    config.ssl_mode(tls.mode);
    if let Some(cb) = props.channel_binding {
        config.channel_binding(cb);
    }
    let timeout = props.connect_timeout.unwrap_or(params.connect_timeout).max(Duration::from_secs(1));
    let host = params.host.clone();
    let port = params.port;
    let connect = async {
        let stream = open_stream(&host, port, &props).await?;
        let mut mk = tls.connector.clone();
        let connector = match MakeTlsConnect::<PgStream>::make_tls_connect(&mut mk, &tls.server_name) {
            Ok(c) => c,
            Err(never) => match never {},
        };
        config.connect_raw(stream, connector).await.map_err(pg)
    };
    let (client, connection) = match tokio::time::timeout(timeout, connect).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(DbError::timeout(format!(
                "Timed out after {}s connecting to {host}:{port}",
                timeout.as_secs()
            )));
        }
    };
    let shared = SessionShared::new();
    let task_shared = shared.clone();
    let task = tokio::spawn(async move {
        let mut connection = connection;
        loop {
            match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(AsyncMessage::Notice(n))) => task_shared.push_notice(notice_message(&n)),
                Some(Ok(AsyncMessage::Notification(n))) => task_shared.push_notice(ServerMessage::new(
                    "NOTIFICATION",
                    format!("{}: {} (pid {})", n.channel(), n.payload(), n.process_id()),
                )),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    task_shared.set_closed(Some(e.to_string()));
                    break;
                }
                None => {
                    task_shared.set_closed(None);
                    break;
                }
            }
        }
    });
    let cancel = Arc::new(PgCancelHandle {
        token: client.cancel_token(),
        host: params.host.clone(),
        port: params.port,
        tls: tls.clone(),
        props: props.clone(),
        timeout,
    });
    Ok(Connected { client, shared, cancel, task, props })
}

/// Out-of-band cancellation through a new connection to the same host and
/// port the session uses (works through SSH tunnels and proxies), with the
/// session's TLS settings.
pub(crate) struct PgCancelHandle {
    token: CancelToken,
    host: String,
    port: u16,
    tls: TlsSetup,
    props: Props,
    timeout: Duration,
}

impl PgCancelHandle {
    /// Send the cancel request and wait until the server has processed it.
    pub(crate) async fn send(&self) -> DbResult<()> {
        let fut = async {
            let stream = open_stream(&self.host, self.port, &self.props).await?;
            let mut mk = self.tls.connector.clone();
            let connector = match MakeTlsConnect::<CancelStream>::make_tls_connect(&mut mk, &self.tls.server_name) {
                Ok(c) => c,
                Err(never) => match never {},
            };
            self.token
                .cancel_query_raw(CancelStream { inner: stream, shut: false }, connector)
                .await
                .map_err(pg)
        };
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(DbError::timeout("Timed out sending the cancel request")),
        }
    }
}

#[async_trait]
impl CancelHandle for PgCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        self.send().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn params(props: &[(&str, &str)]) -> ConnectParams {
        let mut cfg = dbiji_core::ConnectionConfig::new(dbiji_core::DbKind::Postgres, "t");
        cfg.properties = props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        ConnectParams::from_config(&cfg, None)
    }

    #[test]
    fn properties_map_to_startup_options() {
        let p = Props::parse(&params(&[
            ("search_path", "app, public"),
            ("statement_timeout", "5000"),
            ("options", "-c work_mem=8MB"),
            ("application_name", "MyApp"),
            ("target_session_attrs", "read-write"),
            ("keepalives_idle", "30"),
            ("unknown_thing", "x"),
        ]));
        assert_eq!(p.application_name, "MyApp");
        assert_eq!(p.target_session_attrs.as_deref(), Some("read-write"));
        assert_eq!(p.keepalives_idle, Duration::from_secs(30));
        let o = p.options_string().unwrap();
        assert!(o.starts_with("-c work_mem=8MB"), "{o}");
        assert!(o.contains("-c search_path=app,\\ public"), "{o}");
        assert!(o.contains("-c statement_timeout=5000"), "{o}");
    }

    #[test]
    fn escaping() {
        assert_eq!(escape_option("a b\\c"), "a\\ b\\\\c");
    }
}
