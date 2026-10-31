//! Opening TDS connections: configuration from [`ConnectParams`], TCP (and
//! SQL Browser for named instances), TLS mode mapping and Azure gateway
//! redirects.

use crate::error::map_error;
use dbiji_core::config::SslMode;
use dbiji_core::{ConnectParams, DbError, DbResult, ErrorKind};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, SqlBrowser};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// The concrete client type used by the driver.
pub type MsClient = Client<Compat<TcpStream>>;

/// Encryption level and certificate trust for a TLS mode.
///
/// * `disable` → no TLS at all (`NotSupported`); if the server forces
///   encryption the certificate is accepted without validation.
/// * `prefer` → TLS when the server supports it (`On`), no validation.
/// * `require` → TLS mandatory (`Required`), no validation.
/// * `verify-ca` / `verify-full` → TLS mandatory with chain and host name
///   validation (tiberius cannot validate the chain without the host name,
///   so `verify-ca` is as strict as `verify-full`).
pub fn encryption_for(mode: SslMode) -> (EncryptionLevel, bool) {
    match mode {
        SslMode::Disable => (EncryptionLevel::NotSupported, false),
        SslMode::Prefer => (EncryptionLevel::On, false),
        SslMode::Require => (EncryptionLevel::Required, false),
        SslMode::VerifyCa | SslMode::VerifyFull => (EncryptionLevel::Required, true),
    }
}

fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on"))
}

/// Build the tiberius configuration. `host`/`port` override the parameters
/// (used for Azure gateway redirects).
pub fn build_config(params: &ConnectParams, host: &str, port: u16, level: EncryptionLevel, verify: bool) -> DbResult<Config> {
    let mut config = Config::new();
    config.host(host);
    config.port(port);
    if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
        config.database(db);
    }
    config.application_name(&params.application_name);
    if let Some(ws) = params.property("workstation_id").filter(|w| !w.is_empty()) {
        config.client_name(ws);
    }
    if let Some(ps) = params.property("packet_size").and_then(|p| p.parse::<u32>().ok()) {
        config.packet_size(ps);
    }
    if params.property("application_intent").is_some_and(|v| v.eq_ignore_ascii_case("readonly")) {
        config.readonly(true);
    }
    if truthy(params.property("multi_subnet_failover")) {
        config.multi_subnet_failover(true);
    }
    // The driver bounds statements itself (cancel / ExecOptions::timeout);
    // tiberius' default 30 s read timeout would break long queries.
    config.command_timeout(None);
    config.handshake_timeout(Some(params.connect_timeout));

    let user = params.user.clone().unwrap_or_default();
    let password = params.password.as_ref().map(|p| p.expose()).unwrap_or("");
    let auth = params.property("authentication").map(str::to_ascii_lowercase);
    match auth.as_deref() {
        Some("activedirectoryaccesstoken") | Some("aad_token") | Some("access_token") => {
            config.authentication(AuthMethod::aad_token(password));
        }
        _ => {
            if user.is_empty() {
                return Err(DbError::config("SQL Server authentication needs a user name (integrated authentication is not supported)"));
            }
            config.authentication(AuthMethod::sql_server(&user, password));
        }
    }

    config.encryption(level);
    let trust_override = truthy(params.property("trust_server_certificate"));
    if verify && !trust_override {
        if let Some(ca) = params.ssl.ca_cert.as_deref().filter(|c| !c.is_empty()) {
            config.trust_cert_ca(ca);
        }
        // after an Azure gateway redirect the certificate belongs to the new host
        let default_name = if host != params.host { host } else { params.tls_host() };
        let name = params.property("hostname_in_certificate").unwrap_or(default_name);
        config.hostname_in_certificate(name);
    } else {
        // also covers servers that force encryption when TLS was disabled
        config.trust_cert();
    }
    if let (Some(cert), Some(key)) = (params.ssl.client_cert.as_deref().filter(|c| !c.is_empty()), params.ssl.client_key.as_deref().filter(|c| !c.is_empty())) {
        config.client_certificate(cert, key);
    }
    Ok(config)
}

