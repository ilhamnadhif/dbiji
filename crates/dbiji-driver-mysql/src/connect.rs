//! Turning [`ConnectParams`] into `mysql_async` options, TLS configuration,
//! connection establishment (with the `Prefer` fallback) and short-lived side
//! connections used for `KILL QUERY`.

use crate::convert::{self, Charset};
use crate::error::{codes, map_error};
use dbiji_core::config::SslMode;
use dbiji_core::{ConnectParams, DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{ClientIdentity, Conn, Opts, OptsBuilder, SslOpts};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// TLS server name used for `VerifyCa`.
///
/// `mysql_async` 0.37 implements "skip domain validation" by searching the
/// rustls error text for `NotValidForName`, but rustls 0.23 reports name
/// mismatches as `NotValidForNameContext`, whose message is "certificate not
/// valid for name <expected> ...". Using a server name that contains the
/// marker makes the check work again: the certificate chain is still fully
/// verified against the trust anchors, only the (guaranteed) name mismatch is
/// accepted. If a future version changes the message, verification fails
/// closed. The name is a reserved `.invalid` domain, so it is also sent as SNI.
pub const VERIFY_CA_TLS_NAME: &str = "dbiji-verify-ca.NotValidForName.invalid";

/// Default TCP keepalive time.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(60);

/// Upper bound for one `KILL QUERY` attempt (connect + statement).
const KILL_EXTRA_TIMEOUT: Duration = Duration::from_secs(5);

/// Connection settings derived from the driver properties.
#[derive(Debug, Clone)]
pub struct ConnSettings {
    /// Character set for `SET NAMES`.
    pub charset_name: String,
    /// Optional collation for `SET NAMES ... COLLATE`.
    pub collation: Option<String>,
    /// Encoding of SQL text and parameters.
    pub wire_charset: Charset,
    /// Statements run after connecting, in order.
    pub setup: Vec<String>,
}

fn prop<'a>(params: &'a ConnectParams, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| params.property(k))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn prop_bool(params: &ConnectParams, keys: &[&str]) -> Option<bool> {
    prop(params, keys)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn simple_name(v: &str, what: &str) -> DbResult<String> {
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(v.to_string())
    } else {
        Err(DbError::config(format!("Invalid {what} '{v}'")))
    }
}

fn simple_literal(v: &str, what: &str) -> DbResult<String> {
    if v.contains(['\'', '\\', '\0']) {
        Err(DbError::config(format!("Invalid {what} value")))
    } else {
        Ok(format!("'{v}'"))
    }
}

/// Parse the driver properties into settings and session setup statements.
///
/// Supported properties (all optional):
/// * `charset` (default `utf8mb4`), `collation` — `SET NAMES`
/// * `time_zone`, `sql_mode` — session variables
/// * `transaction_isolation` / `isolation` — `READ COMMITTED`, `REPEATABLE READ`, ...
/// * `init_sql` / `init` / `initStatements` — statements run after connecting (`;` separated)
pub fn settings(params: &ConnectParams) -> DbResult<ConnSettings> {
    let charset_name = simple_name(
        prop(params, &["charset", "characterEncoding"]).unwrap_or("utf8mb4"),
        "charset",
    )?;
    let charset_name = if charset_name.eq_ignore_ascii_case("utf-8") {
        "utf8mb4".to_string()
    } else {
        charset_name
    };
    let wire_charset = convert::charset_by_name(&charset_name).ok_or_else(|| {
        DbError::config(format!(
            "Unsupported connection character set '{charset_name}'"
        ))
    })?;
    let collation = match prop(params, &["collation", "connectionCollation"]) {
        Some(c) => Some(simple_name(c, "collation")?),
        None => None,
    };
    let mut setup = Vec::new();
    match &collation {
        Some(c) => setup.push(format!("SET NAMES {charset_name} COLLATE {c}")),
        None => setup.push(format!("SET NAMES {charset_name}")),
    }
    if params.read_only {
        setup.push("SET SESSION TRANSACTION READ ONLY".to_string());
    }
    if !params.auto_commit {
        setup.push("SET autocommit = 0".to_string());
    }
    if let Some(tz) = prop(params, &["time_zone", "timezone", "serverTimezone"]) {
        setup.push(format!(
            "SET time_zone = {}",
            simple_literal(tz, "time_zone")?
        ));
    }
    if let Some(mode) = prop(params, &["sql_mode", "sessionVariables.sql_mode"]) {
        setup.push(format!(
            "SET SESSION sql_mode = {}",
            simple_literal(mode, "sql_mode")?
        ));
    }
    if let Some(iso) = prop(
        params,
        &["transaction_isolation", "isolation", "transactionIsolation"],
    ) {
        let norm = iso.to_ascii_uppercase().replace(['_', '-'], " ");
        let level = match norm.trim() {
            "READ UNCOMMITTED" => "READ UNCOMMITTED",
            "READ COMMITTED" => "READ COMMITTED",
            "REPEATABLE READ" => "REPEATABLE READ",
            "SERIALIZABLE" => "SERIALIZABLE",
            _ => {
                return Err(DbError::config(format!(
                    "Invalid transaction isolation level '{iso}'"
                )));
            }
        };
        setup.push(format!("SET SESSION TRANSACTION ISOLATION LEVEL {level}"));
    }
    if let Some(init) = prop(
        params,
        &["init_sql", "init", "initStatements", "init_statements"],
    ) {
        setup.extend(crate::sqltext::split_statements(init));
    }
    Ok(ConnSettings {
        charset_name,
        collation,
        wire_charset,
        setup,
    })
}

