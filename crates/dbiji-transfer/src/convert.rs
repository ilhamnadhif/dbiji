//! Conversion of source values (text from CSV/XML, typed values from XLSX,
//! JSON or another database) into values of a target column kind.

use crate::format::BinaryEncoding;
use base64::Engine;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use dbiji_core::value::{parse_date, parse_datetime, parse_hex_bytes, parse_time};
use dbiji_core::{DataKind, Value};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How text is interpreted when converting into typed values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvertOptions {
    /// chrono patterns tried for dates before the built-in ISO formats, e.g. `%d/%m/%Y`.
    pub date_formats: Vec<String>,
    /// chrono patterns tried for timestamps (`%z` in a pattern parses an offset).
    pub datetime_formats: Vec<String>,
    /// chrono patterns tried for times of day.
    pub time_formats: Vec<String>,
    /// Decimal separator used in the input numbers.
    pub decimal_separator: char,
    /// Grouping separator removed from input numbers.
    pub thousands_separator: Option<char>,
    /// Texts converted to NULL for every column (exact match), e.g. `NULL`, `\N`.
    pub null_values: Vec<String>,
    /// Encoding of binary values given as text; `None` detects hex (`0x`, `\x`
    /// or plain hex digits) and falls back to base64.
    pub binary: Option<BinaryEncoding>,
    /// Trim whitespace of values for text columns.
    pub trim_text: bool,
    /// Convert empty texts to NULL for text columns too.
    pub empty_text_as_null: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            date_formats: Vec::new(),
            datetime_formats: Vec::new(),
            time_formats: Vec::new(),
            decimal_separator: '.',
            thousands_separator: None,
            null_values: Vec::new(),
            binary: None,
            trim_text: false,
            empty_text_as_null: false,
        }
    }
}

/// Kinds stored as text by drivers (no client side parsing).
fn is_text_kind(kind: DataKind) -> bool {
    matches!(
        kind,
        DataKind::Text
            | DataKind::Enum
            | DataKind::Xml
            | DataKind::Other
            | DataKind::Interval
            | DataKind::Network
            | DataKind::Range
            | DataKind::Geometry
            | DataKind::Bit
            | DataKind::TimeTz
    )
}

/// Convert `value` into a value suitable for a column of `kind`.
///
/// Text input is parsed (with the custom formats of `opts` first); typed
/// input is coerced where lossless (an integral float into an integer, a
/// midnight timestamp into a date ...). Errors describe the offending value.
pub fn convert_value(value: &Value, kind: DataKind, opts: &ConvertOptions) -> Result<Value, String> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => convert_text(s, kind, opts),
        other => convert_typed(other, kind, opts),
    }
}

