//! Mapping of driver, server and BSON errors to [`DbError`].

use dbiji_core::{DbError, ErrorKind};
use mongodb::error::{Error as MongoError, ErrorKind as MongoErrorKind};

/// The server version DBiji needs: the `mongodb` crate refuses anything older
/// (`DRIVER_MIN_WIRE_VERSION`, 8), so the message must say which version is
/// which. The crate is pinned to 3.8 for this range — 3.9 raised the minimum to
/// 4.4 without extending the maximum (both accept MongoDB 8.0).
pub const MIN_SERVER_VERSION: &str = "4.2";

/// Explain a wire-version refusal from the driver.
///
/// The driver reports it as a server selection failure whose text is the only
/// place the versions appear: "Server at host:port reports wire version 8, but
/// this version of the MongoDB Rust driver requires at least 9 (MongoDB 4.4)."
fn wire_version_hint(message: &str) -> Option<String> {
    if !message.contains("wire version") {
        return None;
    }
    let reported = message.split("reports wire version ").nth(1).and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next()).filter(|v| !v.is_empty()).map(str::to_string);
    Some(match reported {
        Some(v) => format!(
            "DBiji needs MongoDB {MIN_SERVER_VERSION} or newer; this server reports wire version {v} \
             (MongoDB {} or older). Upgrade the server.",
            wire_version_to_version(&v)
        ),
        None => format!("DBiji needs MongoDB {MIN_SERVER_VERSION} or newer; this server is older."),
    })
}

/// "No server answered at 127.0.0.1:27017: I/O error: Connection refused (os error 61)"
/// from the driver's topology dump, or `None` for another kind of message.
fn server_selection_summary(message: &str) -> Option<String> {
    if !message.contains("No available servers") {
        return None;
    }
    let addresses = extract_all(message, "Address: ", &[',', '}']);
    let errors = extract_all(message, "Error: ", &[',', '}']);
    if addresses.is_empty() && errors.is_empty() {
        return None;
    }
    let mut out = if addresses.is_empty() { "No server answered".to_string() } else { format!("No server answered at {}", addresses.join(", ")) };
    if !errors.is_empty() {
        out.push_str(": ");
        out.push_str(&errors.join("; "));
    }
    Some(out)
}

