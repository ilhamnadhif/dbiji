//! Local TCP forwarders through chains of proxies and SSH servers.
//!
//! Database drivers always connect to a host and port, so every tunnel is
//! exposed as a TCP listener on `127.0.0.1` (a fixed `local_port` or an
//! ephemeral one). Each accepted connection is forwarded through the hop
//! chain to the target database:
//!
//! * `[Ssh]` – classic SSH tunnel (`ssh -L`),
//! * `[Proxy]` – SOCKS5 or HTTP CONNECT proxy,
//! * `[Proxy, Ssh]` – SSH server reached through a proxy,
//! * `[Ssh, Ssh]` – jump host (and any longer combination).
//!
//! One SSH session per SSH hop is shared by all forwarded connections; each
//! forwarded connection is a `direct-tcpip` channel. A dead session is
//! re-established transparently when the next connection arrives.

use crate::http_proxy::{HttpProxyCredentials, http_connect};
use crate::known_hosts::KnownHosts;
use crate::socks::{SocksCredentials, socks5_authenticate, socks5_connect};
use crate::ssh::{ChannelError, SshParams, SshSession};
use crate::stream::{BoxedStream, display_addr, tcp_connect};
use dbiji_core::config::{ProxyConfig, ProxyKind, SshConfig};
use dbiji_core::{DbError, DbResult, ErrorKind, Secret};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Timeout for proxy handshakes and for opening forwarded connections when
/// no SSH hop supplies one.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an SSH liveness probe may take before the session is considered dead.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One step of a tunnel chain. `hops[0]` is contacted first.
#[derive(Debug, Clone)]
pub enum Hop {
    /// SOCKS5 or HTTP CONNECT proxy. `password` is used when `cfg.user` is set.
    Proxy {
        /// Proxy settings.
        cfg: ProxyConfig,
        /// Proxy password (used when `cfg.user` is set).
        password: Option<Secret>,
    },
    /// SSH server. `password` is used for password / keyboard-interactive
    /// authentication, `key_passphrase` to decrypt the private key file.
    Ssh {
        /// SSH server settings.
        cfg: SshConfig,
        /// Password for password / keyboard-interactive authentication.
        password: Option<Secret>,
        /// Passphrase of an encrypted private key file.
        key_passphrase: Option<Secret>,
    },
}

impl Hop {
    /// Host and port this hop connects to.
    pub fn address(&self) -> (&str, u16) {
        match self {
            Hop::Proxy { cfg, .. } => (&cfg.host, cfg.port),
            Hop::Ssh { cfg, .. } => (&cfg.host, cfg.port),
        }
    }

    fn enabled(&self) -> bool {
        match self {
            Hop::Proxy { cfg, .. } => cfg.enabled,
            Hop::Ssh { cfg, .. } => cfg.enabled,
        }
    }

    fn describe(&self) -> String {
        match self {
            Hop::Proxy { cfg, .. } => {
                let kind = match cfg.kind {
                    ProxyKind::Socks5 => "SOCKS5",
                    ProxyKind::Http => "HTTP",
                };
                format!("{kind} proxy {}", display_addr(&cfg.host, cfg.port))
            }
            Hop::Ssh { cfg, .. } => {
                format!("SSH {}@{}", cfg.user, display_addr(&cfg.host, cfg.port))
            }
        }
    }
}

/// Description of a tunnel to open with [`open_tunnel`].
#[derive(Debug, Clone)]
pub struct TunnelSpec {
    /// Hop chain, nearest first. Hops whose config has `enabled == false` are skipped.
    pub hops: Vec<Hop>,
    /// Database host as seen from the last hop.
    pub target_host: String,
    /// Database port.
    pub target_port: u16,
    /// DBiji's own known_hosts file (read and written; e.g. `AppPaths::known_hosts_file()`).
    pub known_hosts_path: PathBuf,
    /// Fixed local port. `None` uses the `local_port` of the last SSH hop if
    /// set, otherwise an ephemeral port.
    pub local_port: Option<u16>,
    /// Also trust hosts listed in the user's `~/.ssh/known_hosts` (read only).
    pub use_system_known_hosts: bool,
    /// ssh-agent socket (Unix) or named pipe (Windows); `None` = `SSH_AUTH_SOCK` / the default pipe.
    pub agent_socket: Option<PathBuf>,
}

