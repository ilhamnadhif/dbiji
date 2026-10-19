//! Persistent connection configuration model.
//!
//! Secrets (passwords, passphrases) are never part of these structs; they live
//! in the secret store (Keychain or encrypted vault) under the keys produced by
//! [`secret_key`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Database product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    CockroachDb,
    MySql,
    MariaDb,
    Sqlite,
    SqlServer,
    ClickHouse,
    DuckDb,
    /// ANSI SQL, used when no connection is attached (e.g. a scratch editor).
    Generic,
}

impl DbKind {
    pub const ALL: [DbKind; 8] = [DbKind::Postgres, DbKind::MySql, DbKind::MariaDb, DbKind::Sqlite, DbKind::SqlServer, DbKind::CockroachDb, DbKind::ClickHouse, DbKind::DuckDb];

    pub fn display_name(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::CockroachDb => "CockroachDB",
            DbKind::MySql => "MySQL",
            DbKind::MariaDb => "MariaDB",
            DbKind::Sqlite => "SQLite",
            DbKind::SqlServer => "SQL Server",
            DbKind::ClickHouse => "ClickHouse",
            DbKind::DuckDb => "DuckDB",
            DbKind::Generic => "SQL",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::CockroachDb => Some(26257),
            DbKind::MySql | DbKind::MariaDb => Some(3306),
            DbKind::SqlServer => Some(1433),
            DbKind::ClickHouse => Some(8123),
            DbKind::Sqlite | DbKind::DuckDb | DbKind::Generic => None,
        }
    }

    pub fn default_user(self) -> Option<&'static str> {
        match self {
            DbKind::Postgres => Some("postgres"),
            DbKind::CockroachDb => Some("root"),
            DbKind::MySql | DbKind::MariaDb => Some("root"),
            DbKind::SqlServer => Some("sa"),
            DbKind::ClickHouse => Some("default"),
            _ => None,
        }
    }

    pub fn default_database(self) -> Option<&'static str> {
        match self {
            DbKind::Postgres => Some("postgres"),
            DbKind::CockroachDb => Some("defaultdb"),
            DbKind::SqlServer => Some("master"),
            DbKind::ClickHouse => Some("default"),
            _ => None,
        }
    }

    /// Embedded / file based engines need a path instead of host and port.
    pub fn is_file_based(self) -> bool {
        matches!(self, DbKind::Sqlite | DbKind::DuckDb)
    }

    /// Engines speaking the PostgreSQL wire protocol.
    pub fn is_postgres_family(self) -> bool {
        matches!(self, DbKind::Postgres | DbKind::CockroachDb)
    }

    pub fn is_mysql_family(self) -> bool {
        matches!(self, DbKind::MySql | DbKind::MariaDb)
    }

    /// Lowercase identifier used in URLs and file names.
    pub fn id(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::CockroachDb => "cockroachdb",
            DbKind::MySql => "mysql",
            DbKind::MariaDb => "mariadb",
            DbKind::Sqlite => "sqlite",
            DbKind::SqlServer => "sqlserver",
            DbKind::ClickHouse => "clickhouse",
            DbKind::DuckDb => "duckdb",
            DbKind::Generic => "generic",
        }
    }

    pub fn from_id(id: &str) -> Option<DbKind> {
        let id = id.to_ascii_lowercase();
        Some(match id.as_str() {
            "postgres" | "postgresql" | "pg" | "postgis" | "greenplum" | "alloydb" | "redshift" | "timescale" | "timescaledb" | "yugabyte" | "yugabytedb" | "supabase" | "neon" => {
                DbKind::Postgres
            }
            "cockroachdb" | "cockroach" => DbKind::CockroachDb,
            "mysql" | "mysql8" | "mysql5" | "tidb" => DbKind::MySql,
            "mariadb" => DbKind::MariaDb,
            "sqlite" | "sqlite3" => DbKind::Sqlite,
            "sqlserver" | "mssql" | "sql_server" | "microsoft" | "jtds" => DbKind::SqlServer,
            "clickhouse" => DbKind::ClickHouse,
            "duckdb" => DbKind::DuckDb,
            "generic" | "sql" => DbKind::Generic,
            _ => return None,
        })
    }
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Connection type (development / test / production or user defined) that
/// drives safety defaults and the colour marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionType {
    pub id: String,
    pub name: String,
    /// RGB colour used for the tab/status bar marker.
    pub color: [u8; 3],
    pub auto_commit: bool,
    /// Ask for confirmation before statements that change data or schema.
    pub confirm_execute: bool,
    /// Ask for confirmation before saving data changes from the grid.
    pub confirm_data_change: bool,
    /// Ask for confirmation for dangerous statements (DELETE/UPDATE without WHERE, DROP, TRUNCATE).
    pub confirm_dangerous: bool,
}

