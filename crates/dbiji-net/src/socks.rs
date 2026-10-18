//! SOCKS5 client (RFC 1928) with username/password authentication (RFC 1929).
//!
//! The handshake runs on any byte stream, so a SOCKS proxy can be reached
//! directly or through an SSH channel. Target host names are sent to the
//! proxy unresolved (domain-name addressing), so DNS resolution happens on
//! the proxy side like `socks5h://`.

use crate::stream::{display_addr, unbracket};
use dbiji_core::{DbError, DbResult, Secret};
use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

const VERSION: u8 = 0x05;
const METHOD_NONE: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_REJECTED: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// Credentials for RFC 1929 username/password authentication.
#[derive(Clone, Copy)]
pub struct SocksCredentials<'a> {
    /// User name (1 to 255 bytes).
    pub user: &'a str,
    /// Password (at most 255 bytes).
    pub password: &'a Secret,
}

impl std::fmt::Debug for SocksCredentials<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocksCredentials")
            .field("user", &self.user)
            .field("password", &"***")
            .finish()
    }
}

/// Performs method negotiation and (when requested by the proxy)
/// username/password authentication. After `Ok(())` the proxy expects a request.
pub async fn socks5_authenticate<S>(
    stream: &mut S,
    credentials: Option<SocksCredentials<'_>>,
) -> DbResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let greeting: &[u8] = if credentials.is_some() {
        &[VERSION, 2, METHOD_NONE, METHOD_USER_PASS]
    } else {
        &[VERSION, 1, METHOD_NONE]
    };
    stream.write_all(greeting).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(io_err)?;
    if reply[0] != VERSION {
        return Err(DbError::connection(format!(
            "The proxy did not answer as a SOCKS5 server (version byte {:#04x})",
            reply[0]
        )));
    }
    match reply[1] {
        METHOD_NONE => Ok(()),
        METHOD_USER_PASS => match credentials {
            Some(c) => user_pass_auth(stream, c).await,
            None => Err(DbError::auth(
                "The SOCKS5 proxy requires a user name and password, but none are configured",
            )),
        },
        METHOD_REJECTED => Err(if credentials.is_some() {
            DbError::auth("The SOCKS5 proxy rejected all offered authentication methods")
        } else {
            DbError::auth(
                "The SOCKS5 proxy requires authentication, but no credentials are configured",
            )
        }),
        other => Err(DbError::connection(format!(
            "The SOCKS5 proxy selected an unsupported authentication method {other:#04x}"
        ))),
    }
}

async fn user_pass_auth<S>(stream: &mut S, c: SocksCredentials<'_>) -> DbResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let user = c.user.as_bytes();
    let pass = c.password.expose().as_bytes();
    if user.is_empty() || user.len() > 255 {
        return Err(DbError::config(
            "SOCKS5 user names must be 1 to 255 bytes long",
        ));
    }
    if pass.len() > 255 {
        return Err(DbError::config(
            "SOCKS5 passwords must be at most 255 bytes long",
        ));
    }
    let mut msg = Zeroizing::new(Vec::with_capacity(3 + user.len() + pass.len()));
    msg.push(0x01);
    msg.push(user.len() as u8);
    msg.extend_from_slice(user);
    msg.push(pass.len() as u8);
    msg.extend_from_slice(pass);
    stream.write_all(&msg).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(io_err)?;
    if reply[1] != 0x00 {
        return Err(DbError::auth(format!(
            "The SOCKS5 proxy rejected the user name or password for user '{}'",
            c.user
        )));
    }
    Ok(())
}

/// Sends a CONNECT request for `host:port` and reads the reply. Must follow
/// [`socks5_authenticate`].
pub async fn socks5_request_connect<S>(stream: &mut S, host: &str, port: u16) -> DbResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let host = unbracket(host);
    let mut req = Vec::with_capacity(8 + host.len());
    req.extend_from_slice(&[VERSION, CMD_CONNECT, 0x00]);
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            req.push(ATYP_IPV4);
            req.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            req.push(ATYP_IPV6);
            req.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let name = host.as_bytes();
            if name.is_empty() || name.len() > 255 {
                return Err(DbError::config(format!(
                    "Host name '{host}' cannot be sent to a SOCKS5 proxy"
                )));
            }
            req.push(ATYP_DOMAIN);
            req.push(name.len() as u8);
            req.extend_from_slice(name);
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(io_err)?;
    if head[0] != VERSION {
        return Err(DbError::connection("Malformed reply from the SOCKS5 proxy"));
    }
    if head[1] != 0x00 {
        return Err(DbError::connection(format!(
            "The SOCKS5 proxy could not connect to {}: {}",
            display_addr(host, port),
            reply_text(head[1])
        )));
    }
    // Skip the bound address.
    let addr_len = match head[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await.map_err(io_err)?;
            usize::from(l[0])
        }
        other => {
            return Err(DbError::connection(format!(
                "The SOCKS5 proxy replied with an unknown address type {other:#04x}"
            )));
        }
    };
    let mut rest = vec![0u8; addr_len + 2];
    stream.read_exact(&mut rest).await.map_err(io_err)?;
    Ok(())
}

/// Full SOCKS5 handshake: authentication followed by a CONNECT to `host:port`.
/// On success `stream` is a transparent pipe to the target.
pub async fn socks5_connect<S>(
    stream: &mut S,
    host: &str,
    port: u16,
    credentials: Option<SocksCredentials<'_>>,
) -> DbResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    socks5_authenticate(stream, credentials).await?;
    socks5_request_connect(stream, host, port).await
}