impl TunnelSpec {
    /// A spec without hops; add them with [`TunnelSpec::with_proxy`] / [`TunnelSpec::with_ssh`].
    pub fn new(
        target_host: impl Into<String>,
        target_port: u16,
        known_hosts_path: impl Into<PathBuf>,
    ) -> Self {
        TunnelSpec {
            hops: Vec::new(),
            target_host: target_host.into(),
            target_port,
            known_hosts_path: known_hosts_path.into(),
            local_port: None,
            use_system_known_hosts: true,
            agent_socket: None,
        }
    }

    /// Appends a proxy hop.
    pub fn with_proxy(mut self, cfg: ProxyConfig, password: Option<Secret>) -> Self {
        self.hops.push(Hop::Proxy { cfg, password });
        self
    }

    /// Appends an SSH hop.
    pub fn with_ssh(
        mut self,
        cfg: SshConfig,
        password: Option<Secret>,
        key_passphrase: Option<Secret>,
    ) -> Self {
        self.hops.push(Hop::Ssh {
            cfg,
            password,
            key_passphrase,
        });
        self
    }

    /// Sets a fixed local port.
    pub fn with_local_port(mut self, port: Option<u16>) -> Self {
        self.local_port = port;
        self
    }

    /// Overrides the ssh-agent socket.
    pub fn with_agent_socket(mut self, socket: Option<PathBuf>) -> Self {
        self.agent_socket = socket;
        self
    }

    /// Enables or disables reading `~/.ssh/known_hosts`.
    pub fn with_system_known_hosts(mut self, enabled: bool) -> Self {
        self.use_system_known_hosts = enabled;
        self
    }

    /// True when at least one enabled hop is configured (otherwise connect directly).
    pub fn needs_tunnel(&self) -> bool {
        self.hops.iter().any(Hop::enabled)
    }
}

/// True when a connection with these (effective) settings must go through [`open_tunnel`].
pub fn needs_tunnel(ssh: Option<&SshConfig>, proxy: Option<&ProxyConfig>) -> bool {
    ssh.is_some_and(|s| s.enabled) || proxy.is_some_and(|p| p.enabled)
}

/// Counters of a running tunnel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TunnelStats {
    /// Forwarded connections currently open.
    pub active_connections: usize,
    /// Connections accepted since the tunnel was opened.
    pub total_connections: u64,
    /// Accepted connections that could not be forwarded.
    pub failed_connections: u64,
    /// SSH sessions re-established after the previous one died.
    pub ssh_reconnects: u64,
}

enum HopRuntime {
    Proxy {
        cfg: ProxyConfig,
        password: Option<Secret>,
    },
    Ssh(SshHop),
}

struct SshHop {
    params: SshParams,
    session: tokio::sync::Mutex<Option<Arc<SshSession>>>,
    /// A session was established at least once (later ones are reconnects).
    connected_once: AtomicBool,
}

struct Chain {
    hops: Vec<HopRuntime>,
    known_hosts: Arc<KnownHosts>,
    target_host: String,
    target_port: u16,
    connect_timeout: Duration,
    reconnects: AtomicU64,
}

type DialFuture<'a> = Pin<Box<dyn Future<Output = DbResult<BoxedStream>> + Send + 'a>>;