fn convert_text(s: &str, kind: DataKind, opts: &ConvertOptions) -> Result<Value, String> {
    if opts.null_values.iter().any(|n| n == s) {
        return Ok(Value::Null);
    }
    if is_text_kind(kind) {
        let t = if opts.trim_text { s.trim() } else { s };
        if t.is_empty() && opts.empty_text_as_null {
            return Ok(Value::Null);
        }
        return Ok(Value::text(t));
    }
    let t = s.trim();
    if t.is_empty() {
        // Readers report missing values as NULL; an empty text is an empty
        // binary value, and has no other meaning for the remaining kinds.
        return Ok(if kind == DataKind::Binary && s.is_empty() { Value::bytes(Vec::<u8>::new()) } else { Value::Null });
    }
    match kind {
        DataKind::Int | DataKind::Float | DataKind::Decimal => {
            let normalized = normalize_number(t, opts);
            Value::parse_as(&normalized, kind)
        }
        DataKind::Date => {
            for f in &opts.date_formats {
                if let Ok(d) = NaiveDate::parse_from_str(t, f) {
                    return Ok(Value::Date(d));
                }
            }
            for f in &opts.datetime_formats {
                if let Ok(dt) = NaiveDateTime::parse_from_str(t, f) {
                    return date_from_datetime(dt);
                }
            }
            match parse_date(t) {
                Some(d) => Ok(Value::Date(d)),
                None => match parse_datetime(t) {
                    Some(dt) => date_from_datetime(dt).map_err(|_| format!("'{t}' is not a date")),
                    None => Err(format!("'{t}' is not a date")),
                },
            }
        }
        DataKind::DateTime => {
            for f in &opts.datetime_formats {
                if let Ok(dt) = NaiveDateTime::parse_from_str(t, f) {
                    return Ok(Value::DateTime(dt));
                }
            }
            for f in &opts.date_formats {
                if let Ok(d) = NaiveDate::parse_from_str(t, f) {
                    return Ok(Value::DateTime(d.and_time(NaiveTime::MIN)));
                }
            }
            match Value::parse_as(t, DataKind::DateTime) {
                Ok(v) => Ok(v),
                // Values with an offset: keep the local wall clock time.
                Err(e) => match parse_tz_strict(t) {
                    Some(dt) => Ok(Value::DateTime(dt.naive_local())),
                    None => Err(e),
                },
            }
        }
        DataKind::DateTimeTz => {
            for f in &opts.datetime_formats {
                if let Ok(dt) = DateTime::parse_from_str(t, f) {
                    return Ok(Value::DateTimeTz(dt));
                }
                if let Ok(dt) = NaiveDateTime::parse_from_str(t, f) {
                    return local_tz(dt).ok_or_else(|| format!("'{t}' does not exist in the local time zone"));
                }
            }
            for f in &opts.date_formats {
                if let Ok(d) = NaiveDate::parse_from_str(t, f) {
                    return local_tz(d.and_time(NaiveTime::MIN))
                        .ok_or_else(|| format!("'{t}' is not a valid local time"));
                }
            }
            Value::parse_as(t, DataKind::DateTimeTz)
        }
        DataKind::Time => {
            for f in &opts.time_formats {
                if let Ok(v) = NaiveTime::parse_from_str(t, f) {
                    return Ok(Value::Time(v));
                }
            }
            match parse_time(t) {
                Some(v) => Ok(Value::Time(v)),
                None => match parse_datetime(t) {
                    Some(dt) => Ok(Value::Time(dt.time())),
                    None => Err(format!("'{t}' is not a time")),
                },
            }
        }
        DataKind::Binary => decode_binary(t, opts.binary).map(Value::bytes),
        DataKind::Json => {
            if crate::sink::is_valid_json(t) {
                Ok(Value::json(t))
            } else {
                Err(format!("'{}' is not valid JSON", shorten(t)))
            }
        }
        DataKind::Array => Ok(parse_array_literal(t).or_else(|| json_array(t)).unwrap_or_else(|| Value::text(s))),
        DataKind::Bool | DataKind::Uuid => Value::parse_as(t, kind),
        _ => Ok(Value::text(s)),
    }
}

fn shorten(s: &str) -> String {
    if s.chars().count() > 40 { format!("{}…", s.chars().take(40).collect::<String>()) } else { s.to_string() }
}

fn date_from_datetime(dt: NaiveDateTime) -> Result<Value, String> {
    if dt.time() == NaiveTime::MIN {
        Ok(Value::Date(dt.date()))
    } else {
        Err(format!("'{dt}' has a time of day and cannot be stored as a date"))
    }
}

fn local_tz(dt: NaiveDateTime) -> Option<Value> {
    dt.and_local_timezone(chrono::Local).earliest().map(|d| Value::DateTimeTz(d.fixed_offset()))
}

/// Timestamps that carry an explicit offset (no local time zone fallback).
pub(crate) fn parse_tz_strict(t: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(v) = DateTime::parse_from_rfc3339(t) {
        return Some(v);
    }
    for f in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%d %H:%M%#z",
    ] {
        if let Ok(v) = DateTime::parse_from_str(t, f) {
            return Some(v);
        }
    }
    None
}

/// Remove grouping separators and use `.` as decimal point.
pub(crate) fn normalize_number(t: &str, opts: &ConvertOptions) -> String {
    let mut s: String = match opts.thousands_separator {
        Some(sep) => t.chars().filter(|c| *c != sep).collect(),
        None => t.to_string(),
    };
    if opts.decimal_separator != '.' {
        s = s.replace(opts.decimal_separator, ".");
    }
    s
}

fn decode_binary(t: &str, encoding: Option<BinaryEncoding>) -> Result<Vec<u8>, String> {
    let b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD.decode(s.as_bytes()).map_err(|e| format!("invalid base64: {e}"))
    };
    match encoding {
        Some(BinaryEncoding::Hex) => parse_hex_bytes(t).ok_or_else(|| format!("'{}' is not hexadecimal", shorten(t))),
        Some(BinaryEncoding::Base64) => b64(t),
        Some(BinaryEncoding::Text) => Ok(t.as_bytes().to_vec()),
        None => match parse_hex_bytes(t) {
            Some(b) => Ok(b),
            None => b64(t).map_err(|_| format!("'{}' is neither hexadecimal nor base64", shorten(t))),
        },
    }
}

