//! Client side commands (FR-S8), DBeaver compatible:
//!
//! * `@set var = value` — define a variable used by `${var}` and `:var`
//! * `@include file.sql` — execute another script
//! * `@export {"type":"csv", ...}` or `@export format=csv file=/tmp/x.csv`
//!   — export the result of the next query
//! * `@echo text` — print a message
//!
//! Any other `@name args` line is returned as [`ClientCommand::Other`].

use dbiji_core::{tr, trf};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A parsed client command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientCommand {
    /// `@set name = value`.
    Set {
        /// Variable name.
        name: String,
        /// SQL text, used verbatim when the variable is substituted.
        value: String,
    },
    /// `@include path`.
    Include {
        /// Script path (quotes removed).
        path: String,
    },
    /// `@export ...`.
    Export {
        /// Export options. JSON objects are flattened: nested keys are joined
        /// with `.` (`producer.selectedRowsOnly`), strings are unquoted,
        /// numbers/booleans rendered as text, arrays kept as JSON text.
        options: BTreeMap<String, String>,
    },
    /// `@echo text`
    Echo(String),
    /// Any other command, e.g. `@exit`.
    Other {
        /// Command name without `@`.
        name: String,
        /// Rest of the line.
        args: String,
    },
}

/// Parse `stmt` as a client command. `None` when it is not one (does not
/// start with `@` followed by a letter); `Some(Err(message))` when it is a
/// known command with invalid arguments.
pub fn parse_client_command(stmt: &str) -> Option<Result<ClientCommand, String>> {
    let s = stmt.trim();
    let rest = s.strip_prefix('@')?;
    if !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..name_len];
    let args = rest[name_len..].trim();
    Some(match name.to_ascii_lowercase().as_str() {
        "set" => parse_set(args),
        "include" => {
            let path = unquote(strip_semicolon(args));
            if path.is_empty() {
                Err(tr("Missing file name after @include").to_string())
            } else {
                Ok(ClientCommand::Include { path })
            }
        }
        "export" => parse_export(args).map(|options| ClientCommand::Export { options }),
        "echo" => Ok(ClientCommand::Echo(strip_semicolon(args).to_string())),
        _ => Ok(ClientCommand::Other {
            name: name.to_string(),
            args: strip_semicolon(args).to_string(),
        }),
    })
}

