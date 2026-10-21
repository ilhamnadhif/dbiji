//! Connection management: configured connections, live sessions, tunnels,
//! credentials and per-connection metadata caches.

pub mod meta;
pub mod session;

use crate::backend::{Backend, Job, Loadable};
use dbiji_config::ConnectionStore;
use dbiji_core::config::{AuthMethod, ConnectionConfig, ConnectionType, SecretSlot};
use dbiji_core::driver::{ConnectParams, Driver, ServerInfo, TransactionMode};
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{DbError, DbKind, DbResult, Dialect, Secret};
use meta::MetadataCache;
use session::SessionHandle;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where secrets come from (Keychain or encrypted vault).
pub trait SecretSource: Send + Sync {
    fn get(&self, key: &str) -> DbResult<Option<Secret>>;
    fn set(&self, key: &str, value: &Secret) -> DbResult<()>;
    fn delete(&self, key: &str) -> DbResult<()>;
    /// True while the store must be unlocked (encrypted vault).
    fn is_locked(&self) -> bool {
        false
    }
}

/// Opens SSH tunnels / proxy forwarders for a connection.
#[async_trait::async_trait]
pub trait NetConnector: Send + Sync {
    /// Returns `None` when the connection needs no tunnel.
    async fn open(&self, cfg: &ConnectionConfig, creds: &Credentials, host: &str, port: u16) -> DbResult<Option<TunnelGuard>>;
}

/// A running tunnel; dropping it stops forwarding.
pub struct TunnelGuard {
    pub local_host: String,
    pub local_port: u16,
    pub alive: Box<dyn Fn() -> bool + Send + Sync>,
    pub guard: Box<dyn std::any::Any + Send + Sync>,
}

/// Credentials collected before connecting (from the secret store or prompts).
#[derive(Clone, Default)]
pub struct Credentials {
    pub password: Option<Secret>,
    pub ssh_password: Option<Secret>,
    pub ssh_passphrase: Option<Secret>,
    pub proxy_password: Option<Secret>,
}

/// Something the user must enter before a connection can be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptField {
    Password,
    SshPassword,
    SshPassphrase,
    ProxyPassword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
    /// The connection broke after being established.
    Lost,
}

/// A connected connection.
pub struct LiveConnection {
    pub config: ConnectionConfig,
    pub conn_type: ConnectionType,
    pub driver: Arc<dyn Driver>,
    pub dialect: Arc<dyn Dialect>,
    /// Effective parameters (local tunnel endpoint, password) used to open more sessions.
    params: ConnectParams,
    tunnel: Option<TunnelGuard>,
    /// Session used by the navigator and metadata loading.
    pub main: SessionHandle,
    /// Sessions for other databases of the server (PostgreSQL / SQL Server).
    pub db_sessions: HashMap<String, Loadable<SessionHandle>>,
    /// Sessions opened for tabs (editors, data tabs): closed on disconnect too,
    /// so that none keeps running with the old settings, transactions or locks.
    tab_sessions: Arc<parking_lot::Mutex<Vec<crate::conn::session::WeakSession>>>,
    /// A ping of `main` after another session of the connection was lost (is the
    /// server gone or just that session?).
    health_probe: Option<Job<DbResult<()>>>,
    pub server: ServerInfo,
    pub meta: MetadataCache,
    pub connected_at: Instant,
    pub current_database: Option<String>,
    pub current_schema: Option<String>,
    /// Server-level object folders of the dialect (roles, settings …).
    pub catalog_folders: Arc<Vec<dbiji_core::catalog::CatalogFolder>>,
    /// Stops the keep-alive task when the connection is dropped.
    _keepalive: KeepAlive,
}

/// Background keep-alive: pings the main session every `keep_alive_secs`
/// on the tokio runtime, independent of UI frames (the UI does not repaint
/// while idle). Cancelled when dropped.
struct KeepAlive(tokio_util::sync::CancellationToken);

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl KeepAlive {
    fn start(backend: &Backend, session: SessionHandle, secs: u32) -> KeepAlive {
        let token = tokio_util::sync::CancellationToken::new();
        if secs > 0 {
            let t = token.clone();
            backend.spawn_detached(async move {
                let period = Duration::from_secs(secs.max(5) as u64);
                loop {
                    tokio::select! {
                        _ = t.cancelled() => break,
                        _ = tokio::time::sleep(period) => {}
                    }
                    if session.is_closed() {
                        break;
                    }
                    if let Err(e) = session.ping().await {
                        tracing::warn!("keep-alive failed: {e}");
                    }
                }
            });
        }
        KeepAlive(token)
    }
}

