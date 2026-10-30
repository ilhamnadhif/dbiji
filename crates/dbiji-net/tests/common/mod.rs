//! Test fixtures: an in-process SSH server (russh), a TCP echo server and
//! mock SOCKS5 / HTTP CONNECT proxies. Everything listens on 127.0.0.1.
#![allow(dead_code)]

use base64::Engine;
use russh::keys::ssh_key::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub fn ed25519_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("key generation")
}

// ---------------------------------------------------------------- echo server

pub struct EchoServer {
    pub addr: SocketAddr,
    pub connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl EchoServer {
    pub async fn start() -> EchoServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let c = connections.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                c.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        EchoServer {
            addr,
            connections,
            task,
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Sends `payload` and reads the same number of bytes back.
pub async fn echo_roundtrip(port: u16, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await?;
    let (mut r, mut w) = s.split();
    let write = async {
        w.write_all(payload).await?;
        w.flush().await
    };
    let mut buf = vec![0u8; payload.len()];
    let read = r.read_exact(&mut buf);
    let (wr, rd) = tokio::join!(write, read);
    wr?;
    rd?;
    Ok(buf)
}

/// Connects and returns whether the peer closes the connection (EOF) within `wait`.
pub async fn closed_without_data(port: u16, wait: Duration) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await else {
        return true;
    };
    let _ = s.write_all(b"ping").await;
    let mut b = [0u8; 16];
    matches!(
        tokio::time::timeout(wait, s.read(&mut b)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

// ----------------------------------------------------------------- SSH server

#[derive(Clone)]
pub struct SshServerOptions {
    pub user: String,
    pub password: Option<String>,
    /// Accept the password only through keyboard-interactive (no "password" method).
    pub keyboard_interactive_only: bool,
    pub authorized_keys: Vec<PublicKey>,
    pub host_key: PrivateKey,
    pub inactivity_timeout: Option<Duration>,
    /// Fixed port (restart tests); 0 = ephemeral.
    pub port: u16,
}

impl SshServerOptions {
    pub fn password(user: &str, password: &str) -> Self {
        SshServerOptions {
            user: user.into(),
            password: Some(password.into()),
            keyboard_interactive_only: false,
            authorized_keys: Vec::new(),
            host_key: ed25519_key(),
            inactivity_timeout: None,
            port: 0,
        }
    }
}

#[derive(Default)]
pub struct ServerStats {
    pub sessions: AtomicUsize,
    pub authenticated: AtomicUsize,
    pub channels: AtomicUsize,
    pub targets: Mutex<Vec<(String, u32)>>,
}

pub struct TestSshServer {
    pub addr: SocketAddr,
    pub stats: Arc<ServerStats>,
    pub host_key: PrivateKey,
    handle: server::RunningServerHandle,
    task: JoinHandle<()>,
}

struct ServerState {
    opts: SshServerOptions,
    stats: Arc<ServerStats>,
}

#[derive(Clone)]
struct TestServer {
    state: Arc<ServerState>,
}

struct TestHandler {
    state: Arc<ServerState>,
}

impl server::Server for TestServer {
    type Handler = TestHandler;
    fn new_client(&mut self, _peer: Option<SocketAddr>) -> TestHandler {
        self.state.stats.sessions.fetch_add(1, Ordering::SeqCst);
        TestHandler {
            state: self.state.clone(),
        }
    }
}

impl server::Handler for TestHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let o = &self.state.opts;
        if !o.keyboard_interactive_only && user == o.user && o.password.as_deref() == Some(password)
        {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.check_key(user, key)
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.check_key(user, key)
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let o = &self.state.opts;
        if user != o.user || o.password.is_none() {
            return Ok(Auth::reject());
        }
        match response {
            None => Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            }),
            Some(mut r) => {
                let answer = r.next().map(|b| b.to_vec()).unwrap_or_default();
                if Some(answer.as_slice()) == o.password.as_deref().map(str::as_bytes) {
                    Ok(Auth::Accept)
                } else {
                    Ok(Auth::reject())
                }
            }
        }
    }

    async fn auth_succeeded(&mut self, _session: &mut Session) -> Result<(), Self::Error> {
        self.state
            .stats
            .authenticated
            .fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.state.stats.channels.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut t) = self.state.stats.targets.lock() {
            t.push((host_to_connect.to_string(), port_to_connect));
        }
        let Ok(port) = u16::try_from(port_to_connect) else {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        match TcpStream::connect((host_to_connect, port)).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl TestHandler {
    fn check_key(&self, user: &str, key: &PublicKey) -> Result<Auth, russh::Error> {
        let o = &self.state.opts;
        if user == o.user
            && o.authorized_keys
                .iter()
                .any(|k| k.key_data() == key.key_data())
        {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }
}

impl TestSshServer {
    pub async fn start(opts: SshServerOptions) -> TestSshServer {
        let listener = TcpListener::bind(("127.0.0.1", opts.port))
            .await
            .expect("bind ssh server");
        let addr = listener.local_addr().unwrap();
        let mut methods = Vec::new();
        if !opts.keyboard_interactive_only && opts.password.is_some() {
            methods.push(MethodKind::Password);
        }
        if !opts.authorized_keys.is_empty() {
            methods.push(MethodKind::PublicKey);
        }
        if opts.password.is_some() {
            methods.push(MethodKind::KeyboardInteractive);
        }
        let config = Arc::new(server::Config {
            methods: MethodSet::from(&methods[..]),
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::from_millis(0)),
            keys: vec![opts.host_key.clone()],
            inactivity_timeout: opts.inactivity_timeout,
            ..Default::default()
        });
        let stats = Arc::new(ServerStats::default());
        let host_key = opts.host_key.clone();
        let state = Arc::new(ServerState {
            opts,
            stats: stats.clone(),
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut srv = TestServer { state };
            let running = srv.run_on_socket(config, &listener);
            let _ = tx.send(running.handle());
            let _ = running.await;
        });
        let handle = rx.await.expect("server handle");
        TestSshServer {
            addr,
            stats,
            host_key,
            handle,
            task,
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Disconnects all clients and stops listening.
    pub async fn stop(self) {
        self.handle.shutdown("test server stopping".into());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
        // Give the session tasks a moment to deliver the disconnect.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    pub fn sessions(&self) -> usize {
        self.stats.sessions.load(Ordering::SeqCst)
    }

    pub fn channels(&self) -> usize {
        self.stats.channels.load(Ordering::SeqCst)
    }
}

// ------------------------------------------------------------- mock proxies

pub struct MockProxy {
    pub addr: SocketAddr,
    pub connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for MockProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockProxy {
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// SOCKS5 proxy; `auth` = required (user, password).
    pub async fn socks5(auth: Option<(&'static str, &'static str)>) -> MockProxy {
        Self::spawn(move |s| Box::pin(socks5_session(s, auth))).await
    }

    /// HTTP CONNECT proxy; `auth` = required Basic credentials.
    pub async fn http(auth: Option<(&'static str, &'static str)>) -> MockProxy {
        Self::spawn(move |s| Box::pin(http_session(s, auth))).await
    }

    async fn spawn<F>(handler: F) -> MockProxy
    where
        F: Fn(
                TcpStream,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let c = connections.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    break;
                };
                c.fetch_add(1, Ordering::SeqCst);
                let h = handler.clone();
                tokio::spawn(async move {
                    let _ = h(s).await;
                });
            }
        });
        MockProxy {
            addr,
            connections,
            task,
        }
    }
}

async fn socks5_session(
    mut s: TcpStream,
    auth: Option<(&'static str, &'static str)>,
) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods).await?;
    match auth {
        Some((user, pass)) => {
            if !methods.contains(&2) {
                s.write_all(&[5, 0xFF]).await?;
                return Ok(());
            }
            s.write_all(&[5, 2]).await?;
            let mut v = [0u8; 2];
            s.read_exact(&mut v).await?;
            let mut u = vec![0u8; v[1] as usize];
            s.read_exact(&mut u).await?;
            let mut pl = [0u8; 1];
            s.read_exact(&mut pl).await?;
            let mut p = vec![0u8; pl[0] as usize];
            s.read_exact(&mut p).await?;
            if u != user.as_bytes() || p != pass.as_bytes() {
                s.write_all(&[1, 1]).await?;
                return Ok(());
            }
            s.write_all(&[1, 0]).await?;
        }
        None => s.write_all(&[5, 0]).await?,
    }
    let mut req = [0u8; 4];
    if s.read_exact(&mut req).await.is_err() {
        // Authentication probe without a request.
        return Ok(());
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            let mut n = vec![0u8; l[0] as usize];
            s.read_exact(&mut n).await?;
            String::from_utf8_lossy(&n).into_owned()
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    match TcpStream::connect((host.as_str(), u16::from_be_bytes(port))).await {
        Ok(mut target) => {
            s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
            let _ = tokio::io::copy_bidirectional(&mut s, &mut target).await;
        }
        Err(_) => s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?,
    }
    Ok(())
}

async fn http_session(
    mut s: TcpStream,
    auth: Option<(&'static str, &'static str)>,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).await? == 0 {
            return Ok(());
        }
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let target = text.split_whitespace().nth(1).unwrap_or("").to_string();
    if let Some((user, pass)) = auth {
        let expected = format!(
            "Proxy-Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        );
        if !text.contains(&expected) {
            s.write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
            )
            .await?;
            return Ok(());
        }
    }
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (
            h.trim_start_matches('[').trim_end_matches(']').to_string(),
            p.parse::<u16>().unwrap_or(0),
        ),
        None => (target.clone(), 80),
    };
    match TcpStream::connect((host.as_str(), port)).await {
        Ok(mut t) => {
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
            let _ = tokio::io::copy_bidirectional(&mut s, &mut t).await;
        }
        Err(_) => s.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?,
    }
    Ok(())
}

