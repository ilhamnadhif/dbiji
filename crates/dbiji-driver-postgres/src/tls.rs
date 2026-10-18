//! TLS configuration (rustls with the aws-lc-rs provider) for every
//! [`SslMode`]:
//!
//! | mode        | negotiation          | verification                         |
//! |-------------|----------------------|--------------------------------------|
//! | disable     | none                 | -                                    |
//! | prefer      | TLS if offered       | none                                 |
//! | require     | TLS required         | none                                 |
//! | verify-ca   | TLS required         | chain only (no host name check)      |
//! | verify-full | TLS required         | chain + host name (`tls_host()`)     |
//!
//! Trust anchors are the operating system store plus the Mozilla roots of
//! `webpki-roots`, plus the configured CA certificate (or only the latter
//! with `only_custom_ca`). Client certificates enable mutual TLS.

use dbiji_core::config::{SslConfig, SslMode};
use dbiji_core::{DbError, DbResult};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio_postgres::config::SslMode as PgSslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

/// Everything needed to (re)establish TLS for a session and its cancel requests.
#[derive(Clone)]
pub(crate) struct TlsSetup {
    /// Negotiation mode handed to tokio-postgres.
    pub mode: PgSslMode,
    pub connector: MakeRustlsConnect,
    /// Name used for SNI and (verify-full) certificate verification.
    pub server_name: String,
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER.get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider())).clone()
}

/// System + webpki roots, loaded once per process (loading the OS store is expensive).
fn default_roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut store = RootCertStore::empty();
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let native = rustls_native_certs::load_native_certs();
            if !native.errors.is_empty() {
                tracing::debug!("some system certificates could not be loaded: {}", native.errors.len());
            }
            let (added, ignored) = store.add_parsable_certificates(native.certs);
            tracing::debug!("loaded {added} system root certificates ({ignored} ignored)");
            Arc::new(store)
        })
        .clone()
}

fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// Read PEM data given either inline (`-----BEGIN ...`) or as a file path.
fn read_pem(source: &str, what: &str) -> DbResult<Vec<u8>> {
    if source.contains("-----BEGIN") {
        return Ok(source.as_bytes().to_vec());
    }
    let path = expand_path(source.trim());
    std::fs::read(&path).map_err(|e| DbError::config(format!("Cannot read the {what} file '{}': {e}", path.display())))
}

fn load_certs(source: &str, what: &str) -> DbResult<Vec<CertificateDer<'static>>> {
    let pem = read_pem(source, what)?;
    let mut rd = std::io::BufReader::new(pem.as_slice());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut rd)
        .collect::<Result<_, _>>()
        .map_err(|e| DbError::config(format!("Invalid PEM in the {what}: {e}")))?;
    if certs.is_empty() {
        return Err(DbError::config(format!("The {what} contains no PEM certificate")));
    }
    Ok(certs)
}

fn load_key(source: &str) -> DbResult<PrivateKeyDer<'static>> {
    let pem = read_pem(source, "client key")?;
    let mut rd = std::io::BufReader::new(pem.as_slice());
    rustls_pemfile::private_key(&mut rd)
        .map_err(|e| DbError::config(format!("Invalid PEM in the client key: {e}")))?
        .ok_or_else(|| DbError::config("The client key file contains no private key (PKCS#1, PKCS#8 or SEC1 PEM)"))
}

fn root_store(ssl: &SslConfig) -> DbResult<Arc<RootCertStore>> {
    let custom = match ssl.ca_cert.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(src) => load_certs(src, "CA certificate")?,
        None => Vec::new(),
    };
    if ssl.only_custom_ca && custom.is_empty() {
        return Err(DbError::config("\"Use only the custom CA\" requires a CA certificate"));
    }
    if custom.is_empty() {
        return Ok(default_roots());
    }
    let mut store = if ssl.only_custom_ca { RootCertStore::empty() } else { (*default_roots()).clone() };
    for c in custom {
        store
            .add(c)
            .map_err(|e| DbError::config(format!("The CA certificate cannot be used as trust anchor: {e}")))?;
    }
    Ok(Arc::new(store))
}