fn convert_typed(v: &Value, kind: DataKind, opts: &ConvertOptions) -> Result<Value, String> {
    use Value as V;
    let text_fallback = || convert_text(&v.to_text(), kind, opts);
    match kind {
        DataKind::Json => Ok(match v {
            V::Json(_) => v.clone(),
            V::Array(_) => V::json(v.to_json().to_string()),
            V::Bool(_) | V::Int(_) | V::UInt(_) => V::json(v.to_text()),
            V::Float(f) if f.is_finite() => V::json(v.to_text()),
            V::Decimal(s) if crate::sink::is_json_number(s) => V::json(s.to_string()),
            other => V::json(other.to_json().to_string()),
        }),
        k if is_text_kind(k) => Ok(V::text(v.to_text())),
        DataKind::Bool => match v {
            V::Bool(_) => Ok(v.clone()),
            V::Int(i) => Ok(V::Bool(*i != 0)),
            V::UInt(u) => Ok(V::Bool(*u != 0)),
            V::Float(f) if *f == 0.0 || *f == 1.0 => Ok(V::Bool(*f == 1.0)),
            _ => text_fallback(),
        },
        DataKind::Int => match v {
            V::Int(_) | V::UInt(_) => Ok(v.clone()),
            V::Bool(b) => Ok(V::Int(i64::from(*b))),
            V::Float(f) if f.fract() == 0.0 && *f >= -9.2e18 && *f <= 9.2e18 => Ok(V::Int(*f as i64)),
            V::Float(f) => Err(format!("{f} is not an integer")),
            V::Decimal(s) => {
                let int_part = match s.split_once('.') {
                    Some((i, frac)) if frac.chars().all(|c| c == '0') => i,
                    Some(_) => return Err(format!("{s} is not an integer")),
                    None => s,
                };
                Value::parse_as(int_part, DataKind::Int)
            }
            _ => text_fallback(),
        },
        DataKind::Float => match v {
            V::Float(_) => Ok(v.clone()),
            V::Int(i) => Ok(V::Float(*i as f64)),
            V::UInt(u) => Ok(V::Float(*u as f64)),
            V::Bool(b) => Ok(V::Float(if *b { 1.0 } else { 0.0 })),
            V::Decimal(s) => Value::parse_as(s, DataKind::Float),
            _ => text_fallback(),
        },
        DataKind::Decimal => match v {
            V::Decimal(_) => Ok(v.clone()),
            V::Int(_) | V::UInt(_) => Ok(V::decimal(v.to_text())),
            V::Float(f) if f.is_finite() => Ok(V::decimal(format!("{f}"))),
            V::Float(_) => Ok(V::decimal(v.to_text())),
            V::Bool(b) => Ok(V::decimal(if *b { "1" } else { "0" })),
            _ => text_fallback(),
        },
        DataKind::Date => match v {
            V::Date(_) => Ok(v.clone()),
            V::DateTime(dt) => date_from_datetime(*dt),
            V::DateTimeTz(dt) => date_from_datetime(dt.naive_local()),
            _ => text_fallback(),
        },
        DataKind::DateTime => match v {
            V::DateTime(_) => Ok(v.clone()),
            V::Date(d) => Ok(V::DateTime(d.and_time(NaiveTime::MIN))),
            V::DateTimeTz(dt) => Ok(V::DateTime(dt.naive_local())),
            _ => text_fallback(),
        },
        DataKind::DateTimeTz => match v {
            V::DateTimeTz(_) => Ok(v.clone()),
            V::DateTime(dt) => local_tz(*dt).ok_or_else(|| format!("'{dt}' does not exist in the local time zone")),
            V::Date(d) => {
                local_tz(d.and_time(NaiveTime::MIN)).ok_or_else(|| format!("'{d}' is not a valid local time"))
            }
            _ => text_fallback(),
        },
        DataKind::Time => match v {
            V::Time(_) => Ok(v.clone()),
            V::DateTime(dt) => Ok(V::Time(dt.time())),
            V::DateTimeTz(dt) => Ok(V::Time(dt.naive_local().time())),
            _ => text_fallback(),
        },
        DataKind::Uuid => match v {
            V::Uuid(_) => Ok(v.clone()),
            V::Bytes(b) if b.len() == 16 => Uuid::from_slice(b).map(V::Uuid).map_err(|e| e.to_string()),
            _ => text_fallback(),
        },
        DataKind::Binary => match v {
            V::Bytes(_) => Ok(v.clone()),
            V::Uuid(u) => Ok(V::bytes(u.as_bytes().to_vec())),
            _ => text_fallback(),
        },
        DataKind::Array => match v {
            V::Array(_) => Ok(v.clone()),
            V::Json(s) => Ok(json_array(s).unwrap_or_else(|| v.clone())),
            _ => text_fallback(),
        },
        _ => Ok(V::text(v.to_text())),
    }
}

