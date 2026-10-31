//! Parser for the JavaScript literals of the MongoDB shell: documents written
//! as `{key: value}`, arrays, single or double quoted strings, numbers and the
//! shell constructors (`ObjectId("…")`, `ISODate("…")`, `NumberLong(1)` …).
//!
//! The parser is total: invalid text produces an error with the byte offset,
//! never a panic. It does not run code — only literals and the constructors
//! below are understood.

use mongodb::bson::{Binary, Bson, Decimal128, Document, Regex, Timestamp, oid::ObjectId, spec::BinarySubtype};
use std::str::FromStr;

/// A parse failure with the byte offset it was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl ParseError {
    fn new(message: impl Into<String>, offset: usize) -> Self {
        ParseError { message: message.into(), offset }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

/// Parse a shell value (`{…}`, `[…]`, `"text"`, `42`, `true` …).
pub fn parse_value(text: &str) -> Result<Bson, ParseError> {
    let mut p = Parser { src: text, b: text.as_bytes(), pos: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos < p.b.len() {
        return Err(ParseError::new(format!("unexpected '{}' after the value", text[p.pos..].chars().take(12).collect::<String>()), p.pos));
    }
    Ok(v)
}

/// Parse a document; an empty text is the empty document.
pub fn parse_document(text: &str) -> Result<Document, ParseError> {
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    match parse_value(text)? {
        Bson::Document(d) => Ok(d),
        other => Err(ParseError::new(format!("expected a document, found {}", type_name(&other)), 0)),
    }
}

/// Parse an array literal; `[]` and empty text give an empty array.
pub fn parse_array(text: &str) -> Result<Vec<Bson>, ParseError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    match parse_value(text)? {
        Bson::Array(a) => Ok(a),
        other => Err(ParseError::new(format!("expected an array, found {}", type_name(&other)), 0)),
    }
}

fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "a number",
        Bson::String(_) => "a string",
        Bson::Array(_) => "an array",
        Bson::Document(_) => "a document",
        Bson::Boolean(_) => "a boolean",
        Bson::Null => "null",
        _ => "a value",
    }
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'/' && self.b.get(self.pos + 1) == Some(&b'/') {
                while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else if c == b'/' && self.b.get(self.pos + 1) == Some(&b'*') {
                self.pos += 2;
                while self.pos + 1 < self.b.len() && !(self.b[self.pos] == b'*' && self.b[self.pos + 1] == b'/') {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.b.len());
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn value(&mut self, depth: u32) -> Result<Bson, ParseError> {
        if depth > 64 {
            return Err(ParseError::new("the value is nested too deeply", self.pos));
        }
        self.skip_ws();
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Err(ParseError::new("the value is missing", start));
        };
        match c {
            b'{' => Ok(Bson::Document(self.object(depth)?)),
            b'[' => Ok(Bson::Array(self.array(depth)?)),
            b'"' | b'\'' => Ok(Bson::String(self.string()?)),
            b'-' | b'+' | b'0'..=b'9' | b'.' => self.number(),
            b't' | b'f' | b'n' | b'u' => self.word_value(),
            b'/' => self.regex(),
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => self.constructor_or_word(depth),
            _ => Err(ParseError::new(format!("unexpected '{}'", c as char), start)),
        }
    }

    fn object(&mut self, depth: u32) -> Result<Document, ParseError> {
        let open = self.pos;
        self.pos += 1; // {
        let mut doc = Document::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(doc);
                }
                None => {
                    return Err(ParseError::new("the document is not closed: a '}' is missing", open));
                }
                _ => {}
            }
            let key = match self.peek() {
                Some(b'"') | Some(b'\'') => self.string()?,
                Some(_) => self.identifier()?,
                None => return Err(ParseError::new("the key is missing", self.pos)),
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(ParseError::new(format!("':' is missing after the key '{key}'"), self.pos));
            }
            self.pos += 1;
            let v = self.value(depth + 1)?;
            doc.insert(key, v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {}
                Some(_) => return Err(ParseError::new("',' or '}' is missing", self.pos)),
                None => {
                    return Err(ParseError::new("the document is not closed: a '}' is missing", open));
                }
            }
        }
    }

    fn array(&mut self, depth: u32) -> Result<Vec<Bson>, ParseError> {
        let open = self.pos;
        self.pos += 1; // [
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                None => {
                    return Err(ParseError::new("the array is not closed: a ']' is missing", open));
                }
                _ => {}
            }
            out.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {}
                Some(_) => return Err(ParseError::new("',' or ']' is missing", self.pos)),
                None => {
                    return Err(ParseError::new("the array is not closed: a ']' is missing", open));
                }
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let quote = self.b[self.pos];
        let open = self.pos;
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if c == quote {
                self.pos += 1;
                return Ok(out);
            }
            if c == b'\\' {
                self.pos += 1;
                let Some(e) = self.peek() else { break };
                self.pos += 1;
                match e {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'0' => out.push('\0'),
                    b'u' => {
                        let hex: String = self.src.get(self.pos..self.pos + 4).unwrap_or("").to_string();
                        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            Some(ch) => {
                                out.push(ch);
                                self.pos += 4;
                            }
                            None => out.push('u'),
                        }
                    }
                    other => out.push(other as char),
                }
                continue;
            }
            // Copy the whole character (multi-byte UTF-8).
            let start = self.pos;
            self.pos += 1;
            while self.pos < self.b.len() && (self.b[self.pos] & 0xC0) == 0x80 {
                self.pos += 1;
            }
            out.push_str(self.src.get(start..self.pos).unwrap_or(""));
        }
        Err(ParseError::new("the string is not closed: a quote is missing", open))
    }

    fn identifier(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'-' || c >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(ParseError::new("a key is expected", start));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn number(&mut self) -> Result<Bson, ParseError> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-') | Some(b'+')) {
            self.pos += 1;
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' => {
                    float = true;
                    self.pos += 1;
                }
                b'+' | b'-' if float => self.pos += 1,
                _ => break,
            }
        }
        let text = &self.src[start..self.pos];
        if float {
            text.parse::<f64>().map(Bson::Double).map_err(|_| ParseError::new(format!("'{text}' is not a number"), start))
        } else {
            match text.parse::<i64>() {
                Ok(i) if i >= i32::MIN as i64 && i <= i32::MAX as i64 => Ok(Bson::Int32(i as i32)),
                Ok(i) => Ok(Bson::Int64(i)),
                Err(_) => text.parse::<f64>().map(Bson::Double).map_err(|_| ParseError::new(format!("'{text}' is not a number"), start)),
            }
        }
    }

    fn regex(&mut self) -> Result<Bson, ParseError> {
        let open = self.pos;
        self.pos += 1;
        let mut pattern = String::new();
        while let Some(c) = self.peek() {
            if c == b'\\' {
                pattern.push(c as char);
                self.pos += 1;
                if let Some(n) = self.peek() {
                    pattern.push(n as char);
                    self.pos += 1;
                }
                continue;
            }
            if c == b'/' {
                self.pos += 1;
                let start = self.pos;
                while let Some(o) = self.peek() {
                    if o.is_ascii_alphabetic() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                return Ok(Bson::RegularExpression(regex(&pattern, &self.src[start..self.pos])));
            }
            pattern.push(c as char);
            self.pos += 1;
        }
        Err(ParseError::new("the regular expression is not closed", open))
    }

    fn word_value(&mut self) -> Result<Bson, ParseError> {
        let start = self.pos;
        let word = self.identifier()?;
        match word.as_str() {
            "true" => Ok(Bson::Boolean(true)),
            "false" => Ok(Bson::Boolean(false)),
            "null" | "undefined" => Ok(Bson::Null),
            other => Err(ParseError::new(format!("'{other}' is not a value the shell accepts here"), start)),
        }
    }

    /// `ObjectId("…")`, `ISODate("…")`, `NumberLong(1)` … or a bare word.
    fn constructor_or_word(&mut self, depth: u32) -> Result<Bson, ParseError> {
        let start = self.pos;
        let mut name = self.identifier()?;
        // `new Date(...)`
        if name == "new" {
            self.skip_ws();
            name = self.identifier()?;
        }
        self.skip_ws();
        if self.peek() != Some(b'(') {
            return Err(ParseError::new(format!("'{name}' is not a value the shell accepts here"), start));
        }
        self.pos += 1;
        let mut args = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                None => {
                    return Err(ParseError::new(format!("')' is missing after {name}("), start));
                }
                _ => {}
            }
            args.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b')') => {}
                _ => {
                    return Err(ParseError::new(format!("')' is missing after {name}("), self.pos));
                }
            }
        }
        construct(&name, &args).ok_or_else(|| ParseError::new(format!("{name}(…) is not supported"), start))
    }
}