impl LiveConnection {
    /// A catalog folder of this connection's dialect.
    pub fn catalog_folder(&self, id: &str) -> Option<&dbiji_core::catalog::CatalogFolder> {
        self.catalog_folders.iter().find(|f| f.id == id)
    }

    pub fn kind(&self) -> DbKind {
        self.config.kind
    }

    pub fn tunnel_alive(&self) -> bool {
        self.tunnel.as_ref().is_none_or(|t| (t.alive)())
    }

    pub fn params(&self) -> &ConnectParams {
        &self.params
    }

    /// The server has several databases, each needing its own session (PostgreSQL).
    pub fn multiple_databases(&self) -> bool {
        self.driver.info().capabilities.multiple_databases
    }

    /// Session for `database` (`None` or the current database → main session).
    pub fn session_for(&self, database: Option<&str>) -> Option<SessionHandle> {
        match database {
            None => Some(self.main.clone()),
            Some(db) if Some(db) == self.current_database.as_deref() => Some(self.main.clone()),
            Some(db) => {
                if !self.driver.info().capabilities.multiple_databases {
                    return Some(self.main.clone());
                }
                self.db_sessions.get(db).and_then(|l| l.get()).cloned()
            }
        }
    }

    /// Make sure a session for `database` is being opened; returns it once ready.
    pub fn ensure_session(&mut self, backend: &Backend, database: Option<&str>) -> Option<SessionHandle> {
        if let Some(s) = self.session_for(database) {
            return Some(s);
        }
        let db = database?.to_string();
        let entry = self.db_sessions.entry(db.clone()).or_default();
        entry.poll();
        if entry.is_not_loaded() {
            let driver = self.driver.clone();
            let mut params = self.params.clone();
            params.database = Some(db.clone());
            let cfg = self.config.clone();
            let label = format!("{} / {db}", cfg.name);
            *entry = Loadable::Loading(backend.spawn(async move {
                let mut s = driver.connect(&params).await?;
                init_session(s.as_mut(), &cfg, &params).await?;
                Ok(SessionHandle::new(s, label, Some(db)))
            }));
        }
        None
    }

    /// When a session of a tab or of another database lost its connection (the data
    /// tab's insert failed with "connection closed"), ping the main session: if the
    /// server is gone, the connection shows as lost (red) instead of connected.
    fn check_health(&mut self, backend: &Backend) {
        if let Some(job) = self.health_probe.as_mut()
            && job.poll().is_some()
        {
            self.health_probe = None;
        }
        // Each closed session is looked at once: tab sessions are forgotten, a closed
        // session of another database is opened again on its next use.
        let mut lost = {
            let mut list = self.tab_sessions.lock();
            let n = list.iter().filter_map(|w| w.upgrade()).filter(|s| s.is_lost()).count();
            list.retain(|w| w.upgrade().is_some_and(|s| !s.is_closed()));
            n
        };
        self.db_sessions.retain(|_, l| match l.get() {
            Some(s) if s.is_closed() => {
                lost += usize::from(s.is_lost());
                false
            }
            _ => true,
        });
        if lost > 0 && self.health_probe.is_none() && !self.main.is_closed() {
            let main = self.main.clone();
            self.health_probe = Some(backend.spawn(async move { main.ping().await }));
        }
    }

    /// Why the session for `database` could not be opened (shown instead of "Loading…").
    pub fn session_error(&self, database: Option<&str>) -> Option<String> {
        self.db_sessions.get(database?).and_then(|l| l.error()).map(|e| e.full_text())
    }

    /// Forget sessions that failed to open, so that the next use tries again.
    pub fn retry_failed_sessions(&mut self) {
        self.db_sessions.retain(|_, l| l.error().is_none());
    }