/// A JSON array as [`Value::Array`]: integers become `Int`, other numbers
/// `Float`, objects stay JSON text.
fn json_array(s: &str) -> Option<Value> {
    fn conv(j: serde_json::Value) -> Value {
        match j {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Value::UInt(u)
                } else {
                    Value::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            serde_json::Value::String(s) => Value::text(s),
            serde_json::Value::Array(items) => Value::Array(items.into_iter().map(conv).collect()),
            obj @ serde_json::Value::Object(_) => Value::json(obj.to_string()),
        }
    }
    match serde_json::from_str::<serde_json::Value>(s.trim()) {
        Ok(serde_json::Value::Array(items)) => Some(Value::Array(items.into_iter().map(conv).collect())),
        _ => None,
    }
}

/// Parse a PostgreSQL style array literal (`{1,NULL,"a b",{2,3}}`) as
/// produced by [`Value::to_text`]. Quoted elements are text; unquoted
/// elements become NULL, integers, floats or booleans when they look like one.
pub(crate) fn parse_array_literal(s: &str) -> Option<Value> {
    fn element(raw: &str, quoted: bool) -> Value {
        if quoted {
            return Value::text(raw);
        }
        if raw.eq_ignore_ascii_case("null") {
            return Value::Null;
        }
        if let Ok(i) = raw.parse::<i64>() {
            return Value::Int(i);
        }
        if dbiji_core::value::is_decimal_text(raw)
            && let Ok(f) = raw.parse::<f64>()
        {
            return Value::Float(f);
        }
        match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::text(raw),
        }
    }
    fn parse(chars: &[char], pos: &mut usize, depth: usize) -> Option<Value> {
        if depth > 64 || chars.get(*pos) != Some(&'{') {
            return None;
        }
        *pos += 1;
        let mut items = Vec::new();
        if chars.get(*pos) == Some(&'}') {
            *pos += 1;
            return Some(Value::Array(items.into_boxed_slice()));
        }
        loop {
            while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
                *pos += 1;
            }
            match chars.get(*pos)? {
                '{' => items.push(parse(chars, pos, depth + 1)?),
                '"' => {
                    *pos += 1;
                    let mut text = String::new();
                    loop {
                        match chars.get(*pos)? {
                            '\\' => {
                                *pos += 1;
                                text.push(*chars.get(*pos)?);
                            }
                            '"' => break,
                            c => text.push(*c),
                        }
                        *pos += 1;
                    }
                    *pos += 1;
                    items.push(element(&text, true));
                }
                _ => {
                    let start = *pos;
                    while let Some(c) = chars.get(*pos) {
                        if *c == ',' || *c == '}' {
                            break;
                        }
                        *pos += 1;
                    }
                    let raw: String = chars[start..*pos].iter().collect();
                    items.push(element(raw.trim(), false));
                }
            }
            while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
                *pos += 1;
            }
            match chars.get(*pos)? {
                ',' => *pos += 1,
                '}' => {
                    *pos += 1;
                    return Some(Value::Array(items.into_boxed_slice()));
                }
                _ => return None,
            }
        }
    }
    let chars: Vec<char> = s.trim().chars().collect();
    let mut pos = 0;
    let v = parse(&chars, &mut pos, 0)?;
    (pos == chars.len()).then_some(v)
}

