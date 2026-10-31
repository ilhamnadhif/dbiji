//! Mapping of driver, server and BSON errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use mongodb::error::{Error as MongoError, ErrorKind as MongoErrorKind};

/// Maps an error of the MongoDB driver.
pub fn map_error(e: &MongoError) -> DbError {
    let message = e.to_string();
    match e.kind.as_ref() {
        MongoErrorKind::Authentication { .. } => DbError::auth(message),
        MongoErrorKind::InvalidArgument { .. } => DbError::config(message),
        MongoErrorKind::Command(c) => {
            let kind = match c.code {
                13 | 18 => ErrorKind::Auth,
                50 => ErrorKind::Timeout,
                11600 | 11602 | 189 | 91 => ErrorKind::Connection,
                _ => ErrorKind::Query,
            };
            DbError::new(kind, c.message.clone()).with_code(c.code.to_string())
        }
        MongoErrorKind::DnsResolve { .. } => DbError::connection(message),
        MongoErrorKind::Io(_) | MongoErrorKind::ConnectionPoolCleared { .. } | MongoErrorKind::ServerSelection { .. } => DbError::connection(message),
        MongoErrorKind::BsonDeserialization(_) | MongoErrorKind::BsonSerialization(_) => DbError::internal(message),
        _ => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("authentication failed") {
                DbError::auth(message)
            } else if lower.contains("timed out") || lower.contains("timeout") {
                DbError::timeout(message)
            } else {
                DbError::query(message)
            }
        }
    }
}

/// Maps an error of the driver, keeping extra context.
pub fn map_error_with(e: &MongoError, context: &str) -> DbError {
    map_error(e).with_context(context.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_codes_and_io_errors_are_classified() {
        let command: mongodb::error::CommandError = mongodb::bson::from_document(mongodb::bson::doc! {
            "code": 50,
            "codeName": "MaxTimeMSExpired",
            "errmsg": "operation exceeded time limit",
        })
        .expect("a command error");
        let e = MongoError::from(mongodb::error::ErrorKind::Command(command));
        let mapped = map_error(&e);
        assert_eq!(mapped.kind, ErrorKind::Timeout);
        assert_eq!(mapped.code.as_deref(), Some("50"));

        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let e = MongoError::from(mongodb::error::ErrorKind::Io(std::sync::Arc::new(io)));
        assert_eq!(map_error(&e).kind, ErrorKind::Connection);
    }
}
