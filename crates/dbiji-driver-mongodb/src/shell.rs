//! A subset of the `mongosh` language: `db.<collection>.<method>(…)…`
//! chains, `db.<method>(…)`, `show dbs|collections`, `use <db>`, with
//! JavaScript object literals as arguments (unquoted keys, `'…'` strings,
//! regular expressions, `ObjectId(…)`, `ISODate(…)`, `NumberLong(…)` …).

use mongodb::bson::{self, Bson, Document, oid::ObjectId};

#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Vec<Bson>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `db[.getSiblingDB(d)][.<collection>|.getCollection(c)].<calls>`
    Chain {
        db: Option<String>,
        collection: Option<String>,
        calls: Vec<Call>,
    },
    ShowDbs,
    ShowCollections,
    Use(String),
}

struct P<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

type R<T> = Result<T, String>;

impl<'a> P<'a> {
    fn ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.src[self.i..].starts_with("//") {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
            } else if self.src[self.i..].starts_with("/*") {
                match self.src[self.i + 2..].find("*/") {
                    Some(e) => self.i += e + 4,
                    None => self.i = self.s.len(),
                }
            } else {
                break;
            }
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> R<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", c as char)))
        }
    }
    fn error(&self, what: &str) -> String {
        let near: String = self.src[self.i.min(self.src.len())..]
            .chars()
            .take(20)
            .collect();
        if near.is_empty() {
            format!("{what} at the end")
        } else {
            format!("{what} near \"{near}\"")
        }
    }
    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric()
                || self.s[self.i] == b'_'
                || self.s[self.i] == b'$')
        {
            self.i += 1;
        }
        (self.i > start && !self.s[start].is_ascii_digit())
            .then(|| self.src[start..self.i].to_string())
            .or_else(|| {
                self.i = start;
                None
            })
    }
    fn string(&mut self) -> R<String> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.src[self.i..].chars().next() else {
                return Err("unterminated string".into());
            };
            self.i += c.len_utf8();
            match c {
                c if c as u32 == q as u32 => return Ok(out),
                '\\' => {
                    let Some(e) = self.src[self.i..].chars().next() else {
                        return Err("unterminated string".into());
                    };
                    self.i += e.len_utf8();
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex = self.src.get(self.i..self.i + 4).ok_or("bad \\u escape")?;
                            self.i += 4;
                            out.push(
                                u32::from_str_radix(hex, 16)
                                    .ok()
                                    .and_then(char::from_u32)
                                    .ok_or("bad \\u escape")?,
                            );
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }
    fn number(&mut self) -> R<Bson> {
        let start = self.i;
        if matches!(self.s.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        let mut float = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'0'..=b'9' | b'_' => {}
                b'.' | b'e' | b'E' => float = true,
                b'-' | b'+' if matches!(self.s[self.i - 1], b'e' | b'E') => {}
                _ => break,
            }
            self.i += 1;
        }
        let text = self.src[start..self.i].replace('_', "");
        if text == "Infinity" || text == "-Infinity" {
            return Ok(Bson::Double(if text.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        if !float && let Ok(n) = text.parse::<i64>() {
            return Ok(match i32::try_from(n) {
                Ok(n) => Bson::Int32(n),
                Err(_) => Bson::Int64(n),
            });
        }
        text.parse::<f64>()
            .map(Bson::Double)
            .map_err(|_| self.error("invalid number"))
    }
    fn regex(&mut self) -> R<Bson> {
        self.i += 1;
        let mut pattern = String::new();
        let mut class = false;
        loop {
            let Some(c) = self.src[self.i..].chars().next() else {
                return Err("unterminated regular expression".into());
            };
            self.i += c.len_utf8();
            match c {
                '\\' => {
                    pattern.push('\\');
                    if let Some(e) = self.src[self.i..].chars().next() {
                        self.i += e.len_utf8();
                        pattern.push(e);
                    }
                }
                '[' => {
                    class = true;
                    pattern.push(c);
                }
                ']' => {
                    class = false;
                    pattern.push(c);
                }
                '/' if !class => break,
                c => pattern.push(c),
            }
        }
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        let mut options: Vec<char> = self.src[start..self.i].chars().collect();
        options.sort_unstable();
        Ok(Bson::RegularExpression(bson::Regex {
            pattern,
            options: options.into_iter().collect(),
        }))
    }
    fn args(&mut self) -> R<Vec<Bson>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        if self.eat(b')') {
            return Ok(out);
        }
        loop {
            out.push(self.value()?);
            if self.eat(b',') {
                if self.eat(b')') {
                    return Ok(out);
                }
                continue;
            }
            self.expect(b')')?;
            return Ok(out);
        }
    }
    fn value(&mut self) -> R<Bson> {
        match self.peek() {
            None => Err(self.error("expected a value")),
            Some(b'{') => {
                self.i += 1;
                let mut d = Document::new();
                loop {
                    if self.eat(b'}') {
                        return Ok(Bson::Document(d));
                    }
                    let key = match self.peek() {
                        Some(b'"' | b'\'') => self.string()?,
                        Some(c) if c.is_ascii_digit() => {
                            let start = self.i;
                            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                                self.i += 1;
                            }
                            self.src[start..self.i].to_string()
                        }
                        _ => {
                            // Dotted paths are written quoted; `$and` and plain names unquoted.
                            self.ident()
                                .ok_or_else(|| self.error("expected a field name"))?
                        }
                    };
                    self.expect(b':')?;
                    let v = self.value()?;
                    d.insert(key, v);
                    if !self.eat(b',') {
                        self.expect(b'}')?;
                        return Ok(Bson::Document(d));
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                loop {
                    if self.eat(b']') {
                        return Ok(Bson::Array(a));
                    }
                    a.push(self.value()?);
                    if !self.eat(b',') {
                        self.expect(b']')?;
                        return Ok(Bson::Array(a));
                    }
                }
            }
            Some(b'"' | b'\'') => self.string().map(Bson::String),
            Some(b'/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => self.number(),
            Some(_) => {
                let mut name = self.ident().ok_or_else(|| self.error("expected a value"))?;
                if name == "new" {
                    name = self
                        .ident()
                        .ok_or_else(|| self.error("expected a constructor"))?;
                }
                match name.as_str() {
                    "true" => return Ok(Bson::Boolean(true)),
                    "false" => return Ok(Bson::Boolean(false)),
                    "null" | "undefined" => return Ok(Bson::Null),
                    "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
                    "NaN" => return Ok(Bson::Double(f64::NAN)),
                    "MinKey" | "MaxKey" if self.peek() != Some(b'(') => {
                        return Ok(if name == "MinKey" {
                            Bson::MinKey
                        } else {
                            Bson::MaxKey
                        });
                    }
                    _ => {}
                }
                let args = self.args()?;
                constructor(&name, args).map_err(|e| self.error(&e))
            }
        }
    }
}

fn text_arg(args: &[Bson], what: &str) -> Result<String, String> {
    match args.first() {
        Some(Bson::String(s)) => Ok(s.clone()),
        Some(Bson::Int32(n)) => Ok(n.to_string()),
        Some(Bson::Int64(n)) => Ok(n.to_string()),
        Some(Bson::Double(n)) => Ok(n.to_string()),
        _ => Err(format!("{what} takes a string")),
    }
}

fn constructor(name: &str, args: Vec<Bson>) -> Result<Bson, String> {
    Ok(match name {
        "ObjectId" => match args.first() {
            None => Bson::ObjectId(ObjectId::new()),
            Some(_) => Bson::ObjectId(
                ObjectId::parse_str(text_arg(&args, "ObjectId")?)
                    .map_err(|e| format!("invalid ObjectId: {e}"))?,
            ),
        },
        "ISODate" | "Date" => match args.first() {
            None => Bson::DateTime(bson::DateTime::now()),
            Some(Bson::Int32(ms)) => Bson::DateTime(bson::DateTime::from_millis(*ms as i64)),
            Some(Bson::Int64(ms)) => Bson::DateTime(bson::DateTime::from_millis(*ms)),
            Some(Bson::Double(ms)) => Bson::DateTime(bson::DateTime::from_millis(*ms as i64)),
            Some(_) => {
                let s = text_arg(&args, name)?;
                let t = chrono::DateTime::parse_from_rfc3339(&s)
                    .map(|t| t.timestamp_millis())
                    .or_else(|_| {
                        chrono::DateTime::parse_from_rfc3339(&format!("{s}Z"))
                            .map(|t| t.timestamp_millis())
                    })
                    .or_else(|_| {
                        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").map(|d| {
                            d.and_hms_opt(0, 0, 0)
                                .unwrap_or_default()
                                .and_utc()
                                .timestamp_millis()
                        })
                    })
                    .map_err(|_| format!("invalid date \"{s}\""))?;
                Bson::DateTime(bson::DateTime::from_millis(t))
            }
        },
        "NumberLong" | "Long" => Bson::Int64(
            text_arg(&args, name)?
                .parse()
                .map_err(|_| "invalid NumberLong")?,
        ),
        "NumberInt" | "Int32" => Bson::Int32(
            text_arg(&args, name)?
                .parse()
                .map_err(|_| "invalid NumberInt")?,
        ),
        "NumberDecimal" | "Decimal128" => Bson::Decimal128(
            text_arg(&args, name)?
                .parse()
                .map_err(|_| "invalid NumberDecimal")?,
        ),
        "Double" => Bson::Double(
            text_arg(&args, name)?
                .parse()
                .map_err(|_| "invalid Double")?,
        ),
        "UUID" => {
            let u = text_arg(&args, "UUID")?;
            let bytes = hex::decode(u.replace('-', "")).map_err(|_| "invalid UUID")?;
            Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Uuid,
                bytes,
            })
        }
        "BinData" => {
            let subtype = match args.first() {
                Some(Bson::Int32(n)) => *n as u8,
                _ => 0,
            };
            let b64 = match args.get(1) {
                Some(Bson::String(s)) => s.clone(),
                _ => return Err("BinData(subtype, \"base64\")".into()),
            };
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|_| "invalid base64")?;
            Bson::Binary(bson::Binary {
                subtype: subtype.into(),
                bytes,
            })
        }
        "Timestamp" => match (args.first(), args.get(1)) {
            (Some(Bson::Int32(t)), Some(Bson::Int32(i))) => Bson::Timestamp(bson::Timestamp {
                time: *t as u32,
                increment: *i as u32,
            }),
            (Some(Bson::Document(d)), None) => Bson::Timestamp(bson::Timestamp {
                time: d.get_i32("t").unwrap_or(0) as u32,
                increment: d.get_i32("i").unwrap_or(0) as u32,
            }),
            _ => return Err("Timestamp(seconds, increment)".into()),
        },
        "RegExp" => Bson::RegularExpression(bson::Regex {
            pattern: text_arg(&args, "RegExp")?,
            options: match args.get(1) {
                Some(Bson::String(o)) => o.clone(),
                _ => String::new(),
            },
        }),
        "MinKey" => Bson::MinKey,
        "MaxKey" => Bson::MaxKey,
        other => return Err(format!("unknown function {other}()")),
    })
}