impl Chain {
    /// Opens a stream to `host:port` reachable after traversing `hops[..level]`.
    fn dial<'a>(
        &'a self,
        level: usize,
        host: &'a str,
        port: u16,
        originator: SocketAddr,
        timeout: Duration,
    ) -> DialFuture<'a> {
        Box::pin(async move {
            if level == 0 {
                let s = tcp_connect(host, port, timeout).await?;
                return Ok(Box::new(s) as BoxedStream);
            }
            let hop_index = level - 1;
            match self.hops.get(hop_index) {
                None => Err(DbError::internal("tunnel hop index out of range")),
                Some(HopRuntime::Proxy { cfg, password }) => {
                    let mut s = self
                        .dial(hop_index, &cfg.host, cfg.port, originator, timeout)
                        .await?;
                    let handshake = proxy_handshake(&mut s, cfg, password.as_ref(), host, port);
                    match tokio::time::timeout(timeout, handshake).await {
                        Ok(r) => r?,
                        Err(_) => {
                            return Err(DbError::new(
                                ErrorKind::Timeout,
                                format!(
                                    "Timed out negotiating with the proxy {}",
                                    display_addr(&cfg.host, cfg.port)
                                ),
                            ));
                        }
                    }
                    Ok(s)
                }
                Some(HopRuntime::Ssh(_)) => {
                    self.ssh_channel(hop_index, host, port, originator, timeout)
                        .await
                }
            }
        })
    }

    async fn ssh_channel(
        &self,
        index: usize,
        host: &str,
        port: u16,
        originator: SocketAddr,
        timeout: Duration,
    ) -> DbResult<BoxedStream> {
        let mut last_err = None;
        for attempt in 0..2 {
            let session = self.ssh_session(index).await?;
            let open = session.open_direct_tcpip(host, port, originator);
            match tokio::time::timeout(timeout, open).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(ChannelError::Rejected(e))) => return Err(e),
                Ok(Err(ChannelError::SessionLost(e))) => {
                    tracing::info!("{e}; reconnecting");
                    self.invalidate(index, &session).await;
                    last_err = Some(e);
                }
                Err(_) => {
                    let err = DbError::new(
                        ErrorKind::Timeout,
                        format!(
                            "Timed out opening an SSH channel to {} through {}",
                            display_addr(host, port),
                            session.label()
                        ),
                    );
                    // A slow target keeps the session; an unresponsive session is replaced.
                    if attempt == 0 && !session.probe(self.probe_timeout(index)).await {
                        tracing::info!(
                            "SSH session {} does not respond; reconnecting",
                            session.label()
                        );
                        self.invalidate(index, &session).await;
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| DbError::ssh("The SSH session could not be re-established")))
    }

    /// Returns a live session for the SSH hop at `index`, (re)connecting when needed.
    async fn ssh_session(&self, index: usize) -> DbResult<Arc<SshSession>> {
        let Some(HopRuntime::Ssh(hop)) = self.hops.get(index) else {
            return Err(DbError::internal("tunnel hop is not an SSH hop"));
        };
        let mut guard = hop.session.lock().await;
        if let Some(s) = guard.as_ref() {
            if s.is_alive() {
                return Ok(s.clone());
            }
            let why = s
                .disconnect_reason()
                .unwrap_or_else(|| "connection lost".into());
            tracing::info!("SSH session {} ended ({why}); reconnecting", s.label());
            *guard = None;
        }
        let cfg = &hop.params.cfg;
        let originator = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let stream = self
            .dial(
                index,
                &cfg.host,
                cfg.port,
                originator,
                hop.params.connect_timeout(),
            )
            .await
            .map_err(|e| {
                if index == 0 {
                    DbError::new(
                        e.kind,
                        format!("Cannot reach the SSH server: {}", e.message),
                    )
                } else {
                    e
                }
            })?;
        let session =
            Arc::new(SshSession::connect(stream, &hop.params, self.known_hosts.clone()).await?);
        if hop.connected_once.swap(true, Ordering::Relaxed) {
            self.reconnects.fetch_add(1, Ordering::Relaxed);
        }
        *guard = Some(session.clone());
        Ok(session)
    }

    async fn invalidate(&self, index: usize, session: &Arc<SshSession>) {
        if let Some(HopRuntime::Ssh(hop)) = self.hops.get(index) {
            let mut guard = hop.session.lock().await;
            if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, session)) {
                *guard = None;
            }
        }
    }

    /// Liveness probe timeout of an SSH hop: at most [`PROBE_TIMEOUT`], less
    /// when the hop's connect timeout is shorter.
    fn probe_timeout(&self, index: usize) -> Duration {
        match self.hops.get(index) {
            Some(HopRuntime::Ssh(hop)) => hop.params.connect_timeout().min(PROBE_TIMEOUT),
            _ => PROBE_TIMEOUT,
        }
    }

    /// Like [`Chain::warm_up`], additionally replacing sessions that look
    /// alive but no longer answer.
    async fn ensure_live(&self) -> DbResult<()> {
        for (i, hop) in self.hops.iter().enumerate() {
            if matches!(hop, HopRuntime::Ssh(_)) {
                let session = self.ssh_session(i).await?;
                if !session.probe(self.probe_timeout(i)).await {
                    tracing::info!(
                        "SSH session {} does not respond; reconnecting",
                        session.label()
                    );
                    self.invalidate(i, &session).await;
                    self.ssh_session(i).await?;
                }
            }
        }
        Ok(())
    }

    /// Establishes every SSH session in order.
    async fn warm_up(&self) -> DbResult<()> {
        for (i, hop) in self.hops.iter().enumerate() {
            if matches!(hop, HopRuntime::Ssh(_)) {
                self.ssh_session(i).await?;
            }
        }
        // Proxy-only chain: check reachability (and SOCKS5 credentials) without touching the database.
        if let Some(HopRuntime::Proxy { cfg, password }) = self.hops.first()
            && self.hops.len() == 1
        {
            let mut s = tcp_connect(&cfg.host, cfg.port, self.connect_timeout)
                .await
                .map_err(|e| {
                    DbError::new(e.kind, format!("Cannot reach the proxy: {}", e.message))
                })?;
            if cfg.kind == ProxyKind::Socks5 {
                let empty = Secret::default();
                let creds = proxy_user(cfg).map(|user| SocksCredentials {
                    user,
                    password: password.as_ref().unwrap_or(&empty),
                });
                let probe = socks5_authenticate(&mut s, creds);
                match tokio::time::timeout(self.connect_timeout, probe).await {
                    Ok(r) => r?,
                    Err(_) => {
                        return Err(DbError::timeout(
                            "Timed out negotiating with the SOCKS5 proxy",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn ssh_connected(&self) -> bool {
        self.hops.iter().all(|h| match h {
            HopRuntime::Proxy { .. } => true,
            HopRuntime::Ssh(hop) => hop
                .session
                .try_lock()
                .map(|g| g.as_ref().is_some_and(|s| s.is_alive()))
                .unwrap_or(false),
        })
    }

    async fn shutdown(&self) {
        for hop in &self.hops {
            if let HopRuntime::Ssh(h) = hop {
                let session = h.session.lock().await.take();
                if let Some(s) = session {
                    s.disconnect().await;
                }
            }
        }
    }

    async fn dial_target(&self, originator: SocketAddr) -> DbResult<BoxedStream> {
        self.dial(
            self.hops.len(),
            &self.target_host,
            self.target_port,
            originator,
            self.connect_timeout,
        )
        .await
    }
}

fn proxy_user(cfg: &ProxyConfig) -> Option<&str> {
    cfg.user.as_deref().map(str::trim).filter(|u| !u.is_empty())
}

async fn proxy_handshake(
    stream: &mut BoxedStream,
    cfg: &ProxyConfig,
    password: Option<&Secret>,
    host: &str,
    port: u16,
) -> DbResult<()> {
    let user = proxy_user(cfg);
    let empty = Secret::default();
    match cfg.kind {
        ProxyKind::Socks5 => {
            let creds = user.map(|user| SocksCredentials {
                user,
                password: password.unwrap_or(&empty),
            });
            socks5_connect(stream, host, port, creds).await
        }
        ProxyKind::Http => {
            let creds = user.map(|user| HttpProxyCredentials {
                user,
                password: password.unwrap_or(&empty),
            });
            http_connect(stream, host, port, creds).await
        }
    }
}

struct Shared {
    chain: Chain,
    last_error: Mutex<Option<DbError>>,
    active: AtomicUsize,
    total: AtomicU64,
    failed: AtomicU64,
    closed: AtomicBool,
    description: String,
}

impl Shared {
    fn record_error(&self, e: DbError) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(e);
        }
    }
}

/// A running local forwarder. Dropping it stops the listener, closes all
/// forwarded connections and the SSH sessions.
pub struct Tunnel {
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    tracker: TaskTracker,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel")
            .field("local_addr", &self.local_addr)
            .field("route", &self.shared.description)
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// Opens a tunnel: validates the hops, establishes the SSH session(s)
/// eagerly (so authentication and host key problems are reported here), binds
/// the local listener and starts forwarding.
pub async fn open_tunnel(spec: TunnelSpec) -> DbResult<Tunnel> {
    let hops: Vec<Hop> = spec.hops.into_iter().filter(Hop::enabled).collect();
    if hops.is_empty() {
        return Err(DbError::config(
            "The tunnel has no enabled SSH or proxy hop",
        ));
    }
    if spec.target_host.trim().is_empty() || spec.target_port == 0 {
        return Err(DbError::config(
            "The tunnel target host and port must be set",
        ));
    }
    for hop in &hops {
        validate_hop(hop)?;
    }
    let description = {
        let mut parts: Vec<String> = hops.iter().map(Hop::describe).collect();
        parts.push(display_addr(&spec.target_host, spec.target_port));
        parts.join(" -> ")
    };
    let local_port = spec.local_port.filter(|p| *p != 0).or_else(|| {
        hops.iter().rev().find_map(|h| match h {
            Hop::Ssh { cfg, .. } => cfg.local_port.filter(|p| *p != 0),
            Hop::Proxy { .. } => None,
        })
    });
    let connect_timeout = hops
        .iter()
        .filter_map(|h| match h {
            Hop::Ssh { cfg, .. } => Some(Duration::from_secs(u64::from(
                cfg.connect_timeout_secs.max(1),
            ))),
            Hop::Proxy { .. } => None,
        })
        .max()
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT);

    let known_hosts = Arc::new(if spec.use_system_known_hosts {
        KnownHosts::new(&spec.known_hosts_path)
    } else {
        KnownHosts::own_only(&spec.known_hosts_path)
    });
    let runtime_hops = hops
        .into_iter()
        .map(|h| match h {
            Hop::Proxy { cfg, password } => HopRuntime::Proxy { cfg, password },
            Hop::Ssh {
                cfg,
                password,
                key_passphrase,
            } => HopRuntime::Ssh(SshHop {
                params: SshParams {
                    cfg,
                    password,
                    key_passphrase,
                    agent_socket: spec.agent_socket.clone(),
                },
                session: tokio::sync::Mutex::new(None),
                connected_once: AtomicBool::new(false),
            }),
        })
        .collect();
    let chain = Chain {
        hops: runtime_hops,
        known_hosts,
        target_host: spec.target_host,
        target_port: spec.target_port,
        connect_timeout,
        reconnects: AtomicU64::new(0),
    };

    let bind_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port.unwrap_or(0)));
    let listener = bind_listener(bind_addr).await?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| DbError::io(format!("Cannot read the listener address: {e}")))?;

    chain.warm_up().await?;

    let shared = Arc::new(Shared {
        chain,
        last_error: Mutex::new(None),
        active: AtomicUsize::new(0),
        total: AtomicU64::new(0),
        failed: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        description,
    });
    let cancel = CancellationToken::new();
    let tracker = TaskTracker::new();
    tracing::info!("tunnel {local_addr} -> {}", shared.description);
    tracker.spawn(accept_loop(
        listener,
        shared.clone(),
        cancel.clone(),
        tracker.clone(),
    ));
    Ok(Tunnel {
        local_addr,
        shared,
        cancel,
        tracker,
    })
}

/// Binds the local listener. A fixed port that is still held (typically by a
/// tunnel that was just dropped and is shutting down) is retried for up to a second.
async fn bind_listener(addr: SocketAddr) -> DbResult<TcpListener> {
    let mut attempts = if addr.port() == 0 { 1 } else { 10 };
    loop {
        attempts -= 1;
        match TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempts > 0 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(DbError::config(format!(
                    "Local port {} is already in use; choose another local port",
                    addr.port()
                )));
            }
            Err(e) => return Err(DbError::io(format!("Cannot listen on {addr}: {e}"))),
        }
    }
}