impl Default for ConnectionType {
    fn default() -> Self {
        ConnectionType::development()
    }
}

impl ConnectionType {
    pub fn development() -> Self {
        ConnectionType {
            id: "dev".into(),
            name: "Development".into(),
            color: [255, 255, 255],
            auto_commit: true,
            confirm_execute: false,
            confirm_data_change: false,
            confirm_dangerous: false,
        }
    }
    pub fn test() -> Self {
        ConnectionType {
            id: "test".into(),
            name: "Test".into(),
            color: [196, 255, 181],
            auto_commit: true,
            confirm_execute: false,
            confirm_data_change: false,
            confirm_dangerous: true,
        }
    }
    pub fn production() -> Self {
        ConnectionType {
            id: "prod".into(),
            name: "Production".into(),
            color: [247, 159, 129],
            auto_commit: false,
            confirm_execute: true,
            confirm_data_change: true,
            confirm_dangerous: true,
        }
    }
    pub fn builtins() -> Vec<ConnectionType> {
        vec![Self::development(), Self::test(), Self::production()]
    }
    pub fn is_production(&self) -> bool {
        self.id == "prod"
    }
}

/// How the database password is obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Password stored in the secret store (when `save_password`), otherwise prompted.
    #[default]
    Password,
    /// Always prompt.
    Prompt,
    /// Look the password up in `~/.pgpass` (or the file in `PGPASSFILE`).
    Pgpass,
    /// Run a shell command and use the first line of its output as password.
    Command { command: String },
    /// No password (trust/peer auth, embedded databases).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    /// Use TLS when the server offers it, without certificate verification (libpq default).
    #[default]
    Prefer,
    /// TLS required, certificate not verified.
    Require,
    /// TLS required, certificate chain verified, host name not checked.
    VerifyCa,
    /// TLS required, certificate chain and host name verified.
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 5] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull];
    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }
    pub fn parse(s: &str) -> Option<SslMode> {
        Some(match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "false" | "off" => SslMode::Disable,
            "allow" | "prefer" | "preferred" => SslMode::Prefer,
            "require" | "required" | "true" | "on" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" | "verify-identity" => SslMode::VerifyFull,
            _ => return None,
        })
    }
    pub fn uses_tls(self) -> bool {
        self != SslMode::Disable
    }
    pub fn verifies_chain(self) -> bool {
        matches!(self, SslMode::VerifyCa | SslMode::VerifyFull)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SslConfig {
    pub mode: SslMode,
    /// PEM file with CA certificates to trust in addition to (or instead of) system roots.
    pub ca_cert: Option<String>,
    /// PEM client certificate for mutual TLS.
    pub client_cert: Option<String>,
    /// PEM private key for the client certificate.
    pub client_key: Option<String>,
    /// Use only `ca_cert`, not the system / webpki roots.
    pub only_custom_ca: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SshAuth {
    #[default]
    Password,
    /// Private key file (OpenSSH / PEM); passphrase lives in the secret store.
    PublicKey { key_path: String },
    /// Keys from the running ssh-agent (`SSH_AUTH_SOCK`).
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    /// Accept unknown hosts and remember them; reject changed keys.
    #[default]
    AcceptNew,
    /// Only accept hosts already present in known_hosts.
    Strict,
    /// Accept everything (insecure).
    AcceptAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub save_credentials: bool,
    /// Fixed local port for the forward; `None` picks a free port.
    pub local_port: Option<u16>,
    pub keepalive_secs: u32,
    pub connect_timeout_secs: u32,
    pub host_key_policy: HostKeyPolicy,
}

impl Default for SshConfig {
    fn default() -> Self {
        SshConfig {
            enabled: false,
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password,
            save_credentials: true,
            local_port: None,
            keepalive_secs: 30,
            connect_timeout_secs: 15,
            host_key_policy: HostKeyPolicy::AcceptNew,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    #[default]
    Socks5,
    /// HTTP CONNECT proxy.
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Optional user; the password lives in the secret store.
    pub user: Option<String>,
    pub save_credentials: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig { enabled: false, kind: ProxyKind::Socks5, host: String::new(), port: 1080, user: None, save_credentials: true }
    }
}

/// Reusable SSH / proxy settings shared by several connections ("network profile").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkProfile {
    pub id: String,
    pub name: String,
    pub ssh: Option<SshConfig>,
    pub proxy: Option<ProxyConfig>,
}

/// Actions run when connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitConfig {
    /// SQL executed right after the session is opened (statements separated by `;`).
    pub init_sql: String,
    /// Schema (or MySQL database) made current after connecting.
    pub default_schema: Option<String>,
    /// Open a dedicated session for every SQL editor so long queries do not
    /// block the navigator.
    pub separate_editor_connection: bool,
    /// Ping interval to keep idle sessions alive; 0 disables.
    pub keep_alive_secs: u32,
    pub connect_timeout_secs: u32,
    /// Default statement timeout; 0 disables.
    pub query_timeout_secs: u32,
    /// Shell command run before connecting (e.g. start a VPN). Failure aborts the connect.
    pub shell_before_connect: Option<String>,
    /// Shell command run after disconnecting.
    pub shell_after_disconnect: Option<String>,
    /// Show every database of the server in the navigator (PostgreSQL, SQL Server).
    pub show_all_databases: bool,
    /// Show system schemas and objects.
    pub show_system_objects: bool,
    /// Rows fetched per page; `None` uses the global setting.
    pub fetch_size: Option<u32>,
}

impl Default for InitConfig {
    fn default() -> Self {
        InitConfig {
            init_sql: String::new(),
            default_schema: None,
            separate_editor_connection: true,
            keep_alive_secs: 0,
            connect_timeout_secs: 20,
            query_timeout_secs: 0,
            shell_before_connect: None,
            shell_after_disconnect: None,
            show_all_databases: false,
            show_system_objects: false,
            fetch_size: None,
        }
    }
}

/// Include / exclude patterns for one object kind. Patterns use `*` and `?`
/// wildcards and match case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NameFilter {
    pub enabled: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl NameFilter {
    pub fn is_active(&self) -> bool {
        self.enabled && (!self.include.is_empty() || !self.exclude.is_empty())
    }

    pub fn matches(&self, name: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let included = self.include.is_empty() || self.include.iter().any(|p| wildcard_match(p, name));
        included && !self.exclude.iter().any(|p| wildcard_match(p, name))
    }
}

/// Case-insensitive glob match supporting `*` and `?`.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ObjectFilters {
    pub databases: NameFilter,
    pub schemas: NameFilter,
    pub tables: NameFilter,
    pub views: NameFilter,
    pub routines: NameFilter,
    pub other: NameFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorView {
    /// Tables and views only, no folders for indexes/constraints.
    Simple,
    #[default]
    Advanced,
}

/// A configured connection, as stored in `connections.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    /// Navigator folder path, `/` separated.
    pub folder: Option<String>,
    /// Position among the connections of its folder (moved by the user);
    /// connections without one follow, by name.
    pub sort_index: Option<u32>,
    pub description: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub auth: AuthMethod,
    pub save_password: bool,
    /// Database file for SQLite / DuckDB.
    pub file_path: Option<String>,
    /// Connection type id (`dev`, `test`, `prod` or user defined).
    pub connection_type: String,
    pub read_only: bool,
    /// Overrides the connection type default when set.
    pub auto_commit: Option<bool>,
    pub ssh: Option<SshConfig>,
    pub proxy: Option<ProxyConfig>,
    /// Id of a [`NetworkProfile`] whose SSH/proxy settings are used instead of `ssh`/`proxy`.
    pub network_profile: Option<String>,
    pub ssl: SslConfig,
    pub init: InitConfig,
    /// Driver specific properties (key/value), e.g. `application_name`, `charset`.
    pub properties: BTreeMap<String, String>,
    pub filters: ObjectFilters,
    pub navigator_view: NavigatorView,
    /// Optional explicit colour overriding the connection type colour.
    pub color: Option<[u8; 3]>,
    /// Unix timestamp (seconds) of the last successful connect.
    pub last_connected: Option<i64>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        ConnectionConfig {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            kind: DbKind::Postgres,
            folder: None,
            description: None,
            host: "localhost".into(),
            port: None,
            database: None,
            user: None,
            auth: AuthMethod::Password,
            save_password: true,
            file_path: None,
            connection_type: "dev".into(),
            read_only: false,
            auto_commit: None,
            ssh: None,
            proxy: None,
            network_profile: None,
            ssl: SslConfig::default(),
            init: InitConfig::default(),
            properties: BTreeMap::new(),
            filters: ObjectFilters::default(),
            navigator_view: NavigatorView::Advanced,
            color: None,
            last_connected: None,
            sort_index: None,
        }
    }
}

