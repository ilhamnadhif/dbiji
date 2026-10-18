//! Unified error type returned by every driver and tool.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Broad error category, used by the UI to decide how to present an error
/// (e.g. offer reconnect for [`ErrorKind::Connection`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// Network / socket level failure, or the connection was lost.
    Connection,
    /// Authentication failed (bad password, missing key ...).
    Auth,
    /// Server rejected a statement.
    Query,
    /// Statement could not be parsed (client or server side).
    Syntax,
    /// Operation exceeded its time limit.
    Timeout,
    /// Operation was cancelled by the user.
    Cancelled,
    /// Write attempted on a read-only connection.
    ReadOnly,
    /// Feature not available for this database / server version.
    Unsupported,
    /// Local I/O failure (files, pipes).
    Io,
    /// Invalid configuration supplied by the user.
    Config,
    /// TLS negotiation or certificate verification failure.
    Tls,
    /// SSH tunnel failure.
    Ssh,
    /// Bug or unexpected state inside DBiji.
    Internal,
}

/// Error returned by drivers and tools. It is cheap to clone so the UI can keep
/// it around for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbError {
    pub kind: ErrorKind,
    pub message: String,
    /// Vendor error code (SQLSTATE for PostgreSQL, numeric code for MySQL ...).
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character position of the error inside the statement text, when the server reports it.
    pub position: Option<usize>,
    /// Extra context, e.g. "PL/pgSQL function f() line 3".
    pub context: Option<String>,
}

impl DbError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), code: None, detail: None, hint: None, position: None, context: None }
    }

    pub fn connection(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Connection, message)
    }
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, message)
    }
    pub fn query(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Query, message)
    }
    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Syntax, message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Operation cancelled")
    }
    pub fn read_only(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ReadOnly, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }
    pub fn tls(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Tls, message)
    }
    pub fn ssh(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Ssh, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_position(mut self, position: usize) -> Self {
        self.position = Some(position);
        self
    }
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// True when the session that produced the error is probably unusable
    /// and should be reconnected.
    pub fn is_connection_lost(&self) -> bool {
        self.kind == ErrorKind::Connection
    }

    /// Multi-line text with every available detail, for error panels and logs.
    pub fn full_text(&self) -> String {
        let mut s = String::new();
        if let Some(code) = &self.code {
            s.push_str(&format!("[{code}] "));
        }
        s.push_str(&self.message);
        if let Some(d) = &self.detail {
            s.push_str("\nDetail: ");
            s.push_str(d);
        }
        if let Some(h) = &self.hint {
            s.push_str("\nHint: ");
            s.push_str(h);
        }
        if let Some(c) = &self.context {
            s.push_str("\nContext: ");
            s.push_str(c);
        }
        if let Some(p) = self.position {
            s.push_str(&format!("\nPosition: {p}"));
        }
        s
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "[{code}] {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let kind = match e.kind() {
            K::ConnectionRefused | K::ConnectionReset | K::ConnectionAborted | K::NotConnected | K::BrokenPipe | K::AddrNotAvailable | K::UnexpectedEof => ErrorKind::Connection,
            K::TimedOut => ErrorKind::Timeout,
            _ => ErrorKind::Io,
        };
        DbError::new(kind, e.to_string())
    }
}

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        DbError::config(format!("JSON error: {e}"))
    }
}

pub type DbResult<T> = Result<T, DbError>;