/// A regular expression value.
fn regex(pattern: &str, options: &str) -> Regex {
    Regex { pattern: pattern.to_string(), options: options.to_string() }
}

fn arg_text(args: &[Bson], i: usize) -> Option<String> {
    match args.get(i)? {
        Bson::String(s) => Some(s.clone()),
        Bson::Int32(i) => Some(i.to_string()),
        Bson::Int64(i) => Some(i.to_string()),
        Bson::Double(d) => Some(d.to_string()),
        _ => None,
    }
}

/// Turn a shell constructor into the BSON value it stands for.
fn construct(name: &str, args: &[Bson]) -> Option<Bson> {
    match name {
        "ObjectId" => {
            if args.is_empty() {
                return Some(Bson::ObjectId(ObjectId::new()));
            }
            ObjectId::parse_str(arg_text(args, 0)?).ok().map(Bson::ObjectId)
        }
        "ISODate" | "Date" => {
            let text = arg_text(args, 0)?;
            chrono::DateTime::parse_from_rfc3339(&text).ok().map(|d| Bson::DateTime(mongodb::bson::DateTime::from_millis(d.timestamp_millis())))
        }
        "NumberLong" => arg_text(args, 0)?.parse::<i64>().ok().map(Bson::Int64),
        "NumberInt" => arg_text(args, 0)?.parse::<i32>().ok().map(Bson::Int32),
        "NumberDecimal" => Decimal128::from_str(&arg_text(args, 0)?).ok().map(Bson::Decimal128),
        "Timestamp" => {
            let t = args.first().and_then(|a| match a {
                Bson::Int64(i) => Some(*i as u64),
                Bson::Int32(i) => Some(*i as u64),
                _ => None,
            })?;
            Some(Bson::Timestamp(Timestamp { time: (t >> 32) as u32, increment: (t & 0xFFFF_FFFF) as u32 }))
        }
        "BinData" => {
            let subtype = match args.first() {
                Some(Bson::Int32(n)) => *n as u8,
                Some(Bson::Int64(n)) => *n as u8,
                _ => return None,
            };
            let data = arg_text(args, 1)?;
            let bytes: Vec<u8> = data.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=').collect::<String>().into_bytes();
            Some(Bson::Binary(Binary { subtype: BinarySubtype::from(subtype), bytes }))
        }
        "UUID" => {
            let text = arg_text(args, 0)?.replace('-', "");
            if text.len() != 32 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let bytes = (0..16).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect();
            Some(Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes }))
        }
        "RegExp" => Some(Bson::RegularExpression(Regex { pattern: arg_text(args, 0)?, options: arg_text(args, 1).unwrap_or_default() })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_with_unquoted_keys_and_single_quotes() {
        let d = parse_document("{name: 'Ann', age: 31, tags: ['a', 'b'], ok: true, meta: {x: 1.5}}").unwrap();
        assert_eq!(d.get_str("name").unwrap(), "Ann");
        assert_eq!(d.get_i32("age").unwrap(), 31);
        assert_eq!(d.get_array("tags").unwrap().len(), 2);
        assert!(d.get_bool("ok").unwrap());
        assert_eq!(d.get_document("meta").unwrap().get_f64("x").unwrap(), 1.5);
    }

    #[test]
    fn constructors() {
        let d = parse_document(r#"{_id: ObjectId("507f1f77bcf86cd799439011"), at: ISODate("2024-01-02T03:04:05Z"), n: NumberLong(9)}"#).unwrap();
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(d.get("at"), Some(Bson::DateTime(_))));
        assert_eq!(d.get_i64("n").unwrap(), 9);
    }

    #[test]
    fn nested_objects_and_trailing_commas() {
        let d = parse_document("{a: {b: [1, {c: 2},]}, }").unwrap();
        assert_eq!(d.get_document("a").unwrap().get_array("b").unwrap().len(), 2);
    }

    #[test]
    fn regex_and_errors() {
        let v = parse_value("/^ann/i").unwrap();
        assert!(matches!(v, Bson::RegularExpression(r) if r.pattern == "^ann" && r.options == "i"));
        assert!(parse_document("{a: }").is_err());
        assert!(parse_document("{a: 1").is_err());
        assert!(parse_document("{a: eval('x')}").is_err());
    }

    #[test]
    fn empty_text_is_an_empty_document() {
        assert!(parse_document("").unwrap().is_empty());
        assert!(parse_array(" ").unwrap().is_empty());
    }
}
