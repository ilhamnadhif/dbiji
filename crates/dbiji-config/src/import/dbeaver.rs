//! Import of DBeaver connections (FR-C8).
//!
//! Reads every project of a DBeaver workspace (`workspace6/<project>/.dbeaver/`):
//! `data-sources*.json` (connections, folders, connection types) and the
//! encrypted `credentials-config.json` (users and passwords). Legacy
//! `.dbeaver-data-sources*.xml` files of old DBeaver versions are read for
//! connection settings (their passwords are not imported).
//!
//! `credentials-config.json` is AES-128-CBC encrypted with DBeaver's
//! well-known default key; the first 16 bytes of the file are the IV, PKCS#7
//! padding. The plaintext is
//! `{"<connection-id>": {"#connection": {"user": "...", "password": "..."},
//! "network/ssh_tunnel": {...}}}`.
//!
//! The application stores the returned [`ImportResult::secrets`] in its
//! secret store; nothing secret is written anywhere by this module.

use super::ImportResult;
use super::url::parse_connection_url;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use dbiji_core::config::{
    AuthMethod, ConnectionConfig, ConnectionType, DbKind, NavigatorView, ProxyConfig, ProxyKind,
    SecretSlot, SshAuth, SshConfig, SslMode, secret_key,
};
use dbiji_core::{DbError, DbResult, Secret};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// DBeaver's default (public) key for `credentials-config.json`.
pub const DBEAVER_CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// User / password of one credentials section.
#[derive(Debug, Clone, Default)]
pub struct DbeaverCredential {
    /// User name.
    pub user: Option<String>,
    /// Password.
    pub password: Option<Secret>,
}

/// Decrypted `credentials-config.json`: connection id → section
/// (`#connection`, `network/ssh_tunnel` ...) → credential.
#[derive(Debug, Clone, Default)]
pub struct DbeaverCredentials {
    /// Credentials by connection id and section.
    pub entries: BTreeMap<String, BTreeMap<String, DbeaverCredential>>,
}

impl DbeaverCredentials {
    fn get(&self, connection: &str, section: &str) -> Option<&DbeaverCredential> {
        self.entries.get(connection)?.get(section)
    }
}

/// Wrapper deserializing a JSON string straight into a [`Secret`].
struct SecretField(Secret);

impl<'de> Deserialize<'de> for SecretField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SecretField(Secret::from(String::deserialize(d)?)))
    }
}

#[derive(Deserialize)]
struct RawCredential {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<SecretField>,
}

/// Decrypts a `credentials-config.json` file content.
pub fn decrypt_credentials(bytes: &[u8]) -> DbResult<DbeaverCredentials> {
    if bytes.len() < 32 || !bytes.len().is_multiple_of(16) {
        return Err(DbError::config(
            "credentials-config.json is not a DBeaver credentials file",
        ));
    }
    let (iv, ciphertext) = bytes.split_at(16);
    let dec = Aes128CbcDec::new_from_slices(&DBEAVER_CREDENTIALS_KEY, iv)
        .map_err(|_| DbError::internal("Invalid AES key or IV length"))?;
    let plaintext = Zeroizing::new(dec.decrypt_padded_vec::<Pkcs7>(ciphertext).map_err(|_| {
        DbError::config("credentials-config.json could not be decrypted (DBeaver may use a custom key or the secure storage)")
    })?);
    parse_credentials_json(&plaintext)
}

fn parse_credentials_json(plaintext: &[u8]) -> DbResult<DbeaverCredentials> {
    let raw: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_slice(plaintext).map_err(|e| {
            DbError::config(format!(
                "The decrypted DBeaver credentials are not valid JSON: {e}"
            ))
        })?;
    let mut out = DbeaverCredentials::default();
    for (conn, sections) in raw {
        let mut parsed = BTreeMap::new();
        for (section, value) in sections {
            if let Ok(c) = RawCredential::deserialize(value) {
                parsed.insert(
                    section,
                    DbeaverCredential {
                        user: c.user.filter(|u| !u.is_empty()),
                        password: c.password.map(|p| p.0).filter(|p| !p.is_empty()),
                    },
                );
            }
        }
        out.entries.insert(conn, parsed);
    }
    Ok(out)
}

/// Encrypts a plaintext credentials JSON the way DBeaver does (random IV
/// prepended). Used for tests and for exporting to DBeaver.
pub fn encrypt_credentials(plaintext: &[u8]) -> DbResult<Vec<u8>> {
    use rand::TryRng;
    let mut iv = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut iv)
        .map_err(|e| DbError::internal(format!("Random number generator failed: {e}")))?;
    let enc = Aes128CbcEnc::new_from_slices(&DBEAVER_CREDENTIALS_KEY, &iv)
        .map_err(|_| DbError::internal("Invalid AES key or IV length"))?;
    let mut out = iv.to_vec();
    out.extend_from_slice(&enc.encrypt_padded_vec::<Pkcs7>(plaintext));
    Ok(out)
}

/// Default DBeaver workspace locations for this platform (existing or not).
pub fn default_workspace_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(base) = directories::BaseDirs::new() else {
        return out;
    };
    #[cfg(target_os = "macos")]
    out.push(
        base.home_dir()
            .join("Library")
            .join("DBeaverData")
            .join("workspace6"),
    );
    #[cfg(windows)]
    out.push(base.config_dir().join("DBeaverData").join("workspace6"));
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        out.push(base.data_dir().join("DBeaverData").join("workspace6"));
        let fallback = base
            .home_dir()
            .join(".local")
            .join("share")
            .join("DBeaverData")
            .join("workspace6");
        if !out.contains(&fallback) {
            out.push(fallback);
        }
    }
    out
}