/// Make sure a rustls crypto provider is installed (several may be compiled
/// into the application; rustls refuses to guess in that case).
fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

static NATIVE_ROOTS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();

/// Operating system trust store (loaded once, unparsable certificates skipped).
async fn native_roots() -> Vec<Vec<u8>> {
    if let Some(r) = NATIVE_ROOTS.get() {
        return r.clone();
    }
    let loaded = tokio::task::spawn_blocking(|| {
        let res = rustls_native_certs::load_native_certs();
        if !res.errors.is_empty() {
            tracing::debug!(
                "some system root certificates could not be loaded: {} error(s)",
                res.errors.len()
            );
        }
        res.certs
            .into_iter()
            .filter(|c| rustls::RootCertStore::empty().add(c.clone()).is_ok())
            .map(|c| c.to_vec())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    NATIVE_ROOTS.get_or_init(|| loaded).clone()
}

/// Read a PEM (or DER) certificate file into DER certificates.
fn read_certificates(path: &str) -> DbResult<Vec<Vec<u8>>> {
    let data = std::fs::read(path)
        .map_err(|e| DbError::config(format!("Cannot read CA certificate file '{path}': {e}")))?;
    let mut out = Vec::new();
    for cert in CertificateDer::pem_slice_iter(&data) {
        match cert {
            Ok(c) => out.push(c.to_vec()),
            Err(e) => return Err(DbError::config(format!("Invalid PEM in '{path}': {e}"))),
        }
    }
    if out.is_empty() && !data.is_empty() && !data.starts_with(b"-----") {
        out.push(data);
    }
    let mut valid = Vec::new();
    for der in out {
        let mut store = rustls::RootCertStore::empty();
        if store.add(CertificateDer::from(der.clone())).is_ok() {
            valid.push(der);
        }
    }
    if valid.is_empty() {
        return Err(DbError::config(format!(
            "No usable CA certificate found in '{path}'"
        )));
    }
    Ok(valid)
}

fn client_identity(params: &ConnectParams) -> DbResult<Option<ClientIdentity>> {
    let cert = params
        .ssl
        .client_cert
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let key = params
        .ssl
        .client_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (cert, key) {
        (None, None) => Ok(None),
        (Some(c), Some(k)) => {
            for (p, what) in [(c, "client certificate"), (k, "client key")] {
                if !std::path::Path::new(p).is_file() {
                    return Err(DbError::config(format!(
                        "The TLS {what} file '{p}' does not exist"
                    )));
                }
            }
            Ok(Some(ClientIdentity::new(
                PathBuf::from(c).into(),
                PathBuf::from(k).into(),
            )))
        }
        _ => Err(DbError::config(
            "Mutual TLS needs both a client certificate and a client key",
        )),
    }
}

/// TLS options for `mode` (`None` = plain connection).
pub async fn tls_options(params: &ConnectParams, mode: SslMode) -> DbResult<Option<SslOpts>> {
    if mode == SslMode::Disable {
        return Ok(None);
    }
    ensure_crypto_provider();
    let mut ssl = SslOpts::default().with_client_identity(client_identity(params)?);
    match mode {
        SslMode::Disable => return Ok(None),
        SslMode::Prefer | SslMode::Require => {
            ssl = ssl.with_danger_accept_invalid_certs(true);
        }
        SslMode::VerifyCa | SslMode::VerifyFull => {
            let mut roots: Vec<Vec<u8>> = Vec::new();
            if let Some(ca) = params
                .ssl
                .ca_cert
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                roots.extend(read_certificates(ca)?);
            }
            if params.ssl.only_custom_ca {
                if roots.is_empty() {
                    return Err(DbError::config(
                        "'Use only the custom CA' requires a CA certificate file",
                    ));
                }
                ssl = ssl.with_disable_built_in_roots(true);
            } else {
                roots.extend(native_roots().await);
            }
            ssl = ssl.with_root_certs(roots.into_iter().map(Into::into).collect());
            if mode == SslMode::VerifyCa {
                ssl = ssl
                    .with_danger_skip_domain_validation(true)
                    .with_danger_tls_hostname_override(Some(VERIFY_CA_TLS_NAME));
            } else if params.tls_server_name.is_some() {
                ssl = ssl.with_danger_tls_hostname_override(Some(params.tls_host().to_string()));
            }
        }
    }
    Ok(Some(ssl))
}

/// Base options (no TLS) for `params`.
pub fn base_options(params: &ConnectParams) -> DbResult<OptsBuilder> {
    let mut b = OptsBuilder::default()
        .ip_or_hostname(params.host.clone())
        .tcp_port(params.port)
        .user(params.user.clone())
        .pass(params.password.as_ref().map(|p| p.expose().to_string()))
        .db_name(params.database.clone().filter(|d| !d.is_empty()))
        .prefer_socket(false)
        .tcp_nodelay(prop_bool(params, &["tcp_nodelay"]).unwrap_or(true))
        .stmt_cache_size(32usize);
    let keepalive = match prop(
        params,
        &["tcp_keepalive", "tcp_keepalive_secs", "tcpKeepAlive"],
    ) {
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "false" | "off" | "no" | "0" => None,
            "true" | "on" | "yes" => Some(DEFAULT_KEEPALIVE),
            n => Some(Duration::from_secs(n.parse::<u64>().map_err(|_| {
                DbError::config(format!("Invalid tcp_keepalive '{v}'"))
            })?)),
        },
        None => Some(DEFAULT_KEEPALIVE),
    };
    b = b.tcp_keepalive(keepalive);
    if let Some(c) = prop(params, &["compress", "compression", "useCompression"]) {
        let level = match c.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(mysql_async::Compression::fast()),
            "false" | "no" | "off" | "0" => None,
            "best" => Some(mysql_async::Compression::best()),
            n => match n.parse::<u32>() {
                Ok(l) if l <= 9 => Some(mysql_async::Compression::new(l)),
                _ => {
                    return Err(DbError::config(format!(
                        "Invalid compression setting '{c}'"
                    )));
                }
            },
        };
        b = b.compression(level);
    }
    if let Some(v) = prop(params, &["max_allowed_packet", "maxAllowedPacket"]) {
        let n = v
            .parse::<usize>()
            .map_err(|_| DbError::config(format!("Invalid max_allowed_packet '{v}'")))?;
        b = b.max_allowed_packet(Some(n));
    }
    if let Some(v) = prop(params, &["stmt_cache_size"]) {
        let n = v
            .parse::<usize>()
            .map_err(|_| DbError::config(format!("Invalid stmt_cache_size '{v}'")))?;
        b = b.stmt_cache_size(n);
    }
    if let Some(socket) = prop(params, &["socket", "unix_socket"]) {
        b = b.socket(Some(socket.to_string()));
    }
    if prop_bool(
        params,
        &["enable_cleartext_plugin", "enableCleartextPlugin"],
    )
    .unwrap_or(false)
    {
        b = b.enable_cleartext_plugin(true);
    }
    if prop_bool(params, &["client_found_rows", "useAffectedRows"]).is_some() {
        // `useAffectedRows=false` (JDBC) means "found rows".
        let found = match prop(params, &["client_found_rows"]) {
            Some(_) => prop_bool(params, &["client_found_rows"]).unwrap_or(false),
            None => !prop_bool(params, &["useAffectedRows"]).unwrap_or(true),
        };
        b = b.client_found_rows(found);
    }
    let program = prop(params, &["program_name"]).unwrap_or(params.application_name.as_str());
    if !program.is_empty() {
        b = b.connect_attribute("program_name", program.to_string());
    }
    Ok(b)
}