fn reply_text(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::connection(format!("SOCKS5 proxy connection failed: {e}"))
}

/// Encodes an address the way it appears in a SOCKS5 request (used by tests and diagnostics).
#[doc(hidden)]
pub fn encode_address(host: &str) -> Vec<u8> {
    match unbracket(host).parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => [&[ATYP_IPV4][..], &ip.octets()].concat(),
        Ok(IpAddr::V6(ip)) => [&[ATYP_IPV6][..], &ip.octets()].concat(),
        Err(_) => [&[ATYP_DOMAIN, host.len() as u8][..], host.as_bytes()].concat(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Minimal scripted SOCKS5 server on one end of a duplex pipe.
    async fn fake_server(
        mut s: tokio::io::DuplexStream,
        require_auth: Option<(&'static str, &'static str)>,
        reply_code: u8,
    ) -> Vec<u8> {
        let mut head = [0u8; 2];
        s.read_exact(&mut head).await.unwrap();
        let mut methods = vec![0u8; head[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        match require_auth {
            Some((u, p)) => {
                if !methods.contains(&METHOD_USER_PASS) {
                    s.write_all(&[5, METHOD_REJECTED]).await.unwrap();
                    return Vec::new();
                }
                s.write_all(&[5, METHOD_USER_PASS]).await.unwrap();
                let mut v = [0u8; 2];
                s.read_exact(&mut v).await.unwrap();
                let mut user = vec![0u8; v[1] as usize];
                s.read_exact(&mut user).await.unwrap();
                let mut pl = [0u8; 1];
                s.read_exact(&mut pl).await.unwrap();
                let mut pass = vec![0u8; pl[0] as usize];
                s.read_exact(&mut pass).await.unwrap();
                let ok = user == u.as_bytes() && pass == p.as_bytes();
                s.write_all(&[1, if ok { 0 } else { 1 }]).await.unwrap();
                if !ok {
                    return Vec::new();
                }
            }
            None => s.write_all(&[5, METHOD_NONE]).await.unwrap(),
        }
        let mut req = [0u8; 4];
        s.read_exact(&mut req).await.unwrap();
        let mut addr = match req[3] {
            ATYP_IPV4 => vec![0u8; 4],
            ATYP_IPV6 => vec![0u8; 16],
            _ => {
                let mut l = [0u8; 1];
                s.read_exact(&mut l).await.unwrap();
                vec![0u8; l[0] as usize]
            }
        };
        s.read_exact(&mut addr).await.unwrap();
        let mut port = [0u8; 2];
        s.read_exact(&mut port).await.unwrap();
        s.write_all(&[5, reply_code, 0, ATYP_IPV4, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut full = vec![req[3]];
        full.extend_from_slice(&addr);
        full.extend_from_slice(&port);
        full
    }

    #[tokio::test]
    async fn connect_without_auth_uses_domain_addressing() {
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, None, 0));
        socks5_connect(&mut client, "db.internal", 5432, None)
            .await
            .unwrap();
        let req = srv.await.unwrap();
        assert_eq!(req[0], ATYP_DOMAIN);
        assert_eq!(&req[1..req.len() - 2], b"db.internal");
        assert_eq!(&req[req.len() - 2..], &5432u16.to_be_bytes());
    }

    #[tokio::test]
    async fn ipv6_literal_is_binary_encoded() {
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, None, 0));
        socks5_connect(&mut client, "[::1]", 3306, None)
            .await
            .unwrap();
        let req = srv.await.unwrap();
        assert_eq!(req[0], ATYP_IPV6);
        assert_eq!(req.len(), 1 + 16 + 2);
    }

    #[tokio::test]
    async fn auth_success_and_failure() {
        let pw = Secret::new("pa55");
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, Some(("bob", "pa55")), 0));
        let creds = SocksCredentials {
            user: "bob",
            password: &pw,
        };
        socks5_connect(&mut client, "10.0.0.5", 5432, Some(creds))
            .await
            .unwrap();
        assert_eq!(srv.await.unwrap()[0], ATYP_IPV4);

        let bad = Secret::new("wrong");
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, Some(("bob", "pa55")), 0));
        let err = socks5_connect(
            &mut client,
            "10.0.0.5",
            5432,
            Some(SocksCredentials {
                user: "bob",
                password: &bad,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Auth);
        assert!(!err.full_text().contains("wrong"));
        let _ = srv.await;
    }

    #[tokio::test]
    async fn missing_credentials_is_auth_error() {
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, Some(("bob", "x")), 0));
        let err = socks5_connect(&mut client, "h", 1, None).await.unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Auth);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn refused_reply_is_reported() {
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, None, 0x05));
        let err = socks5_connect(&mut client, "db", 5432, None)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("connection refused"),
            "{}",
            err.message
        );
        assert!(err.message.contains("db:5432"), "{}", err.message);
        let _ = srv.await;
    }

    #[test]
    fn address_encoding() {
        assert_eq!(encode_address("1.2.3.4"), vec![1, 1, 2, 3, 4]);
        assert_eq!(encode_address("ab")[..2], [3, 2]);
        assert_eq!(encode_address("[::1]").len(), 17);
    }
}