/// Existing DBeaver workspaces on this computer.
pub fn find_workspaces() -> Vec<PathBuf> {
    default_workspace_candidates()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

/// Project directories of a workspace (those containing DBeaver connection files).
pub fn find_projects(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(workspace) else {
        return out;
    };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        if !p.is_dir() || e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if !data_source_files(&p).is_empty() {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn data_source_files(project: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dbeaver = project.join(".dbeaver");
    if let Ok(rd) = std::fs::read_dir(&dbeaver) {
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("data-sources") && name.ends_with(".json") {
                out.push(e.path());
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(project) {
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".dbeaver-data-sources") && name.ends_with(".xml") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Imports every project of a workspace. Connections of projects other than
/// "General" are placed in a folder named after the project.
pub fn import_workspace(workspace: &Path) -> DbResult<ImportResult> {
    let projects = find_projects(workspace);
    if projects.is_empty() {
        return Err(DbError::config(format!(
            "No DBeaver projects were found in {}",
            workspace.display()
        )));
    }
    let mut result = ImportResult::default();
    for p in projects {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        match import_project(&p) {
            Ok(mut r) => {
                if name != "General" && !name.is_empty() {
                    r.prefix_folders(&name);
                }
                result.merge(r);
            }
            Err(e) => result
                .warnings
                .push(format!("Project '{name}': {}", e.message)),
        }
    }
    Ok(result)
}

/// Imports one project directory (`<project>/.dbeaver/data-sources*.json`
/// plus `credentials-config.json`, or legacy XML files).
pub fn import_project(project: &Path) -> DbResult<ImportResult> {
    let files = data_source_files(project);
    if files.is_empty() {
        return Err(DbError::config(format!(
            "{} contains no DBeaver connections",
            project.display()
        )));
    }
    let mut result = ImportResult::default();
    let cred_path = project.join(".dbeaver").join("credentials-config.json");
    let credentials = match std::fs::read(&cred_path) {
        Ok(bytes) => match decrypt_credentials(&bytes) {
            Ok(c) => c,
            Err(e) => {
                result
                    .warnings
                    .push(format!("{}; passwords were not imported", e.message));
                DbeaverCredentials::default()
            }
        },
        Err(_) => DbeaverCredentials::default(),
    };
    for f in files {
        let text = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) => {
                result
                    .warnings
                    .push(format!("Cannot read {}: {e}", f.display()));
                continue;
            }
        };
        let parsed = if f.extension().and_then(|x| x.to_str()) == Some("xml") {
            parse_legacy_xml(&text)
        } else {
            parse_data_sources(&text, &credentials)
        };
        match parsed {
            Ok(r) => result.merge(r),
            Err(e) => result
                .warnings
                .push(format!("{}: {}", f.display(), e.message)),
        }
    }
    Ok(result)
}

/// Maps DBeaver provider / driver ids to a database kind.
pub fn map_driver(provider: &str, driver: &str) -> Option<DbKind> {
    fn check(s: &str) -> Option<DbKind> {
        let s = s.to_ascii_lowercase();
        let k = if s.contains("cockroach") {
            DbKind::CockroachDb
        } else if s.contains("mariadb") {
            DbKind::MariaDb
        } else if s.contains("mysql") || s == "tidb" {
            DbKind::MySql
        } else if [
            "postgres",
            "greenplum",
            "redshift",
            "timescale",
            "yugabyte",
            "alloydb",
            "supabase",
            "edb_",
            "enterprisedb",
        ]
        .iter()
        .any(|p| s.contains(p))
        {
            DbKind::Postgres
        } else if s.contains("sqlite") {
            DbKind::Sqlite
        } else if s.contains("duckdb") {
            DbKind::DuckDb
        } else if s.contains("clickhouse") {
            DbKind::ClickHouse
        } else if ["mssql", "sqlserver", "sql_server", "jtds"]
            .iter()
            .any(|p| s.contains(p))
            || s == "microsoft"
        {
            DbKind::SqlServer
        } else {
            return None;
        };
        Some(k)
    }
    check(driver)
        .or_else(|| check(provider))
        .or_else(|| DbKind::from_id(provider))
        .filter(|k| *k != DbKind::Generic)
}

fn s<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn b(o: &Map<String, Value>, key: &str) -> Option<bool> {
    match o.get(key)? {
        Value::Bool(v) => Some(*v),
        Value::String(t) => match t.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

fn n(o: &Map<String, Value>, key: &str) -> Option<i64> {
    match o.get(key)? {
        Value::Number(v) => v.as_i64(),
        Value::String(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn obj<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    o.get(key).and_then(Value::as_object)
}

/// Parses `"r,g,b"` (or `#rrggbb`) colours used by DBeaver.
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix('#') {
        if hex.len() == 6 {
            let v = u32::from_str_radix(hex, 16).ok()?;
            return Some([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
        }
        return None;
    }
    let parts: Vec<u8> = t
        .split(',')
        .map(|p| p.trim().parse::<u8>())
        .collect::<Result<_, _>>()
        .ok()?;
    match parts.as_slice() {
        [r, g, bl] => Some([*r, *g, *bl]),
        _ => None,
    }
}

/// Parses the content of a `data-sources*.json` file.
pub fn parse_data_sources(json: &str, credentials: &DbeaverCredentials) -> DbResult<ImportResult> {
    let root: Value = serde_json::from_str(json)
        .map_err(|e| DbError::config(format!("Invalid data-sources JSON: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| DbError::config("Invalid data-sources JSON: not an object"))?;
    let mut result = ImportResult::default();

    if let Some(folders) = obj(root, "folders") {
        for (name, f) in folders {
            let parent = f.as_object().and_then(|o| s(o, "parent"));
            let path = match parent {
                Some(p) if !name.contains('/') => format!("{p}/{name}"),
                _ => name.clone(),
            };
            if let Some(p) = crate::connections::normalize_folder(&path)
                && !result.folders.contains(&p)
            {
                result.folders.push(p);
            }
        }
    }

    if let Some(types) = obj(root, "connection-types") {
        for (id, t) in types {
            let Some(t) = t.as_object() else { continue };
            let builtin = ConnectionType::builtins().into_iter().find(|x| x.id == *id);
            let base = builtin.clone().unwrap_or_else(ConnectionType::development);
            let confirm_execute = b(t, "confirm-execute").unwrap_or(base.confirm_execute);
            let ct = ConnectionType {
                id: id.clone(),
                name: s(t, "name")
                    .map(str::to_string)
                    .unwrap_or_else(|| base.name.clone()),
                color: s(t, "color").and_then(parse_color).unwrap_or(base.color),
                auto_commit: b(t, "auto-commit").unwrap_or(base.auto_commit),
                confirm_execute,
                confirm_data_change: b(t, "confirm-data-change")
                    .unwrap_or(base.confirm_data_change),
                confirm_dangerous: base.confirm_dangerous || confirm_execute,
            };
            // Unchanged built-ins need no import.
            if builtin.as_ref() != Some(&ct) {
                result.connection_types.push(ct);
            }
        }
    }

    let Some(connections) = obj(root, "connections") else {
        return Ok(result);
    };
    for (id, c) in connections {
        let Some(c) = c.as_object() else { continue };
        match parse_connection(id, c, credentials, &mut result) {
            Ok(cfg) => {
                if let Some(f) = cfg.folder.clone()
                    && !result.folders.contains(&f)
                {
                    result.folders.push(f);
                }
                result.connections.push(cfg);
            }
            Err(w) => result.warnings.push(w),
        }
    }
    Ok(result)
}

fn parse_connection(
    id: &str,
    c: &Map<String, Value>,
    creds: &DbeaverCredentials,
    result: &mut ImportResult,
) -> Result<ConnectionConfig, String> {
    let name = s(c, "name").unwrap_or(id).to_string();
    let provider = s(c, "provider").unwrap_or("");
    let driver = s(c, "driver").unwrap_or("");
    let kind = map_driver(provider, driver).ok_or_else(|| {
        format!("Connection '{name}' uses a database DBiji does not support ({provider}/{driver}); skipped")
    })?;
    let empty = Map::new();
    let conf = obj(c, "configuration").unwrap_or(&empty);

    let mut cfg = ConnectionConfig::new(kind, name.clone());
    cfg.id = id.to_string();
    cfg.description = s(c, "description").map(str::to_string);
    cfg.folder = s(c, "folder").and_then(crate::connections::normalize_folder);
    cfg.read_only = b(c, "read-only")
        .or_else(|| b(conf, "read-only"))
        .unwrap_or(false);
    cfg.save_password = b(c, "save-password").unwrap_or(false);
    cfg.connection_type = s(conf, "type").unwrap_or("dev").to_string();
    if let Some(color) = s(c, "color").and_then(parse_color) {
        cfg.color = Some(color);
    }
    cfg.init.show_system_objects = b(c, "show-system-objects").unwrap_or(false);
    if b(c, "navigator-show-only-entities").unwrap_or(false)
        || b(c, "navigator-hide-folders").unwrap_or(false)
    {
        cfg.navigator_view = NavigatorView::Simple;
    }

    // Endpoint: explicit fields, else the JDBC URL.
    let host = s(conf, "host");
    let port = n(conf, "port")
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0);
    let database = s(conf, "database");
    let url = s(conf, "url");
    if kind.is_file_based() {
        let path = database.map(str::to_string).or_else(|| {
            url.and_then(|u| parse_connection_url(u).ok())
                .and_then(|(c, _)| c.file_path)
        });
        cfg.file_path = path;
        cfg.database = None;
        cfg.user = None;
        cfg.auth = AuthMethod::None;
        if cfg.file_path.is_none() {
            return Err(format!("Connection '{name}' has no database file; skipped"));
        }
    } else {
        if let Some(h) = host {
            cfg.host = h.to_string();
            if port.is_some() {
                cfg.port = port;
            }
            cfg.database = database.map(str::to_string).or(cfg.database);
        } else if let Some(u) = url {
            match parse_connection_url(u) {
                Ok((parsed, _)) => {
                    cfg.host = parsed.host;
                    cfg.port = parsed.port;
                    cfg.database = parsed.database.or(cfg.database);
                    cfg.ssl = parsed.ssl;
                    cfg.properties.extend(parsed.properties);
                }
                Err(e) => {
                    return Err(format!(
                        "Connection '{name}': cannot read its URL ({}); skipped",
                        e.message
                    ));
                }
            }
        } else {
            return Err(format!(
                "Connection '{name}' has neither a host nor a URL; skipped"
            ));
        }
        if let Some(server) = s(conf, "server") {
            cfg.properties.insert("server".into(), server.to_string());
        }
    }

    // Credentials.
    let conn_cred = creds.get(id, "#connection");
    let user = conn_cred
        .and_then(|c| c.user.clone())
        .or_else(|| s(conf, "user").map(str::to_string));
    if !kind.is_file_based() {
        cfg.user = user.or(cfg.user);
        match s(conf, "auth-model").unwrap_or("native") {
            "native" | "mysql_native" | "postgres_native" | "sqlserver_database"
            | "clickhouse_native" => {
                cfg.auth = AuthMethod::Password;
            }
            "postgres_pgpass" | "pgpass" => cfg.auth = AuthMethod::Pgpass,
            other => {
                cfg.auth = AuthMethod::Password;
                result.warnings.push(format!(
                    "Connection '{name}': authentication model '{other}' is not supported; using user name and password"
                ));
            }
        }
        if let Some(pw) = conn_cred.and_then(|c| c.password.clone()) {
            result
                .secrets
                .push((secret_key(id, SecretSlot::Password), pw));
            cfg.save_password = true;
        }
    }

    // Bootstrap.
    if let Some(boot) = obj(conf, "bootstrap") {
        if let Some(ac) = b(boot, "autocommit").or_else(|| b(boot, "defaultAutoCommit")) {
            cfg.auto_commit = Some(ac);
        }
        let catalog = s(boot, "defaultCatalog");
        let schema = s(boot, "defaultSchema")
            .or_else(|| s(boot, "defaultObjectName"))
            .or_else(|| s(boot, "defaultObject"));
        if kind.is_mysql_family() {
            cfg.init.default_schema = schema.or(catalog).map(str::to_string);
        } else {
            cfg.init.default_schema = schema.map(str::to_string);
            if let Some(cat) = catalog
                && cfg.database.is_none()
            {
                cfg.database = Some(cat.to_string());
            }
        }
        if let Some(q) = boot.get("initQueries").and_then(Value::as_array) {
            let queries: Vec<&str> = q
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .collect();
            if !queries.is_empty() {
                cfg.init.init_sql = queries
                    .iter()
                    .map(|q| q.trim_end_matches(';'))
                    .collect::<Vec<_>>()
                    .join(";\n");
            }
        }
    }
    if let Some(ka) = n(conf, "keepAliveInterval").filter(|v| *v > 0) {
        cfg.init.keep_alive_secs = u32::try_from(ka).unwrap_or(u32::MAX);
    }
    if let Some(pp) = obj(conf, "provider-properties")
        && b(pp, "@dbeaver-show-non-default-db@") == Some(true)
    {
        cfg.init.show_all_databases = true;
    }
    if let Some(events) = obj(conf, "events") {
        let command = |key: &str| {
            obj(events, key)
                .filter(|e| b(e, "enabled").unwrap_or(true))
                .and_then(|e| s(e, "command"))
                .map(str::to_string)
        };
        cfg.init.shell_before_connect = command("BEFORE_CONNECT");
        cfg.init.shell_after_disconnect = command("AFTER_DISCONNECT");
    }

    // Driver properties (SSL related ones are mapped).
    if let Some(props) = obj(conf, "properties") {
        for (k, v) in props {
            let v = match v {
                Value::String(t) => t.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            apply_driver_property(&mut cfg, k, &v);
        }
    }

    // Network handlers.
    if let Some(handlers) = obj(conf, "handlers") {
        for (hid, h) in handlers {
            let Some(h) = h.as_object() else { continue };
            if !b(h, "enabled").unwrap_or(false) {
                continue;
            }
            let props = obj(h, "properties").unwrap_or(&empty);
            let section = format!("network/{hid}");
            let hcred = creds.get(id, &section);
            match hid.as_str() {
                "ssh_tunnel" => {
                    let ssh = parse_ssh_handler(&name, h, props, hcred, result);
                    let password = hcred.and_then(|c| c.password.clone());
                    if let Some(pw) = password {
                        let slot = match ssh.auth {
                            SshAuth::PublicKey { .. } => SecretSlot::SshPassphrase,
                            _ => SecretSlot::SshPassword,
                        };
                        result.secrets.push((secret_key(id, slot), pw));
                    }
                    cfg.ssh = Some(ssh);
                }
                "socks_proxy" | "http_proxy" | "proxy" => {
                    let kind_p = if hid == "http_proxy" { ProxyKind::Http } else { ProxyKind::Socks5 };
                    let host = s(props, "socks-host").or_else(|| s(props, "host")).unwrap_or("").to_string();
                    let port = n(props, "socks-port")
                        .or_else(|| n(props, "port"))
                        .and_then(|p| u16::try_from(p).ok())
                        .unwrap_or(if kind_p == ProxyKind::Http { 8080 } else { 1080 });
                    let user = hcred
                        .and_then(|c| c.user.clone())
                        .or_else(|| s(props, "socks-user").map(str::to_string))
                        .or_else(|| s(h, "user").map(str::to_string));
                    cfg.proxy = Some(ProxyConfig {
                        enabled: true,
                        kind: kind_p,
                        host,
                        port,
                        user,
                        save_credentials: b(h, "save-password").unwrap_or(true),
                    });
                    if let Some(pw) = hcred.and_then(|c| c.password.clone()) {
                        result.secrets.push((secret_key(id, SecretSlot::ProxyPassword), pw));
                    }
                }
                other if other.ends_with("_ssl") || other == "ssl" => apply_ssl_handler(&mut cfg, props),
                other => result
                    .warnings
                    .push(format!("Connection '{name}': network handler '{other}' is not supported and was ignored")),
            }
        }
    }
    Ok(cfg)
}

fn parse_ssh_handler(
    name: &str,
    h: &Map<String, Value>,
    props: &Map<String, Value>,
    cred: Option<&DbeaverCredential>,
    result: &mut ImportResult,
) -> SshConfig {
    let auth = match s(props, "authType")
        .unwrap_or("PASSWORD")
        .to_ascii_uppercase()
        .as_str()
    {
        "PUBLIC_KEY" | "PUBLICKEY" | "KEY" => SshAuth::PublicKey {
            key_path: s(props, "keyPath").unwrap_or("").to_string(),
        },
        "AGENT" => SshAuth::Agent,
        _ => SshAuth::Password,
    };
    let millis_to_secs = |v: i64| -> u32 { u32::try_from((v + 999) / 1000).unwrap_or(u32::MAX) };
    let defaults = SshConfig::default();
    let user = cred
        .and_then(|c| c.user.clone())
        .or_else(|| s(props, "user").map(str::to_string))
        .or_else(|| s(h, "user").map(str::to_string))
        .unwrap_or_default();
    if props.keys().any(|k| k.starts_with("jumpServer"))
        && b(props, "jumpServerEnabled").unwrap_or(true)
    {
        result.warnings.push(format!(
            "Connection '{name}': SSH jump hosts are not imported; configure them again in DBiji"
        ));
    }
    SshConfig {
        enabled: true,
        host: s(props, "host").unwrap_or("").to_string(),
        port: n(props, "port")
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .unwrap_or(22),
        user,
        auth,
        save_credentials: b(h, "save-password").unwrap_or(true),
        local_port: n(props, "localPort")
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0),
        keepalive_secs: n(props, "aliveInterval")
            .filter(|v| *v > 0)
            .map(millis_to_secs)
            .unwrap_or(defaults.keepalive_secs),
        connect_timeout_secs: n(props, "sshConnectTimeout")
            .filter(|v| *v > 0)
            .map(millis_to_secs)
            .unwrap_or(defaults.connect_timeout_secs),
        host_key_policy: defaults.host_key_policy,
    }
}

fn apply_ssl_handler(cfg: &mut ConnectionConfig, props: &Map<String, Value>) {
    let mode = ["sslMode", "ssl.mode", "sslmode", "ssl-mode"]
        .iter()
        .find_map(|k| s(props, k))
        .and_then(SslMode::parse);
    let verify = [
        "verifyServerCert",
        "ssl.verify.server",
        "verifyServerCertificate",
    ]
    .iter()
    .find_map(|k| b(props, k));
    cfg.ssl.mode = match (mode, verify) {
        (Some(m), _) => m,
        (None, Some(true)) => SslMode::VerifyCa,
        _ => SslMode::Require,
    };
    let first = |keys: &[&str]| keys.iter().find_map(|k| s(props, k)).map(str::to_string);
    if let Some(v) = first(&[
        "ssl.ca.cert",
        "sslRootCert",
        "ssl.root.cert",
        "caCertPath",
        "ssl.ca",
    ]) {
        cfg.ssl.ca_cert = Some(v);
    }
    if let Some(v) = first(&["ssl.client.cert", "sslCert", "clientCertPath", "ssl.cert"]) {
        cfg.ssl.client_cert = Some(v);
    }
    if let Some(v) = first(&["ssl.client.key", "sslKey", "clientKeyPath", "ssl.key"]) {
        cfg.ssl.client_key = Some(v);
    }
}

fn apply_driver_property(cfg: &mut ConnectionConfig, key: &str, value: &str) {
    let truthy = matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "required"
    );
    match key.to_ascii_lowercase().as_str() {
        "sslmode" | "ssl-mode" => {
            if let Some(m) = SslMode::parse(value) {
                cfg.ssl.mode = m;
            }
        }
        "ssl" | "usessl" | "requiressl" => {
            if truthy && cfg.ssl.mode == SslMode::Prefer {
                cfg.ssl.mode = SslMode::Require;
            } else if !truthy
                && value.eq_ignore_ascii_case("false")
                && key.eq_ignore_ascii_case("useSSL")
            {
                cfg.ssl.mode = SslMode::Disable;
            }
        }
        "sslrootcert" => cfg.ssl.ca_cert = Some(value.to_string()),
        "sslcert" => cfg.ssl.client_cert = Some(value.to_string()),
        "sslkey" => cfg.ssl.client_key = Some(value.to_string()),
        _ => {
            cfg.properties.insert(key.to_string(), value.to_string());
        }
    }
}

// ------------------------------------------------------------ legacy XML

/// Parses a legacy `.dbeaver-data-sources*.xml` file (connection settings
/// only; passwords of that format are not imported).
/// Network handler being read: (id, attributes, properties).
type PendingHandler = (String, Map<String, Value>, Map<String, Value>);

/// Parses a legacy `.dbeaver-data-sources*.xml` file.
pub fn parse_legacy_xml(xml: &str) -> DbResult<ImportResult> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    let mut result = ImportResult::default();
    let mut current: Option<(String, Map<String, Value>)> = None;
    let mut configuration = Map::new();
    let mut handlers = Map::new();
    let mut handler: Option<PendingHandler> = None;
    let mut properties = Map::new();
    let mut had_password = false;

    fn attrs(e: &quick_xml::events::BytesStart<'_>) -> Map<String, Value> {
        let mut m = Map::new();
        for a in e.attributes().flatten() {
            let k = a.key.as_ref().to_string();
            let v = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| a.value.to_string());
            m.insert(k, Value::String(v));
        }
        m
    }

    loop {
        let ev = reader
            .read_event()
            .map_err(|e| DbError::config(format!("Invalid legacy DBeaver XML: {e}")))?;
        let (start, is_empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let tag = e.name().as_ref().to_string();
            let a = attrs(&e);
            match tag.as_str() {
                "folder" => {
                    if let Some(name) = s(&a, "name") {
                        let path = match s(&a, "parent") {
                            Some(p) => format!("{p}/{name}"),
                            None => name.to_string(),
                        };
                        if let Some(p) = crate::connections::normalize_folder(&path) {
                            result.folders.push(p);
                        }
                    }
                }
                "data-source" => {
                    let id = s(&a, "id").unwrap_or("").to_string();
                    current = Some((id, a));
                    configuration = Map::new();
                    handlers = Map::new();
                    properties = Map::new();
                }
                "connection" if current.is_some() => {
                    if s(&a, "password").is_some() {
                        had_password = true;
                    }
                    configuration = a;
                }
                "network-handler" if current.is_some() => {
                    let id = s(&a, "id").unwrap_or("").to_string();
                    handler = Some((id, a, Map::new()));
                    if is_empty && let Some((hid, mut h, props)) = handler.take() {
                        h.insert("properties".into(), Value::Object(props));
                        handlers.insert(hid, Value::Object(h));
                    }
                }
                "property" if current.is_some() => {
                    if let (Some(k), Some(v)) =
                        (s(&a, "name"), a.get("value").and_then(Value::as_str))
                    {
                        match handler.as_mut() {
                            Some((_, _, props)) => {
                                props.insert(k.to_string(), Value::String(v.to_string()));
                            }
                            None => {
                                properties.insert(k.to_string(), Value::String(v.to_string()));
                            }
                        }
                    }
                }
                _ => {}
            }
            if is_empty && tag == "data-source" {
                finish_legacy(
                    &mut current,
                    &mut configuration,
                    &mut handlers,
                    &mut properties,
                    &mut result,
                );
            }
            continue;
        }
        match ev {
            Event::End(e) => {
                let tag = e.name().as_ref().to_string();
                match tag.as_str() {
                    "network-handler" => {
                        if let Some((hid, mut h, props)) = handler.take() {
                            h.insert("properties".into(), Value::Object(props));
                            handlers.insert(hid, Value::Object(h));
                        }
                    }
                    "data-source" => {
                        finish_legacy(
                            &mut current,
                            &mut configuration,
                            &mut handlers,
                            &mut properties,
                            &mut result,
                        );
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if had_password {
        result.warnings.push(
            "Passwords stored in legacy DBeaver XML files are not imported; enter them again"
                .to_string(),
        );
    }
    Ok(result)
}

fn finish_legacy(
    current: &mut Option<(String, Map<String, Value>)>,
    configuration: &mut Map<String, Value>,
    handlers: &mut Map<String, Value>,
    properties: &mut Map<String, Value>,
    result: &mut ImportResult,
) {
    let Some((id, attrs)) = current.take() else {
        return;
    };
    let mut conn = attrs;
    let mut conf = std::mem::take(configuration);
    conf.remove("password");
    conf.insert("handlers".into(), Value::Object(std::mem::take(handlers)));
    conf.insert(
        "properties".into(),
        Value::Object(std::mem::take(properties)),
    );
    conn.insert("configuration".into(), Value::Object(conf));
    let creds = DbeaverCredentials::default();
    match parse_connection(&id, &conn, &creds, result) {
        Ok(cfg) => {
            if let Some(f) = cfg.folder.clone()
                && !result.folders.contains(&f)
            {
                result.folders.push(f);
            }
            result.connections.push(cfg);
        }
        Err(w) => result.warnings.push(w),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA_SOURCES: &str = r#"{
        "folders": {"Prod": {}, "EU": {"parent": "Prod", "description": "Europe"}},
        "connection-types": {
            "dev": {"name": "Development", "color": "255,255,255", "auto-commit": true, "confirm-execute": false, "confirm-data-change": false},
            "staging": {"name": "Staging", "color": "10,20,30", "auto-commit": false, "confirm-execute": true}
        },
        "connections": {
            "postgres-jdbc-1": {
                "provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders PG",
                "save-password": true, "read-only": true, "folder": "Prod/EU",
                "show-system-objects": true, "navigator-show-only-entities": true,
                "configuration": {
                    "host": "pg.internal", "port": "6432", "database": "orders",
                    "url": "jdbc:postgresql://pg.internal:6432/orders",
                    "type": "staging", "auth-model": "native",
                    "bootstrap": {"autocommit": false, "defaultSchema": "sales", "initQueries": ["SET search_path TO sales;", "SET timezone = 'UTC'"]},
                    "provider-properties": {"@dbeaver-show-non-default-db@": "true"},
                    "properties": {"sslmode": "verify-full", "sslrootcert": "/certs/ca.pem", "ApplicationName": "dbeaver"},
                    "events": {"BEFORE_CONNECT": {"enabled": true, "command": "vpn up"}},
                    "handlers": {
                        "ssh_tunnel": {"type": "TUNNEL", "enabled": true, "save-password": true,
                            "properties": {"host": "bastion.corp", "port": 2222, "authType": "PUBLIC_KEY",
                                           "keyPath": "~/.ssh/id_ed25519", "aliveInterval": "15000", "sshConnectTimeout": "10000"}}
                    }
                }
            },
            "mysql8-2": {
                "provider": "mysql", "driver": "mysql8", "name": "Shop MySQL",
                "configuration": {
                    "url": "jdbc:mysql://mysql.local:3307/shop?useSSL=true",
                    "type": "dev",
                    "bootstrap": {"defaultCatalog": "shop_ro"},
                    "handlers": {
                        "socks_proxy": {"enabled": true, "properties": {"socks-host": "proxy.corp", "socks-port": "1081"}},
                        "ssh_tunnel": {"enabled": false, "properties": {"host": "ignored"}}
                    }
                }
            },
            "mariaDB-3": {"provider": "mysql", "driver": "mariaDB", "name": "Maria",
                "configuration": {"host": "maria", "port": 3306, "database": "m", "auth-model": "kerberos"}},
            "sqlite_jdbc-4": {"provider": "sqlite", "driver": "sqlite_jdbc", "name": "Local file",
                "configuration": {"database": "/data/local.db", "url": "jdbc:sqlite:/data/local.db"}},
            "generic-5": {"provider": "generic", "driver": "duckdb_jdbc", "name": "Duck",
                "configuration": {"url": "jdbc:duckdb:/data/w.duckdb"}},
            "oracle_thin-6": {"provider": "oracle", "driver": "oracle_thin", "name": "Legacy Oracle",
                "configuration": {"host": "ora", "port": "1521"}},
            "mssql_jdbc_ms_new-7": {"provider": "sqlserver", "driver": "mssql_jdbc_ms_new", "name": "MSSQL",
                "configuration": {"host": "sql", "port": "1433", "database": "master", "user": "sa"}},
            "clickhouse-com-8": {"provider": "clickhouse", "driver": "com_clickhouse", "name": "CH",
                "configuration": {"host": "ch", "port": "8123", "database": "default"}},
            "cockroachdb-9": {"provider": "postgresql", "driver": "cockroachdb", "name": "Roach",
                "configuration": {"host": "crdb", "port": "26257"}}
        }
    }"#;

    const CREDENTIALS: &str = r##"{
        "postgres-jdbc-1": {"#connection": {"user": "orders_app", "password": "pg-secret"},
                            "network/ssh_tunnel": {"user": "deploy", "password": "key-passphrase"}},
        "mysql8-2": {"#connection": {"user": "shop", "password": "my-secret"},
                     "network/socks_proxy": {"user": "pu", "password": "proxy-secret"}}
    }"##;

    fn creds() -> DbeaverCredentials {
        decrypt_credentials(&encrypt_credentials(CREDENTIALS.as_bytes()).unwrap()).unwrap()
    }

    fn find<'a>(r: &'a ImportResult, id: &str) -> &'a ConnectionConfig {
        r.connections
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
    }

    fn secret<'a>(r: &'a ImportResult, key: &str) -> &'a str {
        r.secrets
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.expose())
            .unwrap_or_else(|| panic!("{key} missing"))
    }

    #[test]
    fn credentials_crypto_roundtrip() {
        let enc = encrypt_credentials(CREDENTIALS.as_bytes()).unwrap();
        assert_eq!(enc.len() % 16, 0);
        assert!(!enc.windows(9).any(|w| w == b"pg-secret"));
        let c = decrypt_credentials(&enc).unwrap();
        let pg = c.get("postgres-jdbc-1", "#connection").unwrap();
        assert_eq!(pg.user.as_deref(), Some("orders_app"));
        assert_eq!(pg.password.as_ref().unwrap().expose(), "pg-secret");
        assert!(decrypt_credentials(b"short").is_err());
        let mut bad = enc.clone();
        bad[20] ^= 0xff;
        // Corrupting the first block garbles the JSON or the padding.
        assert!(
            decrypt_credentials(&bad).is_err()
                || decrypt_credentials(&bad).is_ok_and(|c| c.entries.is_empty())
        );
    }

    #[test]
    fn known_vector_with_default_key() {
        // Encrypted with `openssl aes-128-cbc -K babb4a9f774ab853c96c2d653dfe544a -iv 000102030405060708090a0b0c0d0e0f`
        // equivalent: built here with the same primitive and checked against a fixed IV.
        let iv = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let enc = Aes128CbcEnc::new_from_slices(&DBEAVER_CREDENTIALS_KEY, &iv).unwrap();
        let mut file = iv.to_vec();
        file.extend_from_slice(&enc.encrypt_padded_vec::<Pkcs7>(
            br##"{"a":{"#connection":{"user":"u","password":"p"}}}"##,
        ));
        let c = decrypt_credentials(&file).unwrap();
        assert_eq!(
            c.get("a", "#connection")
                .unwrap()
                .password
                .as_ref()
                .unwrap()
                .expose(),
            "p"
        );
        assert_eq!(
            hex::encode(DBEAVER_CREDENTIALS_KEY),
            "babb4a9f774ab853c96c2d653dfe544a"
        );
    }

    #[test]
    fn data_sources_main_fields() {
        let r = parse_data_sources(DATA_SOURCES, &creds()).unwrap();
        assert_eq!(r.connections.len(), 8, "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("Legacy Oracle")));
        assert!(r.warnings.iter().any(|w| w.contains("kerberos")));
        assert!(r.folders.contains(&"Prod".to_string()));
        assert!(r.folders.contains(&"Prod/EU".to_string()));
        assert_eq!(
            r.connection_types.len(),
            1,
            "only the custom type is imported"
        );
        let st = &r.connection_types[0];
        assert_eq!(
            (st.id.as_str(), st.color, st.auto_commit, st.confirm_execute),
            ("staging", [10, 20, 30], false, true)
        );

        let pg = find(&r, "postgres-jdbc-1");
        assert_eq!(pg.kind, DbKind::Postgres);
        assert_eq!(pg.name, "Orders PG");
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_deref()),
            ("pg.internal", Some(6432), Some("orders"))
        );
        assert_eq!(pg.user.as_deref(), Some("orders_app"));
        assert_eq!(pg.folder.as_deref(), Some("Prod/EU"));
        assert!(pg.read_only);
        assert!(pg.save_password);
        assert_eq!(pg.connection_type, "staging");
        assert_eq!(pg.auto_commit, Some(false));
        assert_eq!(pg.init.default_schema.as_deref(), Some("sales"));
        assert_eq!(
            pg.init.init_sql,
            "SET search_path TO sales;\nSET timezone = 'UTC'"
        );
        assert!(pg.init.show_all_databases);
        assert!(pg.init.show_system_objects);
        assert_eq!(pg.init.shell_before_connect.as_deref(), Some("vpn up"));
        assert_eq!(pg.navigator_view, NavigatorView::Simple);
        assert_eq!(pg.ssl.mode, SslMode::VerifyFull);
        assert_eq!(pg.ssl.ca_cert.as_deref(), Some("/certs/ca.pem"));
        assert_eq!(
            pg.properties.get("ApplicationName").map(String::as_str),
            Some("dbeaver")
        );
        let ssh = pg.ssh.as_ref().unwrap();
        assert_eq!(
            (ssh.host.as_str(), ssh.port, ssh.user.as_str()),
            ("bastion.corp", 2222, "deploy")
        );
        assert_eq!(
            ssh.auth,
            SshAuth::PublicKey {
                key_path: "~/.ssh/id_ed25519".into()
            }
        );
        assert_eq!((ssh.keepalive_secs, ssh.connect_timeout_secs), (15, 10));
        assert_eq!(secret(&r, "conn:postgres-jdbc-1:password"), "pg-secret");
        assert_eq!(
            secret(&r, "conn:postgres-jdbc-1:ssh-passphrase"),
            "key-passphrase"
        );

        let my = find(&r, "mysql8-2");
        assert_eq!(my.kind, DbKind::MySql);
        assert_eq!(
            (my.host.as_str(), my.port, my.database.as_deref()),
            ("mysql.local", Some(3307), Some("shop"))
        );
        assert_eq!(my.ssl.mode, SslMode::Require);
        assert_eq!(my.init.default_schema.as_deref(), Some("shop_ro"));
        assert!(my.ssh.is_none(), "disabled handler ignored");
        let proxy = my.proxy.as_ref().unwrap();
        assert_eq!(
            (
                proxy.kind,
                proxy.host.as_str(),
                proxy.port,
                proxy.user.as_deref()
            ),
            (ProxyKind::Socks5, "proxy.corp", 1081, Some("pu"))
        );
        assert_eq!(secret(&r, "conn:mysql8-2:password"), "my-secret");
        assert_eq!(secret(&r, "conn:mysql8-2:proxy-password"), "proxy-secret");

        assert_eq!(find(&r, "mariaDB-3").kind, DbKind::MariaDb);
        let lite = find(&r, "sqlite_jdbc-4");
        assert_eq!(
            (lite.kind, lite.file_path.as_deref()),
            (DbKind::Sqlite, Some("/data/local.db"))
        );
        let duck = find(&r, "generic-5");
        assert_eq!(
            (duck.kind, duck.file_path.as_deref()),
            (DbKind::DuckDb, Some("/data/w.duckdb"))
        );
        let ms = find(&r, "mssql_jdbc_ms_new-7");
        assert_eq!(
            (ms.kind, ms.user.as_deref()),
            (DbKind::SqlServer, Some("sa"))
        );
        assert_eq!(find(&r, "clickhouse-com-8").kind, DbKind::ClickHouse);
        assert_eq!(find(&r, "cockroachdb-9").kind, DbKind::CockroachDb);
        // No secret leaks into the configs.
        let json = serde_json::to_string(&r.connections).unwrap();
        for s in ["pg-secret", "key-passphrase", "my-secret", "proxy-secret"] {
            assert!(!json.contains(s));
        }
    }

    #[test]
    fn driver_mapping() {
        assert_eq!(
            map_driver("postgresql", "postgres-jdbc"),
            Some(DbKind::Postgres)
        );
        assert_eq!(map_driver("mysql", "mysql5"), Some(DbKind::MySql));
        assert_eq!(map_driver("mysql", "mariaDB"), Some(DbKind::MariaDb));
        assert_eq!(map_driver("generic", "sqlite_jdbc"), Some(DbKind::Sqlite));
        assert_eq!(
            map_driver("sqlserver", "microsoft"),
            Some(DbKind::SqlServer)
        );
        assert_eq!(
            map_driver("mssql", "mssql_jdbc_ms_new"),
            Some(DbKind::SqlServer)
        );
        assert_eq!(
            map_driver("clickhouse", "clickhouse-com"),
            Some(DbKind::ClickHouse)
        );
        assert_eq!(map_driver("generic", "duckdb_jdbc"), Some(DbKind::DuckDb));
        assert_eq!(
            map_driver("postgresql", "greenplum"),
            Some(DbKind::Postgres)
        );
        assert_eq!(map_driver("generic", "h2_embedded"), None);
        assert_eq!(map_driver("oracle", "oracle_thin"), None);
        assert_eq!(parse_color("247,159,129"), Some([247, 159, 129]));
        assert_eq!(parse_color("#0a141e"), Some([10, 20, 30]));
        assert_eq!(parse_color("org.jkiss.color"), None);
    }

    #[test]
    fn workspace_import_from_synthetic_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace6");
        let general = ws.join("General").join(".dbeaver");
        std::fs::create_dir_all(&general).unwrap();
        std::fs::write(general.join("data-sources.json"), DATA_SOURCES).unwrap();
        std::fs::write(
            general.join("credentials-config.json"),
            encrypt_credentials(CREDENTIALS.as_bytes()).unwrap(),
        )
        .unwrap();
        let other = ws.join("Analytics").join(".dbeaver");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("data-sources-2.json"),
            r#"{"connections": {"pg-x": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "Warehouse", "folder": "BI",
                "configuration": {"host": "wh", "port": "5432", "database": "dw"}}}}"#,
        )
        .unwrap();
        // Unreadable credentials: warning, connections still imported.
        std::fs::write(other.join("credentials-config.json"), [0u8; 48]).unwrap();
        std::fs::create_dir_all(ws.join(".metadata")).unwrap();
        let projects = find_projects(&ws);
        assert_eq!(projects.len(), 2);
        let r = import_workspace(&ws).unwrap();
        assert_eq!(r.connections.len(), 9);
        let wh = r.connections.iter().find(|c| c.id == "pg-x").unwrap();
        assert_eq!(wh.folder.as_deref(), Some("Analytics/BI"));
        assert!(r.folders.contains(&"Analytics".to_string()));
        assert!(r.folders.contains(&"Analytics/BI".to_string()));
        let pg = r
            .connections
            .iter()
            .find(|c| c.id == "postgres-jdbc-1")
            .unwrap();
        assert_eq!(
            pg.folder.as_deref(),
            Some("Prod/EU"),
            "General project is not prefixed"
        );
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("passwords were not imported"))
        );
        assert!(import_workspace(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn legacy_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<data-sources>
  <folder name="Old"/>
  <data-source id="postgres-jdbc-old" provider="postgresql" driver="postgres-jdbc" name="Legacy &amp; PG" save-password="true" read-only="false" folder="Old">
    <connection host="legacy.db" port="5433" server="" database="app" url="jdbc:postgresql://legacy.db:5433/app" user="olduser" password="ENCRYPTED" type="prod">
      <network-handler type="TUNNEL" id="ssh_tunnel" enabled="true" user="sshuser" save-password="true">
        <property name="host" value="jump.old"/>
        <property name="port" value="22"/>
        <property name="authType" value="PASSWORD"/>
      </network-handler>
      <bootstrap autocommit="false"/>
      <property name="connectTimeout" value="10"/>
    </connection>
  </data-source>
  <data-source id="sqlite-old" provider="sqlite" driver="sqlite_jdbc" name="File">
    <connection database="/tmp/old.db" url="jdbc:sqlite:/tmp/old.db"/>
  </data-source>
</data-sources>"#;
        let r = parse_legacy_xml(xml).unwrap();
        assert_eq!(r.connections.len(), 2, "{:?}", r.warnings);
        let pg = &r.connections[0];
        assert_eq!(pg.name, "Legacy & PG");
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_deref()),
            ("legacy.db", Some(5433), Some("app"))
        );
        assert_eq!(pg.user.as_deref(), Some("olduser"));
        assert_eq!(pg.connection_type, "prod");
        assert_eq!(pg.folder.as_deref(), Some("Old"));
        let ssh = pg.ssh.as_ref().unwrap();
        assert_eq!(
            (ssh.host.as_str(), ssh.user.as_str()),
            ("jump.old", "sshuser")
        );
        assert_eq!(
            pg.properties.get("connectTimeout").map(String::as_str),
            Some("10")
        );
        assert!(r.secrets.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("legacy")));
        assert_eq!(r.connections[1].file_path.as_deref(), Some("/tmp/old.db"));
    }
}
