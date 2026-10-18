//! Boxed byte streams and TCP connection helpers shared by the proxy and SSH code.

use dbiji_core::{DbError, DbResult, ErrorKind};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// A bidirectional byte stream (TCP socket, proxied socket or SSH channel).
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

/// Type erased [`AsyncStream`], used to chain hops of different kinds.
pub type BoxedStream = Box<dyn AsyncStream>;

/// Removes the brackets of an IPv6 literal (`[::1]` → `::1`).
pub(crate) fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// `host:port` for messages, with brackets around IPv6 literals.
pub(crate) fn display_addr(host: &str, port: u16) -> String {
    let h = unbracket(host);
    if h.contains(':') {
        format!("[{h}]:{port}")
    } else {
        format!("{h}:{port}")
    }
}

/// Opens a TCP connection to `host:port`, trying every resolved address in
/// turn, with an overall `timeout`. Errors carry a human readable reason.
pub(crate) async fn tcp_connect(host: &str, port: u16, timeout: Duration) -> DbResult<TcpStream> {
    let target = display_addr(host, port);
    let host_only = unbracket(host).to_string();
    let attempt = async {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host_only.as_str(), port))
            .await
            .map_err(|e| DbError::connection(format!("Cannot resolve host '{host_only}': {e}")))?
            .collect();
        if addrs.is_empty() {
            return Err(DbError::connection(format!(
                "Host '{host_only}' did not resolve to any address"
            )));
        }
        let mut last: Option<std::io::Error> = None;
        for addr in addrs {
            match TcpStream::connect(addr).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        let reason = last
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".into());
        Err(DbError::connection(format!(
            "Cannot connect to {target}: {reason}"
        )))
    };
    match tokio::time::timeout(timeout, attempt).await {
        Ok(r) => r,
        Err(_) => Err(DbError::new(
            ErrorKind::Timeout,
            format!(
                "Timed out after {}s connecting to {target}",
                timeout.as_secs().max(1)
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_formatting() {
        assert_eq!(display_addr("db.local", 5432), "db.local:5432");
        assert_eq!(display_addr("::1", 22), "[::1]:22");
        assert_eq!(display_addr("[::1]", 22), "[::1]:22");
        assert_eq!(unbracket("[fe80::1]"), "fe80::1");
        assert_eq!(unbracket("host"), "host");
    }

    #[tokio::test]
    async fn connect_refused_has_clear_message() {
        // A freed ephemeral port; verified to refuse connections because other
        // processes on the machine may grab it in the meantime.
        let mut port = 0;
        for _ in 0..20 {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            let addr: SocketAddr = ([127, 0, 0, 1], p).into();
            if let Err(e) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500))
                && e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                port = p;
                break;
            }
        }
        assert_ne!(port, 0, "no refusing port found");
        let err = tcp_connect("127.0.0.1", port, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Connection);
        assert!(
            err.message.contains(&format!("127.0.0.1:{port}")),
            "{}",
            err.message
        );
    }
}