/// Parse one statement.
pub fn parse(src: &str) -> Result<Command, String> {
    let text = src.trim().trim_end_matches(';').trim();
    let mut p = P {
        s: text.as_bytes(),
        src: text,
        i: 0,
    };
    let first = p
        .ident()
        .ok_or_else(|| p.error("expected db.<collection>.<method>(…)"))?;
    match first.as_str() {
        "show" => {
            let what = p.ident().unwrap_or_default();
            return match what.as_str() {
                "dbs" | "databases" => Ok(Command::ShowDbs),
                "collections" | "tables" => Ok(Command::ShowCollections),
                _ => Err("show dbs or show collections".into()),
            };
        }
        "use" => {
            let db = p
                .ident()
                .or_else(|| (p.peek().is_some()).then(|| text[p.i..].trim().to_string()))
                .filter(|d| !d.is_empty())
                .ok_or("use <database>")?;
            return Ok(Command::Use(db));
        }
        "db" => {}
        other => return Err(format!("statements start with db. (found \"{other}\")")),
    }
    let mut db = None;
    let mut collection: Option<String> = None;
    let mut calls = Vec::new();
    let mut path: Vec<String> = Vec::new();
    while p.eat(b'.') {
        let name = p.ident().ok_or_else(|| p.error("expected a name"))?;
        if p.peek() == Some(b'(') {
            let args = p.args()?;
            if !path.is_empty() {
                collection = Some(path.join("."));
                path.clear();
            }
            match name.as_str() {
                "getSiblingDB" if collection.is_none() && calls.is_empty() => {
                    db = Some(text_arg(&args, "getSiblingDB")?)
                }
                "getCollection" if collection.is_none() && calls.is_empty() => {
                    collection = Some(text_arg(&args, "getCollection")?)
                }
                _ => calls.push(Call { name, args }),
            }
        } else if calls.is_empty() {
            path.push(name);
        } else {
            return Err(p.error("expected a method call"));
        }
    }
    if p.peek().is_some() {
        return Err(p.error("unexpected text"));
    }
    if !path.is_empty() {
        return Err(format!("db.{}: add a method, e.g. .find()", path.join(".")));
    }
    if calls.is_empty() {
        return Err("expected a method, e.g. db.users.find()".into());
    }
    Ok(Command::Chain {
        db,
        collection,
        calls,
    })
}