/// A freshly opened connection together with the options that worked.
pub struct Opened {
    /// The connection.
    pub conn: Conn,
    /// Options used (after a `Prefer` fallback this is the plain variant),
    /// reused for side connections.
    pub opts: Opts,
    /// The connection is encrypted.
    pub tls: bool,
}

async fn open(opts: Opts, params: &ConnectParams) -> DbResult<Conn> {
    let timeout = params.connect_timeout.max(Duration::from_secs(1));
    match tokio::time::timeout(timeout, Conn::new(opts)).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(connect_error(e, params)),
        Err(_) => Err(DbError::connection(format!(
            "Connection to {}:{} timed out after {} s",
            params.host,
            params.port,
            timeout.as_secs()
        ))),
    }
}

fn connect_error(e: mysql_async::Error, params: &ConnectParams) -> DbError {
    let mut err = map_error(e, None);
    if err.kind == dbiji_core::ErrorKind::Connection || err.kind == dbiji_core::ErrorKind::Timeout {
        err.kind = dbiji_core::ErrorKind::Connection;
        err.message = format!(
            "Cannot connect to {}:{}: {}",
            params.host, params.port, err.message
        );
    }
    err
}

/// Open a connection honoring the TLS mode (including the `Prefer` fallback).
pub async fn open_connection(params: &ConnectParams) -> DbResult<Opened> {
    let base = base_options(params)?;
    let mode = params.ssl.mode;
    let ssl = tls_options(params, mode).await?;
    let tls = ssl.is_some();
    let opts: Opts = base.clone().ssl_opts(ssl).into();
    match open(opts.clone(), params).await {
        Ok(conn) => Ok(Opened { conn, opts, tls }),
        // Prefer: without TLS when the server has none, or none DBiji can use
        // (MySQL 5.7 builds offer only DHE ciphers, rustls only ECDHE), as
        // the mysql client's PREFERRED does.
        Err(e) if mode == SslMode::Prefer && e.kind == dbiji_core::ErrorKind::Tls => {
            tracing::debug!(
                "TLS to {}:{} failed ({}), falling back to a plain connection",
                params.host,
                params.port,
                e.message
            );
            let plain: Opts = base.ssl_opts(None::<SslOpts>).into();
            let conn = open(plain.clone(), params).await?;
            Ok(Opened {
                conn,
                opts: plain,
                tls: false,
            })
        }
        Err(e) if e.kind == dbiji_core::ErrorKind::Tls && e.message.contains("HandshakeFailure") => Err(e.with_hint(
            "The server offers no TLS cipher DBiji can use (MySQL 5.7 offers only DHE ciphers). Use the SSL mode Prefer or Disable, or enable ECDHE ciphers (ssl_cipher) on the server.",
        )),
        Err(e) => Err(e),
    }
}

