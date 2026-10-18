//! Network plumbing for DBiji (FR-C3): SSH tunnels, SOCKS5 / HTTP CONNECT
//! proxies, and local shell command helpers.
//!
//! # Tunnels
//!
//! Database drivers always connect to a host and a port, so every tunnel or
//! proxy is exposed as a local TCP listener on `127.0.0.1` that forwards each
//! accepted connection through a chain of [`Hop`]s to the database:
//!
//! ```no_run
//! # async fn demo(ssh: dbiji_core::config::SshConfig, pw: dbiji_core::Secret) -> dbiji_core::DbResult<()> {
//! use dbiji_net::{open_tunnel, TunnelSpec};
//! let spec = TunnelSpec::new("db.internal", 5432, "/path/to/dbiji/known_hosts").with_ssh(ssh, Some(pw), None);
//! let tunnel = open_tunnel(spec).await?;
//! // Connect the driver to 127.0.0.1:<tunnel.local_port()>, keeping the real
//! // host name for TLS verification (`ConnectParams::tls_server_name`).
//! let port = tunnel.local_port();
//! # let _ = port;
//! tunnel.close().await;
//! # Ok(()) }
//! ```
//!
//! SSH host keys are verified against DBiji's own known_hosts file and the
//! user's `~/.ssh/known_hosts` according to
//! [`HostKeyPolicy`](dbiji_core::config::HostKeyPolicy).
//!
//! # Secrets
//!
//! Passwords and passphrases are [`dbiji_core::Secret`]s; they are exposed
//! only when handed to the SSH library or written to a proxy socket, never
//! logged or included in errors.

// `DbError` (the shared error type of dbiji-core) is ~144 bytes; every
// fallible function returns it, so the lint is not actionable in this crate.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

pub mod http_proxy;
pub mod known_hosts;
pub mod process;
pub mod socks;
mod ssh;
pub mod stream;
pub mod tunnel;

pub use known_hosts::{HostKeyStatus, KnownHosts};
pub use process::{password_from_command, run_shell_command};
pub use tunnel::{Hop, Tunnel, TunnelSpec, TunnelStats, needs_tunnel, open_tunnel};