/// Every `<prefix>value` up to one of `stops`, trimmed.
fn extract_all(message: &str, prefix: &str, stops: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = message;
    while let Some(i) = rest.find(prefix) {
        let after = &rest[i + prefix.len()..];
        let end = after.find(|c: char| stops.contains(&c)).unwrap_or(after.len());
        let value = after[..end].trim().trim_end_matches('}').trim();
        if !value.is_empty() {
            out.push(value.to_string());
        }
        rest = &after[end..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// The MongoDB release a wire version belongs to (the versions the driver
/// refuses, plus the ones it accepts).
fn wire_version_to_version(wire: &str) -> String {
    match wire {
        "0" | "1" | "2" => "3.0".into(),
        "3" => "3.2".into(),
        "4" => "3.4".into(),
        "5" => "3.6".into(),
        "6" => "3.6".into(),
        "7" => "4.0".into(),
        "8" => "4.2".into(),
        "9" => "4.4".into(),
        "13" => "5.0".into(),
        "17" => "6.0".into(),
        "21" => "7.0".into(),
        "25" => "8.0".into(),
        other => format!("(wire version {other})"),
    }
}

/// An authentication failure, with the database the user is checked in.
///
/// The driver's `Display` appends its own debug fields ("Kind: SCRAM failure:
/// Authentication failed., labels: {}, source: Some(Error { … })"), so the
/// inner text is what the user gets to read.
fn auth_failure(message: String) -> DbError {
    DbError::auth(message).with_hint(
        "The server rejected the user name or password. A MongoDB user belongs to one database: \
         DBiji checks the authSource property, else the connection's database, then admin. Check \
         the password, and set authSource to the database where the user was created (`use shop; \
         db.createUser(…)` defines it in shop).",
    )
}

/// Maps an error of the MongoDB driver.
pub fn map_error(e: &MongoError) -> DbError {
    let message = e.to_string();
    match e.kind.as_ref() {
        MongoErrorKind::Authentication { message, .. } => auth_failure(message.clone()),
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
        // The driver wraps the useful text (`message`) and appends its own
        // debug fields to `Display`; use the inner text so the user sees the
        // server's answer and not "Kind: …, labels: {}, source: None".
        MongoErrorKind::ServerSelection { message, .. } => {
            // The driver's text is a topology dump; the first line a user needs
            // is "which address, and why". Keep the dump as the detail.
            let summary = server_selection_summary(message);
            let mut err = DbError::connection(summary.clone().unwrap_or_else(|| message.clone()));
            if summary.is_some() {
                err = err.with_detail(message.clone());
            }
            match wire_version_hint(message) {
                Some(hint) => err.with_hint(hint),
                None if message.contains("No available servers") => err.with_hint(
                    "No server answered: check the host and port, that mongod is running and reachable \
                     (bindIp, firewall), and that the TLS mode matches the server's",
                ),
                None => err,
            }
        }
        MongoErrorKind::IncompatibleServer { message, .. } => {
            let err = DbError::unsupported(message.clone());
            match wire_version_hint(message) {
                Some(hint) => err.with_hint(hint),
                None => err,
            }
        }
        MongoErrorKind::InvalidTlsConfig { message, .. } => DbError::tls(message.clone()),
        MongoErrorKind::SessionsNotSupported => DbError::unsupported("The server does not support sessions"),
        MongoErrorKind::Io(_) | MongoErrorKind::ConnectionPoolCleared { .. } => DbError::connection(message),
        MongoErrorKind::BsonDeserialization(_) | MongoErrorKind::BsonSerialization(_) => DbError::internal(message),
        _ => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("authentication failed") {
                auth_failure(message)
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
    fn an_old_server_names_both_versions() {
        let message = "Server at 127.0.0.1:27017 reports wire version 7, but this version of the \
                       MongoDB Rust driver requires at least 8 (MongoDB 4.2).";
        let hint = wire_version_hint(message).expect("a hint");
        assert!(hint.contains("MongoDB 4.2 or newer"), "{hint}");
        assert!(hint.contains("MongoDB 4.0"), "{hint}");
        assert!(wire_version_hint("connection refused").is_none());
    }

    #[test]
    fn a_connection_refusal_is_summarised() {
        let dump = "Server selection timeout: No available servers. Topology: { Type: Unknown, Servers: [ \
                    { Address: 127.0.0.1:27019, Type: Unknown, Error: Kind: I/O error: Connection refused \
                    (os error 61), labels: {\"RetryableError\"} } ] }";
        let summary = server_selection_summary(dump).expect("a summary");
        assert!(summary.starts_with("No server answered at 127.0.0.1:27019"), "{summary}");
        assert!(summary.contains("Connection refused"), "{summary}");
        assert!(!summary.contains("labels"), "{summary}");
        // Anything else is left alone.
        assert!(server_selection_summary("wire version too old").is_none());
    }

    #[test]
    fn wire_versions_name_their_release() {
        assert_eq!(wire_version_to_version("8"), "4.2");
        assert_eq!(wire_version_to_version("9"), "4.4");
        assert_eq!(wire_version_to_version("17"), "6.0");
        assert_eq!(wire_version_to_version("99"), "(wire version 99)");
        // A refusal without a number still says what is needed.
        let hint = wire_version_hint("wire version too old").expect("a hint");
        assert!(hint.contains("MongoDB 4.2 or newer"), "{hint}");
    }

    #[test]
    fn an_authentication_failure_names_the_database_it_checks() {
        let err = auth_failure("SCRAM failure: Authentication failed.".into());
        assert_eq!(err.kind, ErrorKind::Auth);
        assert_eq!(err.message, "SCRAM failure: Authentication failed.");
        let hint = err.hint.expect("a hint");
        assert!(hint.contains("authSource"), "{hint}");
        assert!(!err.message.contains("labels"), "{}", err.message);
    }

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