/// Shell text of a value (for generated statements).
pub fn to_shell(v: &Bson) -> String {
    match v {
        Bson::String(s) => serde_json::Value::String(s.clone()).to_string(),
        Bson::Document(d) => {
            let parts: Vec<String> = d
                .iter()
                .map(|(k, v)| format!("{}: {}", key(k), to_shell(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Bson::Array(a) => format!(
            "[{}]",
            a.iter().map(to_shell).collect::<Vec<_>>().join(", ")
        ),
        Bson::ObjectId(o) => format!("ObjectId(\"{o}\")"),
        Bson::DateTime(d) => format!(
            "ISODate(\"{}\")",
            d.try_to_rfc3339_string().unwrap_or_default()
        ),
        Bson::Int64(n) => format!("NumberLong(\"{n}\")"),
        Bson::Decimal128(d) => format!("NumberDecimal(\"{d}\")"),
        Bson::RegularExpression(r) => format!("/{}/{}", r.pattern, r.options),
        Bson::Null => "null".into(),
        other => other.clone().into_relaxed_extjson().to_string(),
    }
}

/// A field name as an object key: bare when it is an identifier.
pub fn key(k: &str) -> String {
    let bare = k
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if bare {
        k.to_string()
    } else {
        serde_json::Value::String(k.to_string()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn chains_and_literals() {
        let c = parse("db.users.find({age: {$gt: 30}, 'name': /^a/i, tags: ['x', \"y\",], _id: ObjectId('65f0a1b2c3d4e5f6a7b8c9d0')}, {name: 1}).sort({age: -1}).limit(5);").unwrap();
        let Command::Chain {
            db,
            collection,
            calls,
        } = c
        else {
            panic!()
        };
        assert_eq!((db, collection.as_deref()), (None, Some("users")));
        assert_eq!(
            calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["find", "sort", "limit"]
        );
        let Bson::Document(f) = &calls[0].args[0] else {
            panic!()
        };
        assert_eq!(f.get_document("age").unwrap(), &doc! {"$gt": 30});
        assert_eq!(
            f.get("name"),
            Some(&Bson::RegularExpression(bson::Regex {
                pattern: "^a".into(),
                options: "i".into()
            }))
        );
        assert_eq!(f.get_array("tags").unwrap().len(), 2);
        assert!(matches!(f.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(calls[2].args, vec![Bson::Int32(5)]);
        let Command::Chain { db, collection, calls } = parse("db.getSiblingDB(\"shop\").getCollection(\"order items\").aggregate([{$match: {total: {$gte: 9.5}}}, {$group: {_id: \"$city\", n: {$sum: 1}}}])").unwrap() else { panic!() };
        assert_eq!(
            (db.as_deref(), collection.as_deref(), calls[0].args.len()),
            (Some("shop"), Some("order items"), 1)
        );
        let Command::Chain { collection, calls, .. } = parse("db.logs.app.insertOne({at: ISODate(\"2024-01-02T03:04:05Z\"), n: NumberLong(\"9007199254740993\"), d: NumberDecimal(\"1.10\"), big: 3000000000})").unwrap() else { panic!() };
        assert_eq!(collection.as_deref(), Some("logs.app"));
        let Bson::Document(d) = &calls[0].args[0] else {
            panic!()
        };
        assert_eq!(d.get("n"), Some(&Bson::Int64(9_007_199_254_740_993)));
        assert_eq!(d.get("big"), Some(&Bson::Int64(3_000_000_000)));
        assert!(matches!(d.get("at"), Some(Bson::DateTime(_))));
        let Command::Chain {
            collection, calls, ..
        } = parse("db.runCommand({ping: 1})").unwrap()
        else {
            panic!()
        };
        assert_eq!((collection, calls[0].name.as_str()), (None, "runCommand"));
        assert_eq!(parse("show dbs").unwrap(), Command::ShowDbs);
        assert_eq!(parse("use shop").unwrap(), Command::Use("shop".into()));
        assert!(parse("db.users").unwrap_err().contains("add a method"));
        assert!(parse("db.users.find({a: })").is_err());
        assert!(
            parse("SELECT * FROM users")
                .unwrap_err()
                .contains("start with db.")
        );
        assert_eq!(
            to_shell(&Bson::Document(doc! {"a b": 1, "c": [true]})),
            "{\"a b\": 1, c: [true]}"
        );
    }
}