impl ConnectionConfig {
    pub fn new(kind: DbKind, name: impl Into<String>) -> Self {
        ConnectionConfig {
            kind,
            name: name.into(),
            port: kind.default_port(),
            user: kind.default_user().map(str::to_string),
            database: kind.default_database().map(str::to_string),
            auth: if kind.is_file_based() { AuthMethod::None } else { AuthMethod::Password },
            ..Default::default()
        }
    }

    pub fn effective_port(&self) -> u16 {
        self.port.or(self.kind.default_port()).unwrap_or(0)
    }

    /// Short description for tooltips: `user@host:port/db` or the file path.
    pub fn summary(&self) -> String {
        if self.kind.is_file_based() {
            return self.file_path.clone().unwrap_or_default();
        }
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.host);
        s.push(':');
        s.push_str(&self.effective_port().to_string());
        if let Some(db) = &self.database {
            s.push('/');
            s.push_str(db);
        }
        s
    }

    /// Duplicate with a fresh id and a "copy" suffix.
    pub fn duplicate(&self) -> Self {
        let mut c = self.clone();
        c.id = uuid::Uuid::new_v4().to_string();
        c.name = format!("{} (copy)", self.name);
        c.last_connected = None;
        c
    }
}

/// Which secret of a connection a key refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    Password,
    SshPassword,
    SshPassphrase,
    ProxyPassword,
}