/// Kills statements of one server connection through short-lived side
/// connections opened with the same options.
pub struct Killer {
    opts: Opts,
    connection_id: u32,
    timeout: Duration,
}

impl Killer {
    /// Killer for server connection `connection_id`; `opts` are used for the
    /// side connections.
    pub fn new(opts: Opts, connection_id: u32, connect_timeout: Duration) -> Arc<Killer> {
        Arc::new(Killer {
            opts,
            connection_id,
            timeout: connect_timeout.max(Duration::from_secs(1)) + KILL_EXTRA_TIMEOUT,
        })
    }

    /// Server id of the target connection.
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// `KILL QUERY <id>`: interrupt the running statement, keep the session.
    pub async fn kill_query(&self) -> DbResult<()> {
        self.kill(format!("KILL QUERY {}", self.connection_id))
            .await
    }

    /// `KILL <id>`: terminate the connection.
    pub async fn kill_connection(&self) -> DbResult<()> {
        self.kill(format!("KILL {}", self.connection_id)).await
    }

    /// `KILL QUERY`, telling whether the target thread still existed.
    pub async fn kill_query_checked(&self) -> DbResult<bool> {
        self.kill_checked(format!("KILL QUERY {}", self.connection_id))
            .await
    }

    async fn kill(&self, sql: String) -> DbResult<()> {
        self.kill_checked(sql).await.map(|_| ())
    }