/// TCP relay whose existing connections can be "black-holed": they stay
/// open but stop forwarding, like a dead network path without RST.
pub struct FreezableRelay {
    pub addr: SocketAddr,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    freeze_below: tokio::sync::watch::Sender<u64>,
    task: JoinHandle<()>,
}

impl Drop for FreezableRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FreezableRelay {
    pub async fn start(target: SocketAddr) -> FreezableRelay {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let next_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let ids = next_id.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((down, _)) = listener.accept().await else {
                    break;
                };
                let id = ids.fetch_add(1, Ordering::SeqCst);
                let mut rx = rx.clone();
                tokio::spawn(async move {
                    let Ok(up) = TcpStream::connect(target).await else {
                        return;
                    };
                    let (mut dr, mut dw) = down.into_split();
                    let (mut ur, mut uw) = up.into_split();
                    let copy = async {
                        let _ = tokio::join!(
                            tokio::io::copy(&mut dr, &mut uw),
                            tokio::io::copy(&mut ur, &mut dw)
                        );
                    };
                    let frozen = async {
                        loop {
                            if *rx.borrow() > id {
                                break;
                            }
                            if rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    };
                    tokio::select! {
                        _ = copy => {}
                        _ = frozen => {
                            // Keep both sockets open without forwarding anything.
                            std::future::pending::<()>().await;
                        }
                    }
                });
            }
        });
        FreezableRelay {
            addr,
            next_id,
            freeze_below: tx,
            task,
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Black-holes every connection made so far; new connections still work.
    pub fn freeze_existing(&self) {
        let n = self.next_id.load(Ordering::SeqCst);
        let _ = self.freeze_below.send(n);
    }
}

/// A local port that currently refuses connections (verified, because other
/// processes on the machine may take freed ports at any time).
pub fn refused_port() -> u16 {
    for _ in 0..20 {
        let p = free_port();
        let addr: SocketAddr = ([127, 0, 0, 1], p).into();
        if let Err(e) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            && e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return p;
        }
    }
    panic!("no refusing local port found");
}

/// Picks a currently free local port.
pub fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
