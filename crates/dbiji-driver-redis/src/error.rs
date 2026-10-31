//! Mapping of Redis errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use redis::ErrorKind as RedisErrorKind;
use redis::RedisError;

/// Maps an error of the Redis client.
pub fn map_error(e: &RedisError) -> DbError {
    let message = e.to_string();
    // `NOAUTH` / `WRONGPASS` are not known server codes to redis-rs, so they
    // arrive as `Extension`; without this check an auth failure would be
    // reported as a query error.
    let lower = message.to_ascii_lowercase();
    let auth = e.code().is_some_and(|c| c.eq_ignore_ascii_case("NOAUTH") || c.eq_ignore_ascii_case("WRONGPASS"))
        || lower.contains("noauth")
        || lower.contains("wrongpass")
        || lower.contains("invalid password");
    let kind = match e.kind() {
        RedisErrorKind::AuthenticationFailed => ErrorKind::Auth,
        RedisErrorKind::InvalidClientConfig => ErrorKind::Config,
        RedisErrorKind::Io | RedisErrorKind::ClusterConnectionNotFound => ErrorKind::Connection,
        RedisErrorKind::RESP3NotSupported => ErrorKind::Unsupported,
        RedisErrorKind::Server(_) | RedisErrorKind::Extension if auth => ErrorKind::Auth,
        RedisErrorKind::Server(_) | RedisErrorKind::Extension => ErrorKind::Query,
        RedisErrorKind::Parse | RedisErrorKind::UnexpectedReturnType => ErrorKind::Internal,
        _ => ErrorKind::Query,
    };
    let mut out = DbError::new(kind, message);
    if let Some(code) = e.code() {
        out = out.with_code(code.to_string());
    }
    if let Some(detail) = e.detail() {
        out = out.with_detail(detail.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_errors_are_queries_and_auth_failures_are_auth() {
        let e = RedisError::from((RedisErrorKind::Server(redis::ServerErrorKind::ResponseError), "ERR unknown command", String::from("x")));
        assert_eq!(map_error(&e).kind, ErrorKind::Query);
        let e = RedisError::from((RedisErrorKind::AuthenticationFailed, "wrong password"));
        assert_eq!(map_error(&e).kind, ErrorKind::Auth);
        let e = redis::make_extension_error("NOAUTH".into(), Some("Authentication required.".into()));
        assert_eq!(map_error(&e).kind, ErrorKind::Auth);
        let e = redis::make_extension_error("WRONGPASS".into(), None);
        assert_eq!(map_error(&e).kind, ErrorKind::Auth);
    }
}