fn strip_semicolon(s: &str) -> &str {
    let t = s.trim_end();
    t.strip_suffix(';').map(str::trim_end).unwrap_or(t)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

fn parse_set(args: &str) -> Result<ClientCommand, String> {
    let args = strip_semicolon(args);
    if args.is_empty() {
        return Err(tr("Missing variable name after @set").to_string());
    }
    let Some(eq) = args.find('=') else {
        return Err(trf(
            "Expected '=' after the variable name in '@set {0}'",
            &[&args],
        ));
    };
    let mut name = args[..eq].trim();
    if let Some(n) = name.strip_suffix(':') {
        name = n.trim_end();
    }
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !valid {
        return Err(trf("Invalid variable name: '{0}'", &[&name]));
    }
    Ok(ClientCommand::Set {
        name: name.to_string(),
        value: args[eq + 1..].trim().to_string(),
    })
}

fn flatten(prefix: &str, v: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    use serde_json::Value as J;
    match v {
        J::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        J::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        J::Null => {
            out.insert(prefix.to_string(), String::new());
        }
        J::Bool(_) | J::Number(_) | J::Array(_) => {
            out.insert(prefix.to_string(), v.to_string());
        }
    }
}

fn parse_export(args: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    if args.starts_with('{') {
        let json = strip_semicolon(args);
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| trf("Invalid @export options: {0}", &[&e]))?;
        if !v.is_object() {
            return Err(tr("@export options must be a JSON object").to_string());
        }
        flatten("", &v, &mut out);
        return Ok(out);
    }
    // key=value pairs; a trailing `;` is a statement delimiter unless it is
    // the value of the last pair (`delimiter=;`).
    let mut text = args.trim_end();
    if text.ends_with(';') && !text.ends_with("=;") {
        text = text[..text.len() - 1].trim_end();
    }
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let key_start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &text[key_start..i];
        if i >= b.len() || b[i] != b'=' || key.is_empty() {
            return Err(trf("Expected key=value in @export, found '{0}'", &[&key]));
        }
        i += 1;
        let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
            let q = b[i];
            let vstart = i + 1;
            let mut j = vstart;
            while j < b.len() && b[j] != q {
                j += 1;
            }
            if j >= b.len() {
                return Err(trf(
                    "Unterminated quoted value for '{0}' in @export",
                    &[&key],
                ));
            }
            i = j + 1;
            &text[vstart..j]
        } else {
            let vstart = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            &text[vstart..i]
        };
        out.insert(key.to_string(), value.to_string());
    }
    if out.is_empty() {
        return Err(tr("Missing @export options").to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn ok(s: &str) -> ClientCommand {
        parse_client_command(s).expect("command").expect("valid")
    }

    #[test]
    fn not_commands() {
        assert!(parse_client_command("SELECT 1").is_none());
        assert!(parse_client_command("@").is_none());
        assert!(parse_client_command("@@version").is_none());
        assert!(parse_client_command("@1").is_none());
        assert!(parse_client_command("").is_none());
    }

    #[test]
    fn set() {
        assert_eq!(
            ok("@set x = 1"),
            ClientCommand::Set {
                name: "x".into(),
                value: "1".into()
            }
        );
        assert_eq!(
            ok("  @SET  name='a = b';  "),
            ClientCommand::Set {
                name: "name".into(),
                value: "'a = b'".into()
            }
        );
        assert_eq!(
            ok("@set v := now()"),
            ClientCommand::Set {
                name: "v".into(),
                value: "now()".into()
            }
        );
        assert_eq!(
            ok("@set empty ="),
            ClientCommand::Set {
                name: "empty".into(),
                value: "".into()
            }
        );
        assert!(parse_client_command("@set").unwrap().is_err());
        assert!(parse_client_command("@set novalue").unwrap().is_err());
        assert!(parse_client_command("@set 1x = 2").unwrap().is_err());
        assert!(parse_client_command("@set a b = 2").unwrap().is_err());
    }

    #[test]
    fn include_echo_other() {
        assert_eq!(
            ok("@include scripts/init.sql"),
            ClientCommand::Include {
                path: "scripts/init.sql".into()
            }
        );
        assert_eq!(
            ok("@include \"my file.sql\";"),
            ClientCommand::Include {
                path: "my file.sql".into()
            }
        );
        assert!(parse_client_command("@include").unwrap().is_err());
        assert_eq!(
            ok("@echo Hello, world"),
            ClientCommand::Echo("Hello, world".into())
        );
        assert_eq!(ok("@echo"), ClientCommand::Echo(String::new()));
        assert_eq!(
            ok("@exit 1"),
            ClientCommand::Other {
                name: "exit".into(),
                args: "1".into()
            }
        );
    }

    #[test]
    fn export_json() {
        let c = ok(
            r#"@export {"type": "csv", "producer": {"selectedRowsOnly": false, "x": {"y": 2}}, "consumer": {"outputFolder": "C:\\tmp", "list": [1, 2]}, "n": null}"#,
        );
        let ClientCommand::Export { options } = c else {
            panic!("not export")
        };
        assert_eq!(options.get("type").map(String::as_str), Some("csv"));
        assert_eq!(
            options.get("producer.selectedRowsOnly").map(String::as_str),
            Some("false")
        );
        assert_eq!(options.get("producer.x.y").map(String::as_str), Some("2"));
        assert_eq!(
            options.get("consumer.outputFolder").map(String::as_str),
            Some("C:\\tmp")
        );
        assert_eq!(
            options.get("consumer.list").map(String::as_str),
            Some("[1,2]")
        );
        assert_eq!(options.get("n").map(String::as_str), Some(""));
        assert!(parse_client_command("@export {bad json}").unwrap().is_err());
        assert!(parse_client_command("@export [1]").unwrap().is_err());
        assert!(parse_client_command("@export {\"a\": 1};").unwrap().is_ok());
    }

    #[test]
    fn export_key_values() {
        let c = ok("@export format=csv file=/tmp/x.csv delimiter=;");
        let ClientCommand::Export { options } = c else {
            panic!("not export")
        };
        assert_eq!(options.get("format").map(String::as_str), Some("csv"));
        assert_eq!(options.get("file").map(String::as_str), Some("/tmp/x.csv"));
        assert_eq!(options.get("delimiter").map(String::as_str), Some(";"));
        let c = ok("@export file=\"/tmp/my file.csv\" header=true;");
        let ClientCommand::Export { options } = c else {
            panic!("not export")
        };
        assert_eq!(
            options.get("file").map(String::as_str),
            Some("/tmp/my file.csv")
        );
        assert_eq!(options.get("header").map(String::as_str), Some("true"));
        assert!(parse_client_command("@export").unwrap().is_err());
        assert!(parse_client_command("@export justaword").unwrap().is_err());
        assert!(parse_client_command("@export a=\"open").unwrap().is_err());
    }

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = Rng::new(3);
        for _ in 0..2000 {
            let mut s = String::from(*rng.pick(&[
                "@set ",
                "@export ",
                "@include ",
                "@echo ",
                "@x ",
                "@export {",
            ]));
            s.push_str(&random_sql(&mut rng, 6));
            let _ = parse_client_command(&s);
        }
    }
}