/// Logical kind from its lower case name (`int`, `datetimetz` ...), as written
/// by the JSON export.
pub(crate) fn kind_from_name(name: &str) -> Option<DataKind> {
    const ALL: [DataKind; 22] = [
        DataKind::Bool,
        DataKind::Int,
        DataKind::Float,
        DataKind::Decimal,
        DataKind::Text,
        DataKind::Binary,
        DataKind::Date,
        DataKind::Time,
        DataKind::TimeTz,
        DataKind::DateTime,
        DataKind::DateTimeTz,
        DataKind::Interval,
        DataKind::Uuid,
        DataKind::Json,
        DataKind::Xml,
        DataKind::Array,
        DataKind::Geometry,
        DataKind::Enum,
        DataKind::Network,
        DataKind::Bit,
        DataKind::Range,
        DataKind::Other,
    ];
    ALL.into_iter().find(|k| format!("{k:?}").eq_ignore_ascii_case(name.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(v: Value, k: DataKind) -> Result<Value, String> {
        convert_value(&v, k, &ConvertOptions::default())
    }

    #[test]
    fn text_conversions() {
        assert_eq!(c(Value::text(" 42 "), DataKind::Int).unwrap(), Value::Int(42));
        assert_eq!(c(Value::text(""), DataKind::Int).unwrap(), Value::Null);
        assert_eq!(c(Value::text(""), DataKind::Text).unwrap(), Value::text(""));
        assert_eq!(c(Value::text(" a "), DataKind::Text).unwrap(), Value::text(" a "));
        assert!(c(Value::text("x"), DataKind::Int).is_err());
        assert_eq!(c(Value::text("0xff"), DataKind::Binary).unwrap(), Value::bytes(vec![255u8]));
        assert_eq!(c(Value::text("AAEC/w=="), DataKind::Binary).unwrap(), Value::bytes(vec![0x00u8, 0x01, 0x02, 0xff]));
        // Ambiguous input is read as hex.
        assert_eq!(c(Value::text("AAEC"), DataKind::Binary).unwrap(), Value::bytes(vec![0xAAu8, 0xEC]));
        assert_eq!(c(Value::text("{\"a\":1}"), DataKind::Json).unwrap(), Value::json("{\"a\":1}"));
        assert!(c(Value::text("{x"), DataKind::Json).is_err());
        assert_eq!(
            c(Value::text("{1,NULL,\"x y\",{2}}"), DataKind::Array).unwrap(),
            Value::Array(
                vec![
                    Value::Int(1),
                    Value::Null,
                    Value::text("x y"),
                    Value::Array(vec![Value::Int(2)].into_boxed_slice())
                ]
                .into_boxed_slice()
            )
        );
        assert_eq!(c(Value::text("2024-01-02 00:00:00"), DataKind::Date).unwrap().to_text(), "2024-01-02");
        assert!(c(Value::text("2024-01-02 10:00:00"), DataKind::Date).is_err());
    }

    #[test]
    fn custom_formats_and_separators() {
        let o = ConvertOptions {
            date_formats: vec!["%d/%m/%Y".into()],
            datetime_formats: vec!["%d/%m/%Y %H.%M".into()],
            decimal_separator: ',',
            thousands_separator: Some('.'),
            null_values: vec!["N/A".into()],
            ..Default::default()
        };
        assert_eq!(convert_value(&Value::text("31/01/2024"), DataKind::Date, &o).unwrap().to_text(), "2024-01-31");
        assert_eq!(
            convert_value(&Value::text("31/01/2024 10.30"), DataKind::DateTime, &o).unwrap().to_text(),
            "2024-01-31 10:30:00"
        );
        assert_eq!(convert_value(&Value::text("1.234,50"), DataKind::Decimal, &o).unwrap(), Value::decimal("1234.50"));
        assert_eq!(convert_value(&Value::text("N/A"), DataKind::Text, &o).unwrap(), Value::Null);
    }

    #[test]
    fn typed_conversions() {
        assert_eq!(c(Value::Float(3.0), DataKind::Int).unwrap(), Value::Int(3));
        assert!(c(Value::Float(3.5), DataKind::Int).is_err());
        assert_eq!(c(Value::decimal("12.000"), DataKind::Int).unwrap(), Value::Int(12));
        assert_eq!(c(Value::Int(1), DataKind::Bool).unwrap(), Value::Bool(true));
        assert_eq!(c(Value::Float(0.1), DataKind::Decimal).unwrap(), Value::decimal("0.1"));
        assert_eq!(c(Value::Int(5), DataKind::Text).unwrap(), Value::text("5"));
        assert_eq!(c(Value::json("[1,2]"), DataKind::Array).unwrap().to_text(), "{1,2}");
        assert_eq!(
            c(Value::Array(vec![Value::Int(1)].into_boxed_slice()), DataKind::Json).unwrap(),
            Value::json("[1]")
        );
        let dt = NaiveDate::from_ymd_opt(2024, 5, 6).unwrap().and_time(NaiveTime::MIN);
        assert_eq!(c(Value::DateTime(dt), DataKind::Date).unwrap(), Value::Date(dt.date()));
    }

    #[test]
    fn kind_names() {
        assert_eq!(kind_from_name("datetimetz"), Some(DataKind::DateTimeTz));
        assert_eq!(kind_from_name("INT"), Some(DataKind::Int));
        assert_eq!(kind_from_name("nope"), None);
        assert!(parse_array_literal("{1,2").is_none());
        assert!(parse_array_literal("[1]").is_none());
    }
}