async fn tcp_connect(params: &ConnectParams, config: &Config, host: &str, port: u16, instance: Option<&str>) -> DbResult<TcpStream> {
    let timeout = params.connect_timeout;
    let fut = async {
        match instance {
            // Named instance through SQL Browser (UDP 1434) unless an
            // explicit, non-default port was configured.
            Some(inst) if port == 1433 => {
                let mut c = config.clone();
                c.instance_name(inst);
                c.port(1434);
                TcpStream::connect_named(&c).await.map_err(|e| map_error(e, None))
            }
            _ => {
                let stream = TcpStream::connect((host, port)).await?;
                stream.set_nodelay(true)?;
                Ok(stream)
            }
        }
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(DbError::new(e.kind, format!("Cannot connect to {host}:{port}: {}", e.message))),
        Err(_) => Err(DbError::timeout(format!("Connecting to {host}:{port} timed out after {:?}", timeout))),
    }
}

async fn connect_once(params: &ConnectParams, host: &str, port: u16, level: EncryptionLevel, verify: bool) -> Result<MsClient, (DbError, Option<(String, u16)>)> {
    let config = build_config(params, host, port, level, verify).map_err(|e| (e, None))?;
    let instance = params.property("instance").filter(|i| !i.is_empty());
    let tcp = tcp_connect(params, &config, host, port, instance).await.map_err(|e| (e, None))?;
    match Client::connect(config, tcp.compat_write()).await {
        Ok(c) => Ok(c),
        Err(tiberius::error::Error::Routing { host, port }) => Err((DbError::connection(format!("redirected to {host}:{port}")), Some((host, port)))),
        Err(e) => Err((with_tls_hint(map_error(e, None)), None)),
    }
}

/// Explain a TLS handshake failure, which is what a server speaking only
/// TLS 1.0/1.1 produces: `rustls` (and therefore every driver here) offers
/// TLS 1.2 and 1.3 only.
///
/// SQL Server 2005 and older negotiate TLS 1.0, so they cannot complete the
/// handshake with this build no matter which encryption mode is chosen — the
/// login packet itself is always encrypted from SQL Server 2005 on.
fn with_tls_hint(err: DbError) -> DbError {
    if err.kind == ErrorKind::Connection && err.message.contains("Protocol error") {
        return err.with_hint("Check the TLS mode: the server may require (or not support) encryption");
    }
    if err.kind != ErrorKind::Tls {
        return err;
    }
    let m = err.message.to_ascii_lowercase();
    let legacy = m.contains("protocol version") || m.contains("peer is incompatible") || m.contains("tls12notoffered") || m.contains("no cipher suites");
    if legacy {
        err.with_hint(
            "The server only offers TLS 1.0/1.1 (SQL Server 2008 and older), which this build does not support: \
             TLS 1.2 is the minimum. Set the SSL mode to `disable` to connect without TLS when the server does \
             not force encryption, or upgrade the server.",
        )
    } else if m.contains("certificate") || m.contains("unsupportedcertversion") {
        err.with_hint(
            "SQL Server's self-signed certificate was rejected. Choose the `require` TLS mode, set \
             `trust_server_certificate` in Driver properties, or give the server a proper certificate.",
        )
    } else {
        err
    }
}