fn validate_hop(hop: &Hop) -> DbResult<()> {
    match hop {
        Hop::Proxy { cfg, .. } => {
            if cfg.host.trim().is_empty() || cfg.port == 0 {
                return Err(DbError::config("The proxy host and port must be set"));
            }
        }
        Hop::Ssh { cfg, .. } => {
            if cfg.host.trim().is_empty() || cfg.port == 0 {
                return Err(DbError::config("The SSH host and port must be set"));
            }
            if cfg.user.trim().is_empty() {
                return Err(DbError::config("The SSH user name must be set"));
            }
        }
    }
    Ok(())
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    tracker: TaskTracker,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => {
                    let _ = socket.set_nodelay(true);
                    shared.total.fetch_add(1, Ordering::Relaxed);
                    tracker.spawn(forward(socket, peer, shared.clone(), cancel.child_token()));
                }
                Err(e) => {
                    tracing::warn!("tunnel accept failed: {e}");
                    shared.record_error(DbError::io(format!("Accepting a local connection failed: {e}")));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    shared.closed.store(true, Ordering::Release);
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn forward(
    mut inbound: TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    cancel: CancellationToken,
) {
    shared.active.fetch_add(1, Ordering::Relaxed);
    let _guard = ActiveGuard(&shared.active);
    let outbound = tokio::select! {
        _ = cancel.cancelled() => return,
        r = shared.chain.dial_target(peer) => r,
    };
    let mut outbound = match outbound {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("tunnel {}: {}", shared.description, e.message);
            shared.failed.fetch_add(1, Ordering::Relaxed);
            shared.record_error(e);
            return;
        }
    };
    tokio::select! {
        _ = cancel.cancelled() => {}
        r = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {
            if let Err(e) = r {
                tracing::debug!("tunnel connection from {peer} ended: {e}");
            }
        }
    }
}

impl Tunnel {
    /// Port of the local listener on 127.0.0.1.
    pub fn local_port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Address of the local listener (always on 127.0.0.1).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Host drivers must connect to (`127.0.0.1`).
    pub fn local_host(&self) -> String {
        self.local_addr.ip().to_string()
    }

    /// Human readable route, e.g. `SSH deploy@bastion:22 -> db:5432`.
    pub fn description(&self) -> &str {
        &self.shared.description
    }

    /// The listener is running (the tunnel was not closed). SSH sessions that
    /// died are re-established on the next connection.
    pub fn is_alive(&self) -> bool {
        !self.cancel.is_cancelled() && !self.shared.closed.load(Ordering::Acquire)
    }

    /// Every SSH session of the chain is currently connected.
    pub fn ssh_connected(&self) -> bool {
        self.shared.chain.ssh_connected()
    }

    /// The most recent forwarding error (failed connection, lost session ...).
    pub fn last_error(&self) -> Option<DbError> {
        self.shared.last_error.lock().ok().and_then(|e| e.clone())
    }

    /// Clears [`Tunnel::last_error`].
    pub fn clear_last_error(&self) {
        if let Ok(mut slot) = self.shared.last_error.lock() {
            *slot = None;
        }
    }

    /// Connection counters.
    pub fn stats(&self) -> TunnelStats {
        TunnelStats {
            active_connections: self.shared.active.load(Ordering::Relaxed),
            total_connections: self.shared.total.load(Ordering::Relaxed),
            failed_connections: self.shared.failed.load(Ordering::Relaxed),
            ssh_reconnects: self.shared.chain.reconnects.load(Ordering::Relaxed),
        }
    }

    /// (Re)establishes every SSH session now, e.g. before reconnecting a
    /// driver, so errors are reported directly. Sessions that look alive but
    /// no longer answer a keepalive are replaced.
    pub async fn ensure_connected(&self) -> DbResult<()> {
        if !self.is_alive() {
            return Err(DbError::connection("The tunnel is closed"));
        }
        self.shared.chain.ensure_live().await
    }

    /// Stops the listener, closes all forwarded connections and disconnects
    /// the SSH sessions. Idempotent.
    pub async fn close(&self) {
        self.cancel.cancel();
        self.tracker.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.tracker.wait()).await;
        self.shared.chain.shutdown().await;
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.tracker.close();
    }
}