    /// Open an additional session (separate connection per editor, FR-C7).
    pub fn open_session_job(&self, backend: &Backend, label: String, database: Option<String>) -> Job<DbResult<SessionHandle>> {
        let driver = self.driver.clone();
        let mut params = self.params.clone();
        if database.is_some() {
            params.database = database.clone();
        }
        let cfg = self.config.clone();
        let tab_sessions = self.tab_sessions.clone();
        backend.spawn(async move {
            let mut s = driver.connect(&params).await?;
            init_session(s.as_mut(), &cfg, &params).await?;
            let handle = SessionHandle::new(s, label, database);
            // Weak: a session its tab dropped (closed while opening) closes instead of staying open.
            let mut list = tab_sessions.lock();
            list.retain(|w| w.upgrade().is_some_and(|s| !s.is_closed()));
            list.push(handle.downgrade());
            Ok(handle)
        })
    }
}

/// Apply transaction mode, default schema and init SQL after connecting.
pub async fn init_session(s: &mut dyn dbiji_core::Session, cfg: &ConnectionConfig, params: &ConnectParams) -> DbResult<()> {
    if !params.auto_commit {
        s.set_transaction_mode(TransactionMode::Manual).await?;
    }
    if let Some(schema) = cfg.init.default_schema.as_deref().filter(|s| !s.is_empty())
        && let Err(e) = s.set_current_schema(schema).await
    {
        tracing::warn!("cannot set default schema: {e}");
    }
    for stmt in split_simple(&cfg.init.init_sql) {
        dbiji_core::driver::execute_update(s, &stmt, &[]).await.map_err(|e| e.with_context("Initialization SQL"))?;
    }
    Ok(())
}