/// Open a connection, following Azure SQL gateway redirects and falling
/// back to an unencrypted connection when `prefer` meets a server without
/// TLS support.
pub async fn open(params: &ConnectParams) -> DbResult<MsClient> {
    let (level, verify) = encryption_for(params.ssl.mode);
    let mut host = params.host.clone();
    let mut port = params.port;
    let mut level = level;
    for _attempt in 0..3 {
        match connect_once(params, &host, port, level, verify).await {
            Ok(c) => return Ok(c),
            Err((_, Some((h, p)))) => {
                tracing::debug!("SQL Server redirected the connection to {h}:{p}");
                host = h;
                port = p;
            }
            Err((e, None)) => {
                if should_retry_without_tls(params.ssl.mode, level, &e) {
                    level = EncryptionLevel::NotSupported;
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(DbError::connection("Too many server redirects while connecting"))
}

/// `prefer` means "TLS when the server offers it": when the handshake itself
/// fails, the connection is retried once without TLS.
///
/// The failure is not always classified as TLS — a server that only speaks
/// TLS 1.0/1.1 (SQL Server 2008 and older) closes the socket after our TLS 1.2
/// ClientHello, which arrives as a connection error — so every failure of the
/// first, encrypted attempt is retried unencrypted.
fn should_retry_without_tls(mode: SslMode, level: EncryptionLevel, err: &DbError) -> bool {
    if mode != SslMode::Prefer || level != EncryptionLevel::On {
        return false;
    }
    matches!(err.kind, ErrorKind::Tls | ErrorKind::Connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ConnectionConfig, DbKind, Secret};

    fn params(mode: SslMode) -> ConnectParams {
        let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "x");
        cfg.ssl.mode = mode;
        ConnectParams::from_config(&cfg, Some(Secret::new("pw")))
    }

    #[test]
    fn tls_mapping() {
        assert!(matches!(encryption_for(SslMode::Disable), (EncryptionLevel::NotSupported, false)));
        assert!(matches!(encryption_for(SslMode::Prefer), (EncryptionLevel::On, false)));
        assert!(matches!(encryption_for(SslMode::Require), (EncryptionLevel::Required, false)));
        assert!(matches!(encryption_for(SslMode::VerifyFull), (EncryptionLevel::Required, true)));
    }

    #[test]
    fn prefer_retries_without_tls_on_a_failed_handshake() {
        let tls = DbError::tls("received fatal alert");
        let dropped = DbError::connection("Network error: connection reset by peer");
        let auth = DbError::auth("Login failed for user 'sa'");
        assert!(should_retry_without_tls(SslMode::Prefer, EncryptionLevel::On, &tls));
        assert!(should_retry_without_tls(SslMode::Prefer, EncryptionLevel::On, &dropped));
        // A refused login is conclusive: TLS worked, the credentials did not.
        assert!(!should_retry_without_tls(SslMode::Prefer, EncryptionLevel::On, &auth));
        // Already unencrypted, or encryption was not optional.
        assert!(!should_retry_without_tls(SslMode::Prefer, EncryptionLevel::NotSupported, &tls));
        assert!(!should_retry_without_tls(SslMode::Require, EncryptionLevel::Required, &tls));
        assert!(!should_retry_without_tls(SslMode::VerifyFull, EncryptionLevel::Required, &tls));
    }

    #[test]
    fn legacy_tls_and_certificates_get_a_hint() {
        let legacy = with_tls_hint(DbError::tls("received fatal alert: protocol version: peer is incompatible: Tls12NotOffered"));
        assert!(legacy.hint.as_deref().unwrap_or_default().contains("TLS 1.2"), "{legacy:?}");
        let cert = with_tls_hint(DbError::tls("the server's certificate was rejected during the TLS handshake"));
        assert!(cert.hint.as_deref().unwrap_or_default().contains("trust_server_certificate"), "{cert:?}");
        let other = with_tls_hint(DbError::tls("something else"));
        assert!(other.hint.is_none());
    }

    #[test]
    fn config_builds() {
        for mode in SslMode::ALL {
            let p = params(mode);
            let (level, verify) = encryption_for(mode);
            let c = build_config(&p, &p.host, p.port, level, verify).unwrap();
            assert_eq!(c.get_addr(), "localhost:1433");
            assert_eq!(c.get_command_timeout(), None);
            // never leak the password through Debug
            assert!(!format!("{c:?}").contains("pw\""));
        }
        let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "x");
        cfg.user = None;
        let p = ConnectParams::from_config(&cfg, None);
        assert!(build_config(&p, "h", 1433, EncryptionLevel::Off, false).is_err());
    }
}
