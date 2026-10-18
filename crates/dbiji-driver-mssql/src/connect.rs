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
        Err(e) => {
            let mut err = map_error(e, None);
            if err.kind == ErrorKind::Connection && err.message.contains("Protocol error") {
                err = err.with_hint("Check the TLS mode: the server may require (or not support) encryption");
            }
            Err((err, None))
        }
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
                if params.ssl.mode == SslMode::Prefer && level == EncryptionLevel::On && e.kind == ErrorKind::Tls {
                    level = EncryptionLevel::NotSupported;
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(DbError::connection("Too many server redirects while connecting"))
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
