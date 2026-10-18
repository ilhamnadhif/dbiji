//! Mapping of ClickHouse exceptions and HTTP/transport failures to [`DbError`].

use dbiji_core::{DbError, ErrorKind};

/// Parsed form of a ClickHouse exception text such as
/// `Code: 62. DB::Exception: Syntax error: ... (SYNTAX_ERROR) (version 24.8.1.1)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exception {
    /// Numeric error code.
    pub code: Option<u32>,
    /// Symbolic error name, e.g. `SYNTAX_ERROR`.
    pub name: Option<String>,
    /// Message without the `Code: N. DB::Exception:` prefix and suffixes.
    pub message: String,
    /// Server version suffix, if present.
    pub version: Option<String>,
}

/// Does `text` look like the start of a ClickHouse exception (`Code: N.`)?
pub fn find_exception_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = text[from..].find("Code: ") {
        let at = from + i;
        let rest = &text[at + 6..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with('.') {
            return Some(at);
        }
        from = at + 6;
    }
    None
}

/// Parse an exception text. Unknown shapes keep the whole text as message.
pub fn parse_exception(text: &str) -> Exception {
    let text = text.trim();
    let mut code = None;
    let mut body = text;
    if let Some(start) = find_exception_start(text) {
        let rest = &text[start + 6..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        code = digits.parse().ok();
        body = rest[digits.len()..].trim_start_matches('.').trim_start();
    }
    if let Some(r) = body.strip_prefix("DB::Exception:") {
        body = r.trim_start();
    } else if let Some(r) = body.strip_prefix("DB::ErrnoException:") {
        body = r.trim_start();
    }
    let mut message = body.to_string();
    let mut version = None;
    // trailing "(version 24.8.1.1 (official build))"
    if let Some(i) = message.rfind("(version ") {
        let v = message[i + 9..].trim_end().trim_end_matches(')').trim().to_string();
        version = Some(v.trim_end_matches(" (official build").to_string());
        message.truncate(i);
        message = message.trim_end().to_string();
    }
    // "(ERROR_NAME)": the last parenthesised upper-case token. Old servers
    // may append further text after it (legacy mid-stream exceptions).
    let mut name = None;
    if let Some((start, end)) = find_error_name(&message) {
        name = Some(message[start + 1..end - 1].to_string());
        let after = message[end..].trim().to_string();
        let mut m = message[..start].trim_end().to_string();
        if !after.is_empty() {
            m.push('\n');
            m.push_str(&after);
        }
        message = m;
    }
    // Messages often end with '.' before the name: keep as is.
    Exception { code, name, message, version }
}

/// Byte range of the last `(UPPER_CASE_NAME)` token in `message`.
fn find_error_name(message: &str) -> Option<(usize, usize)> {
    let bytes = message.as_bytes();
    let mut best = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'(' {
            continue;
        }
        let rest = &message[i + 1..];
        let len = rest.chars().take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_').count();
        if len >= 3 && rest[len..].starts_with(')') && rest[..len].chars().any(|c| c.is_ascii_uppercase()) {
            best = Some((i, i + 1 + len + 1));
        }
    }
    best
}

/// Error kind for a ClickHouse error code.
pub fn kind_for_code(code: u32) -> ErrorKind {
    match code {
        62 => ErrorKind::Syntax,
        192 | 193 | 194 | 516 => ErrorKind::Auth,
        164 => ErrorKind::ReadOnly,
        159 | 209 => ErrorKind::Timeout,
        394 => ErrorKind::Cancelled,
        210 | 279 | 1000 => ErrorKind::Connection,
        115 => ErrorKind::Config,
        _ => ErrorKind::Query,
    }
}

/// Convert an exception text (from a response body) into a [`DbError`].
/// `header_code` is the `X-ClickHouse-Exception-Code` header when present.
pub fn exception_to_error(text: &str, header_code: Option<&str>) -> DbError {
    let ex = parse_exception(text);
    let code = ex.code.or_else(|| header_code.and_then(|c| c.trim().parse().ok()));
    let message = if ex.message.is_empty() { "ClickHouse returned an empty error".to_string() } else { ex.message.clone() };
    let mut err = DbError::new(code.map(kind_for_code).unwrap_or(ErrorKind::Query), message);
    if let Some(c) = code {
        err = err.with_code(match &ex.name {
            Some(n) => format!("{c} {n}"),
            None => c.to_string(),
        });
    }
    if let Some(v) = &ex.version {
        err = err.with_detail(format!("ClickHouse {v}"));
    }
    if let Some(pos) = syntax_position(&ex.message) {
        err = err.with_position(pos);
    }
    match code {
        Some(373) => err.with_hint("Another query is still running in this ClickHouse session"),
        Some(374) => err.with_hint("Lower the `session_timeout` connection property"),
        Some(164) => err.with_hint("The user or connection is read-only"),
        _ => err,
    }
}