/// Minimal `;` splitter for initialization scripts (quotes aware).
pub fn split_simple(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' => {
                    if !cur.trim().is_empty() {
                        out.push(cur.trim().to_string());
                    }
                    cur.clear();
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

struct Connected {
    session: Box<dyn dbiji_core::Session>,
    params: ConnectParams,
    tunnel: Option<TunnelGuard>,
    server: ServerInfo,
    database: Option<String>,
    schema: Option<String>,
}

/// Runs a shell command (pre-connect hook, password command).
pub async fn run_shell(command: &str, timeout: Duration) -> DbResult<String> {
    dbiji_net::run_shell_command(command, timeout).await
}

async fn connect_task(
    driver: Arc<dyn Driver>,
    cfg: ConnectionConfig,
    conn_type: ConnectionType,
    creds: Credentials,
    net: Arc<dyn NetConnector>,
    pgpass: Option<Arc<dyn Fn(&str, u16, &str, &str) -> Option<Secret> + Send + Sync>>,
) -> DbResult<Connected> {
    if let Some(cmd) = cfg.init.shell_before_connect.as_deref().filter(|c| !c.trim().is_empty()) {
        run_shell(cmd, Duration::from_secs(60)).await.map_err(|e| e.with_context("Shell command before connect"))?;
    }
    let password = match &cfg.auth {
        AuthMethod::Password | AuthMethod::Prompt => creds.password.clone(),
        AuthMethod::Pgpass => {
            let user = cfg.user.clone().unwrap_or_default();
            let db = cfg.database.clone().unwrap_or_default();
            pgpass.as_ref().and_then(|f| f(&cfg.host, cfg.effective_port(), &db, &user))
        }
        AuthMethod::Command { command } => Some(dbiji_net::password_from_command(command, Duration::from_secs(30)).await.map_err(|e| e.with_context("Password command"))?),
        AuthMethod::None => None,
    };
    let mut params = ConnectParams::from_config(&cfg, password);
    params.auto_commit = cfg.auto_commit.unwrap_or(conn_type.auto_commit);
    // An in-memory database is one per connection, not one per session (editor, data tab, navigator).
    if matches!(cfg.kind, DbKind::Sqlite | DbKind::DuckDb) && params.file_path.as_deref().is_some_and(|p| p.as_os_str() == ":memory:") {
        params.file_path = Some(PathBuf::from(format!(":memory:{}", cfg.id)));
    }
    let mut tunnel = None;
    if !cfg.kind.is_file_based()
        && let Some(t) = net.open(&cfg, &creds, &cfg.host, cfg.effective_port()).await?
    {
        params.tls_server_name = Some(cfg.host.clone());
        params.host = t.local_host.clone();
        params.port = t.local_port;
        tunnel = Some(t);
    }
    let timeout = params.connect_timeout + Duration::from_secs(5);
    let mut session =
        tokio::time::timeout(timeout, driver.connect(&params)).await.map_err(|_| DbError::timeout(format!("Connection timed out after {}s", timeout.as_secs())))??;
    init_session(session.as_mut(), &cfg, &params).await?;
    let database = session.current_database().await.ok().flatten();
    let schema = session.current_schema().await.ok().flatten();
    let server = session.server_info().clone();
    Ok(Connected { session, params, tunnel, server, database, schema })
}

/// A connection opened without the UI (tasks, `dbiji --run-task`). Dropping
/// it closes the tunnel; close the session first.
pub struct HeadlessConnection {
    pub session: Box<dyn dbiji_core::Session>,
    pub params: ConnectParams,
    pub driver: Arc<dyn Driver>,
    pub config: ConnectionConfig,
    _tunnel: Option<TunnelGuard>,
}

/// Connect to a saved connection with saved credentials only (no prompts).
pub async fn connect_saved(registry: &DriverRegistry, store: &ConnectionStore, secrets: &dyn SecretSource, net: Arc<dyn NetConnector>, id: &str) -> DbResult<HeadlessConnection> {
    let cfg = store.get(id).cloned().ok_or_else(|| DbError::config(format!("Unknown connection {id}")))?;
    if matches!(cfg.auth, AuthMethod::Prompt) {
        return Err(DbError::auth(format!("The connection \"{}\" asks for its password at every connect; save the password to use it in tasks", cfg.name)));
    }
    let driver = registry.get(cfg.kind).ok_or_else(|| DbError::unsupported(format!("No driver for {}", cfg.kind)))?;
    let net_cfg = store.effective_network(&cfg);
    // Only read the secrets this connection uses (each read may hit the OS keychain).
    let get = |slot, used: bool| if used { secrets.get(&store.network_secret_key(&cfg, slot)).ok().flatten() } else { None };
    let password_used = !cfg.kind.is_file_based() && matches!(cfg.auth, AuthMethod::Password);
    let creds = Credentials {
        password: get(SecretSlot::Password, password_used),
        ssh_password: get(SecretSlot::SshPassword, net_cfg.ssh.is_some()),
        ssh_passphrase: get(SecretSlot::SshPassphrase, net_cfg.ssh.is_some()),
        proxy_password: get(SecretSlot::ProxyPassword, net_cfg.proxy.is_some()),
    };
    let mut eff = cfg.clone();
    eff.ssh = net_cfg.ssh;
    eff.proxy = net_cfg.proxy;
    let conn_type = store.connection_type(&cfg.connection_type);
    let pgpass: Option<Arc<dyn Fn(&str, u16, &str, &str) -> Option<Secret> + Send + Sync>> =
        Some(Arc::new(|h: &str, p: u16, d: &str, u: &str| dbiji_config::import::pgpass::lookup(h, p, d, u)));
    let c = connect_task(driver.clone(), eff, conn_type, creds, net, pgpass).await?;
    Ok(HeadlessConnection { session: c.session, params: c.params, driver, config: cfg, _tunnel: c.tunnel })
}

/// Configured + live connections.
pub struct ConnectionManager {
    pub registry: DriverRegistry,
    /// Configured connections, folders, connection types and network profiles.
    pub store: ConnectionStore,
    live: HashMap<String, LiveConnection>,
    connecting: HashMap<String, Job<DbResult<Connected>>>,
    failed: HashMap<String, String>,
    /// Connect errors already reported (their connection stays red until it connects).
    failed_shown: HashSet<String>,
    pub secrets: Arc<dyn SecretSource>,
    pub net: Arc<dyn NetConnector>,
    pub pgpass: Option<Arc<dyn Fn(&str, u16, &str, &str) -> Option<Secret> + Send + Sync>>,
    /// Ids whose connect finished this frame (for UI reactions like expanding the tree).
    pub just_connected: Vec<String>,
    /// The store changed and should be written to disk.
    pub dirty: bool,
}

impl ConnectionManager {
    pub fn new(registry: DriverRegistry, secrets: Arc<dyn SecretSource>, net: Arc<dyn NetConnector>) -> Self {
        ConnectionManager {
            registry,
            store: ConnectionStore::new(),
            live: HashMap::new(),
            connecting: HashMap::new(),
            failed: HashMap::new(),
            failed_shown: HashSet::new(),
            secrets,
            net,
            pgpass: None,
            just_connected: Vec::new(),
            dirty: false,
        }
    }

    pub fn configs(&self) -> &[ConnectionConfig] {
        &self.store.connections
    }

    pub fn folders(&self) -> &[String] {
        &self.store.folders
    }

    /// Built-in and user defined connection types.
    pub fn connection_types(&self) -> Vec<ConnectionType> {
        self.store.all_connection_types()
    }

    pub fn config(&self, id: &str) -> Option<&ConnectionConfig> {
        self.store.get(id)
    }

    pub fn config_mut(&mut self, id: &str) -> Option<&mut ConnectionConfig> {
        self.dirty = true;
        self.store.get_mut(id)
    }

    pub fn connection_type(&self, id: &str) -> ConnectionType {
        self.store.connection_type(id)
    }

    pub fn type_of(&self, conn_id: &str) -> ConnectionType {
        self.config(conn_id).map(|c| self.connection_type(&c.connection_type)).unwrap_or_default()
    }

    /// Marker colour of a connection (explicit colour or connection type colour).
    pub fn color_of(&self, conn_id: &str) -> Option<[u8; 3]> {
        self.config(conn_id).map(|c| self.store.color(c))
    }

    pub fn upsert(&mut self, cfg: ConnectionConfig) {
        let r = if self.store.get(&cfg.id).is_some() { self.store.update(cfg) } else { self.store.add(cfg).map(|_| ()) };
        if let Err(e) = r {
            tracing::warn!("cannot store connection: {e}");
        }
        self.dirty = true;
    }

    pub fn remove(&mut self, id: &str) {
        self.disconnect_now(id);
        self.store.remove(id);
        for key in ConnectionStore::secret_keys_for(id) {
            let _ = self.secrets.delete(&key);
        }
        self.dirty = true;
    }

    /// Configuration with the effective SSH / proxy settings (network profile) applied.
    pub fn effective_config(&self, cfg: &ConnectionConfig) -> ConnectionConfig {
        let net = self.store.effective_network(cfg);
        let mut c = cfg.clone();
        c.ssh = net.ssh;
        c.proxy = net.proxy;
        c
    }

    fn secret_for(&self, cfg: &ConnectionConfig, slot: SecretSlot) -> Option<Secret> {
        self.secrets.get(&self.store.network_secret_key(cfg, slot)).ok().flatten()
    }

    pub fn status(&self, id: &str) -> ConnStatus {
        if self.connecting.contains_key(id) {
            return ConnStatus::Connecting;
        }
        if let Some(l) = self.live.get(id) {
            if l.main.is_closed() || !l.tunnel_alive() {
                return ConnStatus::Lost;
            }
            return ConnStatus::Connected;
        }
        if let Some(e) = self.failed.get(id) {
            return ConnStatus::Failed(e.clone());
        }
        ConnStatus::Disconnected
    }

    pub fn is_connected(&self, id: &str) -> bool {
        self.live.contains_key(id)
    }

    pub fn is_connecting(&self) -> bool {
        !self.connecting.is_empty()
    }

    /// Connect with a draft configuration and disconnect again ("Test connection").
    /// Returns a short server description on success.
    pub fn test_connection(&self, backend: &Backend, cfg: ConnectionConfig, prompted: Credentials) -> Job<DbResult<String>> {
        let driver = self.registry.get(cfg.kind);
        let conn_type = self.connection_type(&cfg.connection_type);
        let creds = self.credentials_for(&cfg, &prompted);
        let cfg = self.effective_config(&cfg);
        let net = self.net.clone();
        let pgpass = self.pgpass.clone();
        backend.spawn(async move {
            let driver = driver.ok_or_else(|| DbError::unsupported(format!("No driver for {}", cfg.kind)))?;
            let started = Instant::now();
            let mut c = connect_task(driver, cfg, conn_type, creds, net, pgpass).await?;
            let info = format!("{} {} ({} ms)", c.server.product, c.server.version.split_whitespace().next().unwrap_or(""), started.elapsed().as_millis());
            let _ = c.session.close().await;
            drop(c.tunnel.take());
            Ok(info)
        })
    }

    pub fn live(&self, id: &str) -> Option<&LiveConnection> {
        self.live.get(id)
    }

    pub fn live_mut(&mut self, id: &str) -> Option<&mut LiveConnection> {
        self.live.get_mut(id)
    }

    pub fn live_ids(&self) -> Vec<String> {
        self.live.keys().cloned().collect()
    }

    /// Which secrets must be prompted for because they are not saved.
    pub fn missing_credentials(&self, id: &str) -> Vec<PromptField> {
        let Some(cfg) = self.config(id) else { return Vec::new() };
        let mut out = Vec::new();
        let saved = |slot| self.secret_for(cfg, slot).is_some();
        match cfg.auth {
            AuthMethod::Prompt => out.push(PromptField::Password),
            AuthMethod::Password if !cfg.kind.is_file_based() && !(cfg.save_password && saved(SecretSlot::Password)) => out.push(PromptField::Password),
            _ => {}
        }
        let net = self.store.effective_network(cfg);
        if let Some(ssh) = net.ssh.as_ref() {
            match &ssh.auth {
                dbiji_core::config::SshAuth::Password if !(ssh.save_credentials && saved(SecretSlot::SshPassword)) => out.push(PromptField::SshPassword),
                _ => {}
            }
        }
        if let Some(p) = net.proxy.as_ref().filter(|p| p.user.is_some())
            && !(p.save_credentials && saved(SecretSlot::ProxyPassword))
        {
            out.push(PromptField::ProxyPassword);
        }
        out
    }

    /// Collect saved credentials, overlaid with the prompted ones.
    pub fn credentials(&self, id: &str, prompted: &Credentials) -> Credentials {
        match self.config(id) {
            Some(cfg) => self.credentials_for(cfg, prompted),
            None => prompted.clone(),
        }
    }

    /// Saved credentials of `cfg` (also an unsaved draft), overlaid with the prompted ones.
    pub fn credentials_for(&self, cfg: &ConnectionConfig, prompted: &Credentials) -> Credentials {
        // Only read the secrets the connection can use (each read may hit the OS keychain).
        let net = self.store.effective_network(cfg);
        let get = |slot, used: bool| if used { self.secret_for(cfg, slot) } else { None };
        let password_used = !cfg.kind.is_file_based() && matches!(cfg.auth, AuthMethod::Password | AuthMethod::Prompt);
        Credentials {
            password: prompted.password.clone().or_else(|| get(SecretSlot::Password, password_used)),
            ssh_password: prompted.ssh_password.clone().or_else(|| get(SecretSlot::SshPassword, net.ssh.is_some())),
            ssh_passphrase: prompted.ssh_passphrase.clone().or_else(|| get(SecretSlot::SshPassphrase, net.ssh.is_some())),
            proxy_password: prompted.proxy_password.clone().or_else(|| get(SecretSlot::ProxyPassword, net.proxy.is_some())),
        }
    }

    /// Start connecting with the given (prompted) credentials.
    pub fn connect(&mut self, backend: &Backend, id: &str, prompted: Credentials) -> DbResult<()> {
        if self.live.contains_key(id) || self.connecting.contains_key(id) {
            return Ok(());
        }
        let cfg = self.config(id).map(|c| self.effective_config(c)).ok_or_else(|| DbError::config("Unknown connection"))?;
        let driver = self.registry.get(cfg.kind).ok_or_else(|| DbError::unsupported(format!("No driver for {}", cfg.kind)))?;
        let conn_type = self.connection_type(&cfg.connection_type);
        let creds = self.credentials(id, &prompted);
        let net = self.net.clone();
        let pgpass = self.pgpass.clone();
        self.failed.remove(id);
        self.failed_shown.remove(id);
        self.connecting.insert(id.to_string(), backend.spawn(connect_task(driver, cfg, conn_type, creds, net, pgpass)));
        Ok(())
    }

    /// Poll connect jobs and keep-alive. Call once per frame.
    pub fn poll(&mut self, backend: &Backend) {
        self.just_connected.clear();
        let ids: Vec<String> = self.connecting.keys().cloned().collect();
        for id in ids {
            let done = self.connecting.get_mut(&id).and_then(|j| j.poll());
            if let Some(res) = done {
                self.connecting.remove(&id);
                match res {
                    Ok(c) => {
                        let Some(cfg) = self.config(&id).cloned() else { continue };
                        let Some(driver) = self.registry.get(cfg.kind) else { continue };
                        // The session's dialect: it knows the server (MySQL / MariaDB version,
                        // CockroachDB) and the connection's settings (SQLite foreign keys).
                        let dialect = c.session.dialect();
                        let catalog_folders = Arc::new(dialect.catalog_folders());
                        let conn_type = self.connection_type(&cfg.connection_type);
                        let main = SessionHandle::new(c.session, format!("{} (main)", cfg.name), c.database.clone());
                        let keepalive = KeepAlive::start(backend, main.clone(), cfg.init.keep_alive_secs);
                        if let Some(cfg) = self.config_mut(&id) {
                            cfg.last_connected = Some(chrono::Utc::now().timestamp());
                        }
                        self.live.insert(
                            id.clone(),
                            LiveConnection {
                                config: cfg,
                                conn_type,
                                driver,
                                dialect,
                                params: c.params,
                                tunnel: c.tunnel,
                                main,
                                db_sessions: HashMap::new(),
                                tab_sessions: Arc::new(parking_lot::Mutex::new(Vec::new())),
                                health_probe: None,
                                server: c.server,
                                meta: MetadataCache::default(),
                                connected_at: Instant::now(),
                                current_database: c.database,
                                current_schema: c.schema,
                                catalog_folders,
                                _keepalive: keepalive,
                            },
                        );
                        self.just_connected.push(id);
                    }
                    Err(e) => {
                        self.failed_shown.remove(&id);
                        self.failed.insert(id, e.full_text());
                    }
                }
            }
        }
        for live in self.live.values_mut() {
            live.meta.poll();
            for s in live.db_sessions.values_mut() {
                s.poll();
            }
            live.check_health(backend);
        }
    }

    /// `s` is a connection's own session (main or per database), not a tab's: closing a
    /// tab that uses it ends no transaction.
    pub fn is_shared_session(&self, s: &SessionHandle) -> bool {
        self.live.values().any(|l| l.main.same(s) || l.db_sessions.values().any(|d| d.get().is_some_and(|d| d.same(s))))
    }

    /// Take the connect error of `id` (shown once by the UI).
    pub fn take_error(&mut self, id: &str) -> Option<String> {
        let e = self.failed.get(id).cloned();
        self.failed_shown.insert(id.to_string());
        e
    }

    pub fn errors(&self) -> Vec<(String, String)> {
        self.failed.iter().filter(|(k, _)| !self.failed_shown.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Disconnect, closing all sessions in the background.
    pub fn disconnect(&mut self, backend: &Backend, id: &str) {
        if let Some(live) = self.live.remove(id) {
            backend.spawn_detached(close_live(live));
        }
        self.connecting.remove(id);
    }

    fn disconnect_now(&mut self, id: &str) {
        self.live.remove(id);
        self.connecting.remove(id);
    }

    pub fn disconnect_all(&mut self, backend: &Backend) {
        for id in self.live_ids() {
            self.disconnect(backend, &id);
        }
    }

    /// Disconnect everything and wait (at most `timeout`): DBiji is quitting, and a
    /// detached task would not finish (sessions, tunnels, the after-disconnect commands).
    pub fn disconnect_all_and_wait(&mut self, backend: &Backend, timeout: Duration) {
        let lives: Vec<LiveConnection> = self.live_ids().iter().filter_map(|id| self.live.remove(id)).collect();
        self.connecting.clear();
        if lives.is_empty() {
            return;
        }
        backend.block_on(async move {
            let _ = tokio::time::timeout(timeout, async move {
                for live in lives {
                    close_live(live).await;
                }
            })
            .await;
        });
    }
}

/// Close a connection's sessions and tunnel, then run its after-disconnect command.
async fn close_live(live: LiveConnection) {
    let after = live.config.init.shell_after_disconnect.clone();
    live.main.cancel_and_close().await;
    for s in live.db_sessions.values() {
        if let Some(s) = s.get() {
            s.cancel_and_close().await;
        }
    }
    let tab_sessions: Vec<SessionHandle> = std::mem::take(&mut *live.tab_sessions.lock()).iter().filter_map(|w| w.upgrade()).collect();
    for s in tab_sessions {
        s.cancel_and_close().await;
    }
    drop(live.tunnel);
    if let Some(cmd) = after.filter(|c| !c.trim().is_empty())
        && let Err(e) = run_shell(&cmd, Duration::from_secs(60)).await
    {
        tracing::warn!("shell command after disconnect failed: {e}");
    }
}

/// Secret store that keeps everything in memory (tests, or when the real store is locked).
#[derive(Default)]
pub struct MemorySecrets {
    map: parking_lot::Mutex<HashMap<String, Secret>>,
}

impl SecretSource for MemorySecrets {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        Ok(self.map.lock().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.map.lock().insert(key.to_string(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        self.map.lock().remove(key);
        Ok(())
    }
}

/// Secret source backed by the configured Keychain / encrypted vault.
pub struct ManagedSecrets(pub Arc<parking_lot::RwLock<dbiji_config::SecretsManager>>);

impl SecretSource for ManagedSecrets {
    fn get(&self, key: &str) -> DbResult<Option<Secret>> {
        self.0.read().get(key)
    }
    fn set(&self, key: &str, value: &Secret) -> DbResult<()> {
        self.0.read().set(key, value)
    }
    fn delete(&self, key: &str) -> DbResult<()> {
        self.0.read().delete(key)
    }
    fn is_locked(&self) -> bool {
        self.0.read().is_locked()
    }
}

/// Opens SSH tunnels and SOCKS5 / HTTP proxies through `dbiji-net`
/// (the connection's settings are already the effective ones).
pub struct TunnelNet {
    pub known_hosts: PathBuf,
}

#[async_trait::async_trait]
impl NetConnector for TunnelNet {
    async fn open(&self, cfg: &ConnectionConfig, creds: &Credentials, host: &str, port: u16) -> DbResult<Option<TunnelGuard>> {
        let ssh = cfg.ssh.clone().filter(|s| s.enabled);
        let proxy = cfg.proxy.clone().filter(|p| p.enabled);
        if ssh.is_none() && proxy.is_none() {
            return Ok(None);
        }
        let mut spec = dbiji_net::TunnelSpec::new(host, port, self.known_hosts.clone());
        if let Some(p) = proxy {
            spec = spec.with_proxy(p, creds.proxy_password.clone());
        }
        if let Some(s) = ssh {
            spec = spec.with_ssh(s, creds.ssh_password.clone(), creds.ssh_passphrase.clone());
        }
        let tunnel = Arc::new(dbiji_net::open_tunnel(spec).await?);
        let alive = tunnel.clone();
        Ok(Some(TunnelGuard { local_host: tunnel.local_host(), local_port: tunnel.local_port(), alive: Box::new(move || alive.is_alive()), guard: Box::new(tunnel) }))
    }
}

/// Connector for direct connections only (tests).
pub struct DirectNet;

#[async_trait::async_trait]
impl NetConnector for DirectNet {
    async fn open(&self, cfg: &ConnectionConfig, _creds: &Credentials, _host: &str, _port: u16) -> DbResult<Option<TunnelGuard>> {
        if cfg.ssh.as_ref().is_some_and(|s| s.enabled) || cfg.proxy.as_ref().is_some_and(|p| p.enabled) {
            return Err(DbError::unsupported("SSH tunnels and proxies are not available in this build"));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sqlite_session(label: &str) -> SessionHandle {
        let mut cfg = ConnectionConfig::new(DbKind::Sqlite, label);
        cfg.file_path = Some(":memory:".into());
        let s = dbiji_driver_sqlite::SqliteDriver::new().connect(&ConnectParams::from_config(&cfg, None)).await.unwrap();
        SessionHandle::new(s, label, None)
    }

    #[tokio::test]
    async fn a_connection_ended_underneath_is_lost() {
        // The server ends the connection (idle timeout, restart): no error was seen yet.
        let h = sqlite_session("a").await;
        assert!(!h.is_closed());
        h.lock().await.close().await.unwrap();
        assert!(h.is_closed() && h.is_lost());
        // Closed by DBiji: closed, not lost.
        let h = sqlite_session("b").await;
        h.close().await;
        assert!(h.is_closed() && !h.is_lost());
    }

    #[test]
    fn simple_split() {
        assert_eq!(split_simple("set a=1; select ';'; \n"), vec!["set a=1".to_string(), "select ';'".to_string()]);
        assert!(split_simple("  ").is_empty());
    }
}
