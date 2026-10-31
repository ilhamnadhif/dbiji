//! Mapping of Redis errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use redis::ErrorKind as RedisErrorKind;
use redis::RedisError;

/// Maps an error of the Redis client.
pub fn map_error(e: &RedisError) -> DbError {
    let message = e.to_string();
    let kind = match e.kind() {
        RedisErrorKind::AuthenticationFailed => ErrorKind::Auth,
        RedisErrorKind::InvalidClientConfig => ErrorKind::Config,
        RedisErrorKind::Io | RedisErrorKind::ClusterConnectionNotFound => ErrorKind::Connection,
        RedisErrorKind::RESP3NotSupported => ErrorKind::Unsupported,
        RedisErrorKind::Server(_) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("noauth") || lower.contains("wrongpass") || lower.contains("invalid password") { ErrorKind::Auth } else { ErrorKind::Query }
        }
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
    }
}
