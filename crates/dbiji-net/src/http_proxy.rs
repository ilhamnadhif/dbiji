//! HTTP CONNECT proxy client (RFC 9110 §9.3.6) with Basic authentication.
//!
//! The response head is read byte by byte so no data that the target server
//! sends right after the `200` reply (e.g. the MySQL greeting) is consumed.

use crate::stream::display_addr;
use base64::Engine;
use dbiji_core::{DbError, DbResult, Secret};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// Upper bound of the proxy response head.
const MAX_HEAD: usize = 16 * 1024;

/// Credentials for `Proxy-Authorization: Basic`.
#[derive(Clone, Copy)]
pub struct HttpProxyCredentials<'a> {
    /// Proxy user name.
    pub user: &'a str,
    /// Proxy password.
    pub password: &'a Secret,
}

impl std::fmt::Debug for HttpProxyCredentials<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProxyCredentials")
            .field("user", &self.user)
            .field("password", &"***")
            .finish()
    }
}

/// Asks the HTTP proxy on `stream` to open a tunnel to `host:port`.
/// On success `stream` is a transparent pipe to the target.
///
/// Errors: [`dbiji_core::ErrorKind::Auth`] for `407 Proxy Authentication
/// Required`, [`dbiji_core::ErrorKind::Connection`] for any other non-2xx
/// status or a malformed response.
pub async fn http_connect<S>(
    stream: &mut S,
    host: &str,
    port: u16,
    credentials: Option<HttpProxyCredentials<'_>>,
) -> DbResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let authority = display_addr(host, port);
    let mut req = Zeroizing::new(Vec::with_capacity(256));
    req.extend_from_slice(
        format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").as_bytes(),
    );
    if let Some(c) = credentials {
        let mut raw = Zeroizing::new(Vec::with_capacity(
            c.user.len() + 1 + c.password.expose().len(),
        ));
        raw.extend_from_slice(c.user.as_bytes());
        raw.push(b':');
        raw.extend_from_slice(c.password.expose().as_bytes());
        let token = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(&*raw));
        req.extend_from_slice(b"Proxy-Authorization: Basic ");
        req.extend_from_slice(token.as_bytes());
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"User-Agent: DBiji\r\n\r\n");
    stream.write_all(&req).await.map_err(io_err)?;
    stream.flush().await.map_err(io_err)?;

    let head = read_head(stream).await?;
    let status_line = head.lines().next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let code: u16 = parts
        .next()
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(0);
    let reason = parts.next().unwrap_or("").trim();
    if !version.starts_with("HTTP/") || code == 0 {
        return Err(DbError::connection(format!(
            "The proxy did not answer as an HTTP proxy (got '{}')",
            status_line.chars().take(80).collect::<String>()
        )));
    }
    match code {
        200..=299 => Ok(()),
        407 => Err(if credentials.is_some() {
            DbError::auth(
                "The HTTP proxy rejected the user name or password (407 Proxy Authentication Required)",
            )
        } else {
            DbError::auth(
                "The HTTP proxy requires authentication (407), but no credentials are configured",
            )
        }),
        _ => Err(DbError::connection(format!(
            "The HTTP proxy refused to connect to {authority}: {code} {reason}"
        ))),
    }
}

async fn read_head<S>(stream: &mut S) -> DbResult<String>
where
    S: AsyncRead + Unpin + ?Sized,
{
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.map_err(io_err)?;
        if n == 0 {
            return Err(DbError::connection(
                "The HTTP proxy closed the connection before answering",
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(DbError::connection(
                "The HTTP proxy response header is too large",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::connection(format!("HTTP proxy connection failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn scripted(mut s: tokio::io::DuplexStream, response: &'static str) -> String {
        let mut got = Vec::new();
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).await.unwrap();
            got.push(b[0]);
        }
        s.write_all(response.as_bytes()).await.unwrap();
        // Data from the target immediately after the head must not be eaten.
        s.write_all(b"GREETING").await.unwrap();
        String::from_utf8(got).unwrap()
    }

    #[tokio::test]
    async fn sends_basic_auth_and_preserves_following_bytes() {
        let (mut client, server) = duplex(4096);
        let srv = tokio::spawn(scripted(
            server,
            "HTTP/1.1 200 Connection established\r\n\r\n",
        ));
        let pw = Secret::new("s3cret");
        http_connect(
            &mut client,
            "db.example",
            3306,
            Some(HttpProxyCredentials {
                user: "alice",
                password: &pw,
            }),
        )
        .await
        .unwrap();
        let req = srv.await.unwrap();
        assert!(
            req.starts_with("CONNECT db.example:3306 HTTP/1.1\r\n"),
            "{req}"
        );
        let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
        assert!(
            req.contains(&format!("Proxy-Authorization: Basic {expected}")),
            "{req}"
        );
        let mut greeting = [0u8; 8];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"GREETING");
    }

    #[tokio::test]
    async fn status_407_is_auth_error() {
        let (mut client, server) = duplex(4096);
        let srv = tokio::spawn(scripted(
            server,
            "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
        ));
        let err = http_connect(&mut client, "h", 1, None).await.unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Auth);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn other_status_is_connection_error() {
        let (mut client, server) = duplex(4096);
        let srv = tokio::spawn(scripted(server, "HTTP/1.0 403 Forbidden\r\n\r\n"));
        let err = http_connect(&mut client, "::1", 5432, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Connection);
        assert!(err.message.contains("403 Forbidden"), "{}", err.message);
        assert!(err.message.contains("[::1]:5432"), "{}", err.message);
        let _ = srv.await;
    }

    #[tokio::test]
    async fn garbage_response() {
        let (mut client, server) = duplex(4096);
        let srv = tokio::spawn(scripted(server, "SSH-2.0-OpenSSH\r\n\r\n"));
        let err = http_connect(&mut client, "h", 1, None).await.unwrap_err();
        assert!(
            err.message.contains("did not answer as an HTTP proxy"),
            "{}",
            err.message
        );
        let _ = srv.await;
    }
}