    /// `Ok(false)`: the server has no thread with that id (already gone).
    async fn kill_checked(&self, sql: String) -> DbResult<bool> {
        let opts = self.opts.clone();
        let fut = async move {
            let mut side = Conn::new(opts).await?;
            let r = side.query_drop(sql).await;
            let _ = side.disconnect().await;
            r
        };
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(mysql_async::Error::Server(e))) if e.code == codes::NO_SUCH_THREAD => Ok(false),
            Ok(Err(e)) => Err(map_error(e, None)),
            Err(_) => Err(DbError::timeout(
                "Timed out while sending KILL QUERY to the server",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::config::SslConfig;
    use std::collections::BTreeMap;

    fn params() -> ConnectParams {
        ConnectParams {
            kind: dbiji_core::DbKind::MySql,
            host: "127.0.0.1".into(),
            port: 3306,
            tls_server_name: None,
            database: Some("app".into()),
            user: Some("root".into()),
            password: Some("secret".into()),
            file_path: None,
            ssl: SslConfig::default(),
            properties: BTreeMap::new(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(5),
            read_only: false,
            auto_commit: true,
        }
    }

    #[test]
    fn default_setup() {
        let s = settings(&params()).unwrap();
        assert_eq!(s.setup, vec!["SET NAMES utf8mb4".to_string()]);
        assert!(s.wire_charset.is_utf8());
    }

    #[test]
    fn property_setup() {
        let mut p = params();
        p.read_only = true;
        p.auto_commit = false;
        p.properties.insert("charset".into(), "latin1".into());
        p.properties.insert("collation".into(), "latin1_bin".into());
        p.properties.insert("time_zone".into(), "+07:00".into());
        p.properties
            .insert("transaction_isolation".into(), "read-committed".into());
        p.properties
            .insert("init_sql".into(), "SET @a = 1; SET @b = ';'".into());
        let s = settings(&p).unwrap();
        assert_eq!(
            s.setup,
            vec![
                "SET NAMES latin1 COLLATE latin1_bin",
                "SET SESSION TRANSACTION READ ONLY",
                "SET autocommit = 0",
                "SET time_zone = '+07:00'",
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
                "SET @a = 1",
                "SET @b = ';'",
            ]
        );
        assert_eq!(s.wire_charset.name, "latin1");
    }

    #[test]
    fn rejects_injection() {
        let mut p = params();
        p.properties
            .insert("charset".into(), "utf8mb4; DROP TABLE x".into());
        assert!(settings(&p).is_err());
        let mut p = params();
        p.properties.insert("time_zone".into(), "x'; DROP".into());
        assert!(settings(&p).is_err());
        let mut p = params();
        p.properties.insert("charset".into(), "klingon".into());
        assert!(settings(&p).is_err());
    }

    #[test]
    fn base_options_props() {
        let mut p = params();
        p.properties.insert("compress".into(), "true".into());
        p.properties.insert("tcp_keepalive".into(), "30".into());
        let opts: Opts = base_options(&p).unwrap().into();
        assert!(opts.compression().is_some());
        assert_eq!(opts.tcp_keepalive(), Some(Duration::from_secs(30)));
        assert!(!opts.prefer_socket());
        assert_eq!(opts.db_name(), Some("app"));
        let mut p = params();
        p.properties.insert("compress".into(), "11".into());
        assert!(base_options(&p).is_err());
    }

    #[tokio::test]
    async fn tls_modes() {
        let p = params();
        assert!(tls_options(&p, SslMode::Disable).await.unwrap().is_none());
        let req = tls_options(&p, SslMode::Require).await.unwrap().unwrap();
        assert!(req.accept_invalid_certs());
        let mut p2 = params();
        p2.ssl.only_custom_ca = true;
        assert!(tls_options(&p2, SslMode::VerifyFull).await.is_err());
        p2.ssl.ca_cert = Some("/nonexistent/ca.pem".into());
        assert!(tls_options(&p2, SslMode::VerifyCa).await.is_err());
        let mut p3 = params();
        p3.ssl.client_cert = Some("/x.pem".into());
        assert!(tls_options(&p3, SslMode::Require).await.is_err());
        let mut p4 = params();
        p4.tls_server_name = Some("db.example.com".into());
        let full = tls_options(&p4, SslMode::VerifyFull)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(full.tls_hostname_override(), Some("db.example.com"));
        assert!(!full.accept_invalid_certs());
        let ca = tls_options(&params(), SslMode::VerifyCa)
            .await
            .unwrap()
            .unwrap();
        assert!(ca.skip_domain_validation());
        assert_eq!(ca.tls_hostname_override(), Some(VERIFY_CA_TLS_NAME));
    }

    #[test]
    fn prefer_fallback_detection() {
        let e = crate::error::map_error(
            mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer),
            None,
        );
        // A server without TLS is a TLS error: Prefer goes on without it (as
        // after a failed handshake).
        assert_eq!(e.kind, dbiji_core::ErrorKind::Tls);
    }

    #[test]
    fn verify_ca_marker_matches_rustls_message() {
        // The VerifyCa approach relies on the rustls error text containing the
        // expected server name; guard against changes in that message.
        use rustls::pki_types::ServerName;
        use rustls::{CertificateError, Error};
        let expected = ServerName::try_from(VERIFY_CA_TLS_NAME).unwrap().to_owned();
        let e = Error::InvalidCertificate(CertificateError::NotValidForNameContext {
            expected,
            presented: vec![],
        });
        assert!(e.to_string().contains("NotValidForName"), "{e}");
    }
}