/// Key under which a connection secret is stored in the secret store.
pub fn secret_key(connection_id: &str, slot: SecretSlot) -> String {
    let suffix = match slot {
        SecretSlot::Password => "password",
        SecretSlot::SshPassword => "ssh-password",
        SecretSlot::SshPassphrase => "ssh-passphrase",
        SecretSlot::ProxyPassword => "proxy-password",
    };
    format!("conn:{connection_id}:{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard() {
        assert!(wildcard_match("pg_*", "pg_catalog"));
        assert!(wildcard_match("*log", "pg_catalog"));
        assert!(wildcard_match("t?st", "TEST"));
        assert!(!wildcard_match("pg_*", "public"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn filter() {
        let f = NameFilter { enabled: true, include: vec!["app_*".into()], exclude: vec!["*_old".into()] };
        assert!(f.matches("app_users"));
        assert!(!f.matches("app_users_old"));
        assert!(!f.matches("other"));
    }

    #[test]
    fn config_roundtrip() {
        let c = ConnectionConfig::new(DbKind::MySql, "local");
        let s = serde_json::to_string(&c).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
        assert_eq!(back.effective_port(), 3306);
        // unknown / missing fields fall back to defaults
        let partial: ConnectionConfig = serde_json::from_str(r#"{"name":"x","kind":"sqlite"}"#).unwrap();
        assert_eq!(partial.kind, DbKind::Sqlite);
    }
}
