//! Minimal JSON reader used for ClickHouse `JSONCompactEachRow*` output.
//!
//! `serde_json` (without the workspace-wide `arbitrary_precision` feature)
//! converts large integers and decimals to `f64`, silently losing precision
//! for `UInt64`, `Int128`, `UInt256` and `Decimal` values. This reader keeps
//! every number as its exact source text, tolerates invalid UTF-8 inside
//! strings (replaced with U+FFFD, as ClickHouse `String` values are arbitrary
//! bytes) and bounds nesting depth so hostile input cannot overflow the stack.

use std::fmt::Write as _;

/// Maximum nesting depth accepted by the parser.
const MAX_DEPTH: usize = 256;

/// A parsed JSON value. Numbers keep their original text.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// A number exactly as written in the input.
    Number(String),
    /// A string with escapes resolved.
    String(String),
    /// A string that is not UTF-8 text: binary data in a `String` column
    /// (`unhex(…)`, digests), kept byte for byte.
    Bytes(Vec<u8>),
    /// An array.
    Array(Vec<Json>),
    /// An object; key order is preserved.
    Object(Vec<(String, Json)>),
}

/// Error produced for malformed JSON input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset where parsing failed.
    pub offset: usize,
    /// Human readable reason.
    pub reason: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.reason)
    }
}

impl std::error::Error for JsonError {}

/// Parse a complete JSON document (surrounding whitespace allowed).
pub fn parse(input: &[u8]) -> Result<Json, JsonError> {
    let mut p = Parser { input, pos: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != input.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

/// Parse one `JSONCompactEachRow` line: a JSON array of column values.
pub fn parse_row(line: &[u8]) -> Result<Vec<Json>, JsonError> {
    match parse(line)? {
        Json::Array(items) => Ok(items),
        _ => Err(JsonError { offset: 0, reason: "expected a JSON array" }),
    }
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, reason: &'static str) -> JsonError {
        JsonError { offset: self.pos, reason }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, lit: &'static [u8], v: Json) -> Result<Json, JsonError> {
        if self.input[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'n') if self.input[self.pos..].starts_with(b"null") => self.expect_literal(b"null", Json::Null),
            Some(b't') => self.expect_literal(b"true", Json::Bool(true)),
            Some(b'f') => self.expect_literal(b"false", Json::Bool(false)),
            Some(b'"') => self.string_bytes().map(|b| match String::from_utf8(b) {
                Ok(s) => Json::String(s),
                Err(e) => Json::Bytes(e.into_bytes()),
            }),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            // Numbers; ClickHouse may also print special floats (`nan`,
            // `inf`, `-inf`) unquoted in some modes.
            Some(b'-' | b'+' | b'0'..=b'9' | b'n' | b'i') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("invalid number"));
        }
        let text = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        Ok(Json::Number(text))
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // {
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':'"));
            }
            self.pos += 1;
            self.skip_ws();
            let v = self.value(depth + 1)?;
            fields.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let end = self.pos + 4;
        if end > self.input.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let mut v = 0u32;
        for &b in &self.input[self.pos..end] {
            let d = (b as char).to_digit(16).ok_or_else(|| self.err("invalid \\u escape"))?;
            v = v * 16 + d;
        }
        self.pos = end;
        Ok(v)
    }

    /// Parse a string (an object key) starting at the opening quote; invalid
    /// UTF-8 is replaced so it never aborts a row.
    fn string(&mut self) -> Result<String, JsonError> {
        self.string_bytes().map(|b| match String::from_utf8(b) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    /// Bytes of a string starting at the opening quote, escapes resolved.
    fn string_bytes(&mut self) -> Result<Vec<u8>, JsonError> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(e) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.pos += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'0' => out.push(0),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                // surrogate pair
                                if self.input[self.pos..].starts_with(b"\\u") {
                                    let save = self.pos;
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                    } else {
                                        self.pos = save;
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                char::from_u32(hi)
                            };
                            let ch = ch.unwrap_or('\u{FFFD}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => {
                            // Unknown escape: keep it verbatim.
                            out.push(b'\\');
                            out.push(other);
                        }
                    }
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }
}

impl Json {
    /// Compact JSON serialisation (numbers are emitted verbatim).
    pub fn to_json_string(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s);
        s
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => {
                // Special floats are not valid JSON numbers; quote them.
                if n.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
                    write_json_string(out, n);
                } else {
                    out.push_str(n);
                }
            }
            Json::String(s) => write_json_string(out, s),
            Json::Bytes(b) => write_json_string(out, &String::from_utf8_lossy(b)),
            Json::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_to(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, k);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }

    /// Text of a scalar (strings unquoted, numbers verbatim); `None` for
    /// containers and null.
    pub fn scalar_text(&self) -> Option<&str> {
        match self {
            Json::String(s) | Json::Number(s) => Some(s),
            Json::Bool(true) => Some("true"),
            Json::Bool(false) => Some("false"),
            _ => None,
        }
    }
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_row_with_exact_numbers() {
        let row = parse_row(br#"["18446744073709551615", 340282366920938463463374607431768211455, -1.5e3, true, null]"#).unwrap();
        assert_eq!(row[0], Json::String("18446744073709551615".into()));
        assert_eq!(row[1], Json::Number("340282366920938463463374607431768211455".into()));
        assert_eq!(row[2], Json::Number("-1.5e3".into()));
        assert_eq!(row[3], Json::Bool(true));
        assert_eq!(row[4], Json::Null);
    }

    #[test]
    fn parses_nested_and_escapes() {
        let bs = '\\';
        let input = format!(r#"{{"a":[1,{{"b":"x\"y\\z\/{bs}u00e9{bs}ud83d{bs}ude00"}}],"c":{{}}}}"#);
        let v = parse(input.as_bytes()).unwrap();
        let Json::Object(f) = &v else { panic!("object") };
        assert_eq!(f[0].0, "a");
        let Json::Array(items) = &f[0].1 else { panic!("array") };
        let Json::Object(inner) = &items[1] else { panic!("inner") };
        assert_eq!(inner[0].1, Json::String("x\"y\\z/é😀".into()));
        assert_eq!(v.to_json_string(), r#"{"a":[1,{"b":"x\"y\\z/é😀"}],"c":{}}"#);
    }

    #[test]
    fn invalid_utf8_is_kept_as_bytes() {
        let mut line = b"[\"a".to_vec();
        line.push(0xff);
        line.extend_from_slice(b"b\"]");
        let row = parse_row(&line).unwrap();
        assert_eq!(row[0], Json::Bytes(vec![b'a', 0xff, b'b']));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_row(b"[1, 2").is_err());
        assert!(parse_row(b"{\"a\":1}").is_err());
        assert!(parse(b"[1] x").is_err());
        assert!(parse(b"").is_err());
        let deep = "[".repeat(1000);
        assert!(parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn lone_surrogate_becomes_replacement() {
        let v = parse(br#""\ud800x""#).unwrap();
        assert_eq!(v, Json::String("\u{FFFD}x".into()));
    }

    #[test]
    fn special_numbers_are_quoted_on_output() {
        assert_eq!(Json::Number("nan".into()).to_json_string(), "\"nan\"");
        assert_eq!(Json::Number("1e5".into()).to_json_string(), "1e5");
    }
}