/// Accepts any certificate (prefer / require) but still checks handshake signatures.
#[derive(Debug)]
struct NoVerifier {
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// verify-ca: the chain must lead to a trusted root, the host name is not checked.
#[derive(Debug)]
struct ChainVerifier {
    roots: Arc<RootCertStore>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(&cert, &self.roots, intermediates, now, self.algs.all)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Normalise a host for use as TLS server name (`[::1]` → `::1`).
fn normalize_server_name(host: &str) -> String {
    let h = host.trim();
    let h = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h);
    if h.is_empty() { "localhost".to_string() } else { h.to_string() }
}

/// Build the TLS setup for `ssl`, verifying against `server_name`.
pub(crate) fn build(ssl: &SslConfig, server_name: &str) -> DbResult<TlsSetup> {
    let prov = provider();
    let algs = prov.signature_verification_algorithms;
    let server_name = normalize_server_name(server_name);
    if ServerName::try_from(server_name.clone()).is_err() && ssl.mode.uses_tls() {
        return Err(DbError::config(format!("'{server_name}' is not a valid TLS server name")));
    }
    let builder = ClientConfig::builder_with_provider(prov.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::tls(format!("TLS initialisation failed: {e}")))?;
    let mode = match ssl.mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => PgSslMode::Require,
    };
    let with_verifier = match ssl.mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerifier { algs }))
        }
        SslMode::VerifyCa => {
            let roots = root_store(ssl)?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(ChainVerifier { roots, algs }))
        }
        SslMode::VerifyFull => {
            let roots = root_store(ssl)?;
            let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(roots, prov.clone())
                .build()
                .map_err(|e| DbError::tls(format!("Cannot create the certificate verifier: {e}")))?;
            builder.with_webpki_verifier(verifier)
        }
    };
    let cert = ssl.client_cert.as_deref().filter(|s| !s.trim().is_empty());
    let key = ssl.client_key.as_deref().filter(|s| !s.trim().is_empty());
    let mut config = match (cert, key) {
        (Some(c), Some(k)) if ssl.mode.uses_tls() => {
            let chain = load_certs(c, "client certificate")?;
            let key = load_key(k)?;
            with_verifier
                .with_client_auth_cert(chain, key)
                .map_err(|e| DbError::config(format!("The client certificate and key cannot be used: {e}")))?
        }
        (Some(_), None) if ssl.mode.uses_tls() => {
            return Err(DbError::config("A client certificate requires a client key"));
        }
        (None, Some(_)) if ssl.mode.uses_tls() => {
            return Err(DbError::config("A client key requires a client certificate"));
        }
        _ => with_verifier.with_no_client_auth(),
    };
    // PostgreSQL 17 checks the ALPN protocol when the client offers one.
    config.alpn_protocols = vec![b"postgresql".to_vec()];
    Ok(TlsSetup { mode, connector: MakeRustlsConnect::new(config), server_name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_every_mode_without_files() {
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            let ssl = SslConfig { mode, ..Default::default() };
            let t = build(&ssl, "db.example.com").unwrap();
            assert_eq!(t.server_name, "db.example.com");
        }
        assert_eq!(build(&SslConfig::default(), "[::1]").unwrap().server_name, "::1");
    }

    #[test]
    fn config_errors() {
        let ssl = SslConfig { mode: SslMode::VerifyCa, only_custom_ca: true, ..Default::default() };
        assert_eq!(build(&ssl, "h").err().unwrap().kind, dbiji_core::ErrorKind::Config);
        let ssl = SslConfig { mode: SslMode::VerifyFull, ca_cert: Some("/nonexistent/ca.pem".into()), ..Default::default() };
        let e = build(&ssl, "h").err().unwrap();
        assert_eq!(e.kind, dbiji_core::ErrorKind::Config);
        assert!(e.message.contains("/nonexistent/ca.pem"));
        let ssl = SslConfig { mode: SslMode::Require, client_cert: Some("x".into()), ..Default::default() };
        assert!(build(&ssl, "h").is_err());
        let ssl = SslConfig { mode: SslMode::Require, ca_cert: Some("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".into()), ..Default::default() };
        // not used in require mode: no error
        assert!(build(&ssl, "h").is_ok());
    }
}