/// `... failed at position 12 (...)` → 12.
fn syntax_position(message: &str) -> Option<usize> {
    let i = message.find("at position ")?;
    let digits: String = message[i + 12..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|p| *p > 0)
}

/// Map a transport level failure from `ureq`.
pub fn transport_error(e: ureq::Error, url_for_message: &str) -> DbError {
    match e {
        ureq::Error::Timeout(t) => DbError::timeout(format!("Request to {url_for_message} timed out ({t})")),
        ureq::Error::HostNotFound => {
            DbError::connection(format!("Host not found for {url_for_message}"))
        }
        ureq::Error::ConnectionFailed => DbError::connection(format!("Could not connect to {url_for_message}")),
        ureq::Error::Io(io) => {
            let kind = match io.kind() {
                std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
                _ => ErrorKind::Connection,
            };
            DbError::new(kind, format!("I/O error talking to {url_for_message}: {io}"))
        }
        ureq::Error::Tls(m) => DbError::tls(format!("TLS error: {m}")),
        ureq::Error::Rustls(r) => DbError::tls(format!("TLS error: {r}")),
        ureq::Error::Pem(p) => DbError::config(format!("Invalid PEM data: {p}")),
        ureq::Error::BadUri(u) => DbError::config(format!("Invalid URL: {u}")),
        ureq::Error::Protocol(p) => DbError::connection(format!("HTTP protocol error from {url_for_message}: {p}")),
        ureq::Error::StatusCode(c) => DbError::query(format!("HTTP status {c}")),
        other => DbError::connection(format!("HTTP error talking to {url_for_message}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_exception() {
        let e = parse_exception(
            "Code: 62. DB::Exception: Syntax error: failed at position 1 (SELEC): SELEC 1. Expected one of: Query. (SYNTAX_ERROR) (version 26.9.19.9 (official build))\n",
        );
        assert_eq!(e.code, Some(62));
        assert_eq!(e.name.as_deref(), Some("SYNTAX_ERROR"));
        assert_eq!(e.version.as_deref(), Some("26.9.19.9"));
        assert!(e.message.starts_with("Syntax error: failed at position 1"), "{}", e.message);
        assert!(e.message.ends_with("Expected one of: Query."), "{}", e.message);
    }

    #[test]
    fn to_db_error() {
        let err = exception_to_error(
            "Code: 60. DB::Exception: Unknown table expression identifier 'nope' in scope SELECT * FROM nope. (UNKNOWN_TABLE) (version 24.3.1.1)",
            None,
        );
        assert_eq!(err.kind, ErrorKind::Query);
        assert_eq!(err.code.as_deref(), Some("60 UNKNOWN_TABLE"));
        assert!(err.message.contains("'nope'"));
        let err = exception_to_error("Code: 62. DB::Exception: Syntax error: failed at position 8 ('x'). (SYNTAX_ERROR)", None);
        assert_eq!(err.kind, ErrorKind::Syntax);
        assert_eq!(err.position, Some(8));
        let err = exception_to_error("Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)", None);
        assert_eq!(err.kind, ErrorKind::Cancelled);
        let err = exception_to_error("Code: 516. DB::Exception: x: Authentication failed (AUTHENTICATION_FAILED)", None);
        assert_eq!(err.kind, ErrorKind::Auth);
        let err = exception_to_error("something odd", Some("159"));
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("159"));
        assert_eq!(err.message, "something odd");
    }

    #[test]
    fn finds_exception_inside_line() {
        let line = "[1, \"abcCode: 395. DB::Exception: boom";
        let i = find_exception_start(line).unwrap();
        assert_eq!(&line[i..i + 9], "Code: 395");
        assert!(find_exception_start("Code: x. nothing").is_none());
        assert!(find_exception_start("no code here").is_none());
    }
}
