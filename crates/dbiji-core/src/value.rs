//! Database value model shared by all drivers, the grid and data transfer.
//!
//! [`Value`] is deliberately small (24 bytes): heap payloads are boxed slices,
//! and database specific types without a natural Rust representation
//! (intervals, ranges, inet, geometry as EWKT, enums ...) are carried as
//! [`Value::Text`] while [`ColumnMeta::kind`] tells how to interpret them.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use uuid::Uuid;

/// One row of a result set.
pub type Row = Vec<Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Exact numeric kept as text to avoid precision loss (NUMERIC, DECIMAL, MONEY).
    Decimal(Box<str>),
    Text(Box<str>),
    Bytes(Box<[u8]>),
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(NaiveDateTime),
    DateTimeTz(DateTime<FixedOffset>),
    Uuid(Uuid),
    /// JSON document as text (compact or as returned by the server).
    Json(Box<str>),
    Array(Box<[Value]>),
}

/// Logical type of a column, independent of the database specific type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum DataKind {
    Bool,
    Int,
    Float,
    Decimal,
    #[default]
    Text,
    Binary,
    Date,
    Time,
    TimeTz,
    DateTime,
    DateTimeTz,
    Interval,
    Uuid,
    Json,
    Xml,
    Array,
    Geometry,
    Enum,
    Network,
    Bit,
    Range,
    Other,
}

impl DataKind {
    pub fn is_numeric(self) -> bool {
        matches!(self, DataKind::Int | DataKind::Float | DataKind::Decimal)
    }

    pub fn is_temporal(self) -> bool {
        matches!(self, DataKind::Date | DataKind::Time | DataKind::TimeTz | DataKind::DateTime | DataKind::DateTimeTz | DataKind::Interval)
    }

    pub fn is_textual(self) -> bool {
        matches!(self, DataKind::Text | DataKind::Enum | DataKind::Xml | DataKind::Json)
    }

    /// Heuristic mapping from a database type name to a logical kind. Drivers
    /// should map their own types precisely; this is a fallback for generic code
    /// (imports, migrations, tests).
    pub fn guess_from_type_name(type_name: &str) -> DataKind {
        let t = type_name.trim().to_ascii_lowercase();
        if t.ends_with("[]") || t.starts_with('_') || t.starts_with("array") || t.starts_with("list") {
            return DataKind::Array;
        }
        let base = t.split(['(', ' ']).next().unwrap_or("");
        match base {
            "bool" | "boolean" | "bit" if t == "bit(1)" || base != "bit" => DataKind::Bool,
            "bit" | "varbit" => DataKind::Bit,
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "int2" | "int4" | "int8" | "serial" | "bigserial" | "smallserial" | "serial4" | "serial8"
            | "oid" | "year" | "hugeint" | "ubigint" | "uinteger" | "usmallint" | "utinyint" | "int16" | "int32" | "int64" | "int128" | "int256" | "uint8" | "uint16"
            | "uint32" | "uint64" | "int1" | "long" => DataKind::Int,
            "real" | "float" | "float4" | "float8" | "double" | "float32" | "float64" => DataKind::Float,
            "numeric" | "decimal" | "money" | "smallmoney" | "number" | "dec" | "fixed" => DataKind::Decimal,
            "date" | "date32" => DataKind::Date,
            "time" if t.contains("with time zone") => DataKind::TimeTz,
            "time" | "time without time zone" => DataKind::Time,
            "timetz" => DataKind::TimeTz,
            "timestamp" if t.contains("with time zone") => DataKind::DateTimeTz,
            "timestamptz" | "datetimeoffset" => DataKind::DateTimeTz,
            "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "datetime64" => DataKind::DateTime,
            "interval" => DataKind::Interval,
            "uuid" | "uniqueidentifier" => DataKind::Uuid,
            "json" | "jsonb" => DataKind::Json,
            "xml" => DataKind::Xml,
            "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image" | "raw" | "bytes" => DataKind::Binary,
            "geometry" | "geography" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" | "box2d" | "box3d" => {
                DataKind::Geometry
            }
            "inet" | "cidr" | "macaddr" | "macaddr8" | "ipv4" | "ipv6" => DataKind::Network,
            "enum" | "set" => DataKind::Enum,
            "int4range" | "int8range" | "numrange" | "tsrange" | "tstzrange" | "daterange" | "int4multirange" | "int8multirange" | "nummultirange" | "tsmultirange"
            | "tstzmultirange" | "datemultirange" => DataKind::Range,
            "char" | "character" | "varchar" | "nvarchar" | "nchar" | "text" | "tinytext" | "mediumtext" | "longtext" | "ntext" | "string" | "citext" | "name" | "clob"
            | "bpchar" | "character varying" | "fixedstring" | "lowcardinality" | "sysname" => DataKind::Text,
            _ => DataKind::Other,
        }
    }
}

/// Where a result column comes from. Present when the driver could resolve the
/// base table of the column; required for editing result sets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnSource {
    /// Catalog/database (PostgreSQL database, SQL Server catalog). Usually `None`.
    pub database: Option<String>,
    /// Schema used to qualify the table in SQL (MySQL database, SQLite attached db name).
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

/// Description of a result set column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ColumnMeta {
    /// Label shown in the result header.
    pub name: String,
    /// Database specific type name, e.g. `int4`, `varchar(255)`, `geometry`.
    pub type_name: String,
    pub kind: DataKind,
    pub nullable: Option<bool>,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    pub max_length: Option<u32>,
    pub auto_increment: Option<bool>,
    pub source: Option<ColumnSource>,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, kind: DataKind) -> Self {
        ColumnMeta { name: name.into(), type_name: type_name.into(), kind, ..Default::default() }
    }

    pub fn with_source(mut self, source: ColumnSource) -> Self {
        self.source = Some(source);
        self
    }

    pub fn is_numeric(&self) -> bool {
        self.kind.is_numeric()
    }
}

/// How binary values are rendered as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BinaryFormat {
    #[default]
    Hex,
    Base64,
    /// Interpret as UTF-8 (lossy).
    Text,
}

/// How booleans are rendered in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BoolStyle {
    TrueFalse,
    #[default]
    Checkbox,
    OneZero,
}

/// User configurable data formatting ("format data" settings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatOptions {
    pub null_text: String,
    /// chrono strftime patterns
    pub date_format: String,
    pub time_format: String,
    pub datetime_format: String,
    pub show_fractional_seconds: bool,
    /// Maximum fraction digits for floats; `None` keeps the shortest exact representation.
    pub float_max_decimals: Option<usize>,
    pub thousands_separator: bool,
    pub binary_format: BinaryFormat,
    /// Maximum number of bytes shown for binary values in a grid cell.
    pub binary_max_len: usize,
    /// Text longer than this is truncated in grid cells (the value panel shows everything).
    pub max_text_len: usize,
    /// Saved as `bool_display`: settings of earlier versions (true / false
    /// by default) switch to the check boxes once.
    #[serde(rename = "bool_display")]
    pub bool_style: BoolStyle,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            null_text: "[NULL]".into(),
            date_format: "%Y-%m-%d".into(),
            time_format: "%H:%M:%S".into(),
            datetime_format: "%Y-%m-%d %H:%M:%S".into(),
            show_fractional_seconds: true,
            float_max_decimals: None,
            thousands_separator: false,
            binary_format: BinaryFormat::Hex,
            binary_max_len: 64,
            max_text_len: 512,
            bool_style: BoolStyle::Checkbox,
        }
    }
}

fn group_thousands(int_part: &str) -> String {
    let (sign, digits) = match int_part.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", int_part),
    };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

fn group_number_text(s: &str) -> String {
    let (int_part, rest) = match s.find(['.', 'e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if int_part.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) { format!("{}{}", group_thousands(int_part), rest) } else { s.to_string() }
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        let s = format!("{f}");
        s
    }
}

fn truncate_chars(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &s[..end]))
}

fn time_text(t: &NaiveTime, fractional: bool) -> String {
    if fractional && t.nanosecond() != 0 {
        let micros = t.nanosecond() / 1000;
        if t.nanosecond().is_multiple_of(1000) {
            format!("{}.{:06}", t.format("%H:%M:%S"), micros).trim_end_matches('0').to_string()
        } else {
            format!("{}.{:09}", t.format("%H:%M:%S"), t.nanosecond()).trim_end_matches('0').to_string()
        }
    } else {
        t.format("%H:%M:%S").to_string()
    }
}

fn offset_text(o: &FixedOffset) -> String {
    let secs = o.local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.abs();
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if m == 0 { format!("{sign}{h:02}") } else { format!("{sign}{h:02}:{m:02}") }
}

impl Value {
    pub fn text(s: impl Into<String>) -> Value {
        Value::Text(s.into().into_boxed_str())
    }
    pub fn decimal(s: impl Into<String>) -> Value {
        Value::Decimal(s.into().into_boxed_str())
    }
    pub fn json(s: impl Into<String>) -> Value {
        Value::Json(s.into().into_boxed_str())
    }
    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into().into_boxed_slice())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) | Value::Decimal(s) | Value::Json(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::UInt(u) => i64::try_from(*u).ok(),
            Value::Bool(b) => Some(*b as i64),
            Value::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            Value::UInt(u) => Some(*u != 0),
            Value::Text(s) => parse_bool(s),
            _ => None,
        }
    }

    /// Name of the variant, for diagnostics.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::UInt(_) => "uint",
            Value::Float(_) => "float",
            Value::Decimal(_) => "decimal",
            Value::Text(_) => "text",
            Value::Bytes(_) => "bytes",
            Value::Date(_) => "date",
            Value::Time(_) => "time",
            Value::DateTime(_) => "datetime",
            Value::DateTimeTz(_) => "datetimetz",
            Value::Uuid(_) => "uuid",
            Value::Json(_) => "json",
            Value::Array(_) => "array",
        }
    }

    /// Canonical text form, independent of user formatting settings. Used for
    /// copy, CSV export and as the starting text of inline editing.
    /// `Null` becomes an empty string; binary becomes lowercase hex.
    pub fn to_text(&self) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed(""),
            Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Value::Int(i) => Cow::Owned(i.to_string()),
            Value::UInt(u) => Cow::Owned(u.to_string()),
            Value::Float(f) => Cow::Owned(float_text(*f)),
            Value::Decimal(s) | Value::Text(s) | Value::Json(s) => Cow::Borrowed(s),
            Value::Bytes(b) => Cow::Owned(hex::encode(b)),
            Value::Date(d) => Cow::Owned(d.format("%Y-%m-%d").to_string()),
            Value::Time(t) => Cow::Owned(time_text(t, true)),
            Value::DateTime(dt) => Cow::Owned(format!("{} {}", dt.date().format("%Y-%m-%d"), time_text(&dt.time(), true))),
            Value::DateTimeTz(dt) => {
                Cow::Owned(format!("{} {}{}", dt.naive_local().date().format("%Y-%m-%d"), time_text(&dt.naive_local().time(), true), offset_text(dt.offset())))
            }
            Value::Uuid(u) => Cow::Owned(u.hyphenated().to_string()),
            Value::Array(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|v| match v {
                        Value::Null => "NULL".to_string(),
                        Value::Text(s) => {
                            if s.is_empty() || s.contains([',', '{', '}', '"', '\\', ' ']) || s.eq_ignore_ascii_case("null") {
                                format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
                            } else {
                                s.to_string()
                            }
                        }
                        other => other.to_text().into_owned(),
                    })
                    .collect();
                Cow::Owned(format!("{{{}}}", parts.join(",")))
            }
        }
    }

    /// Text for a grid cell using user formatting settings.
    pub fn display(&self, opts: &FormatOptions) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Owned(opts.null_text.clone()),
            Value::Bool(b) => match opts.bool_style {
                BoolStyle::OneZero => Cow::Borrowed(if *b { "1" } else { "0" }),
                BoolStyle::Checkbox => Cow::Borrowed(if *b { "☑" } else { "☐" }),
                BoolStyle::TrueFalse => Cow::Borrowed(if *b { "true" } else { "false" }),
            },
            Value::Int(i) => {
                let s = i.to_string();
                if opts.thousands_separator { Cow::Owned(group_thousands(&s)) } else { Cow::Owned(s) }
            }
            Value::UInt(u) => {
                let s = u.to_string();
                if opts.thousands_separator { Cow::Owned(group_thousands(&s)) } else { Cow::Owned(s) }
            }
            Value::Float(f) => {
                let s = match opts.float_max_decimals {
                    Some(d) if f.is_finite() => {
                        let s = format!("{f:.d$}");
                        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
                    }
                    _ => float_text(*f),
                };
                if opts.thousands_separator { Cow::Owned(group_number_text(&s)) } else { Cow::Owned(s) }
            }
            Value::Decimal(s) => {
                if opts.thousands_separator {
                    Cow::Owned(group_number_text(s))
                } else {
                    Cow::Borrowed(s)
                }
            }
            Value::Text(s) | Value::Json(s) => {
                let first_line_end = s.find('\n');
                match first_line_end {
                    Some(i) => {
                        let mut t: String = s[..i].to_string();
                        t.push('¶');
                        Cow::Owned(truncate_chars(&t, opts.max_text_len).into_owned())
                    }
                    None => truncate_chars(s, opts.max_text_len),
                }
            }
            Value::Bytes(b) => {
                let shown = &b[..b.len().min(opts.binary_max_len)];
                let more = if b.len() > shown.len() { "…" } else { "" };
                match opts.binary_format {
                    BinaryFormat::Hex => Cow::Owned(format!("0x{}{}", hex::encode_upper(shown), more)),
                    BinaryFormat::Base64 => {
                        use base64::Engine;
                        Cow::Owned(format!("{}{}", base64::engine::general_purpose::STANDARD.encode(shown), more))
                    }
                    BinaryFormat::Text => Cow::Owned(format!("{}{}", String::from_utf8_lossy(shown), more)),
                }
            }
            Value::Date(d) => Cow::Owned(d.format(&opts.date_format).to_string()),
            Value::Time(t) => {
                if opts.show_fractional_seconds && opts.time_format == "%H:%M:%S" {
                    Cow::Owned(time_text(t, true))
                } else {
                    Cow::Owned(t.format(&opts.time_format).to_string())
                }
            }
            Value::DateTime(dt) => {
                if opts.show_fractional_seconds && opts.datetime_format == "%Y-%m-%d %H:%M:%S" {
                    Cow::Owned(format!("{} {}", dt.date().format("%Y-%m-%d"), time_text(&dt.time(), true)))
                } else {
                    Cow::Owned(dt.format(&opts.datetime_format).to_string())
                }
            }
            Value::DateTimeTz(dt) => {
                if opts.show_fractional_seconds && opts.datetime_format == "%Y-%m-%d %H:%M:%S" {
                    Cow::Owned(self.to_text().into_owned())
                } else {
                    Cow::Owned(format!("{}{}", dt.naive_local().format(&opts.datetime_format), offset_text(dt.offset())))
                }
            }
            Value::Uuid(_) | Value::Array(_) => {
                let t = self.to_text().into_owned();
                Cow::Owned(truncate_chars(&t, opts.max_text_len).into_owned())
            }
        }
    }

    /// Parse user input (inline editing, imports) into a value of `kind`.
    /// An empty input yields `Null` for every non-textual kind.
    pub fn parse_as(input: &str, kind: DataKind) -> Result<Value, String> {
        let t = input.trim();
        if t.is_empty() && !matches!(kind, DataKind::Text | DataKind::Enum | DataKind::Xml) {
            return Ok(Value::Null);
        }
        match kind {
            DataKind::Bool => parse_bool(t).map(Value::Bool).ok_or_else(|| format!("'{t}' is not a boolean")),
            DataKind::Int => {
                if let Ok(i) = t.parse::<i64>() {
                    Ok(Value::Int(i))
                } else if let Ok(u) = t.parse::<u64>() {
                    Ok(Value::UInt(u))
                } else {
                    Err(format!("'{t}' is not an integer"))
                }
            }
            DataKind::Float => match t {
                "NaN" | "nan" => Ok(Value::Float(f64::NAN)),
                "Infinity" | "inf" => Ok(Value::Float(f64::INFINITY)),
                "-Infinity" | "-inf" => Ok(Value::Float(f64::NEG_INFINITY)),
                _ => t.parse::<f64>().map(Value::Float).map_err(|_| format!("'{t}' is not a number")),
            },
            DataKind::Decimal => {
                if is_decimal_text(t) {
                    Ok(Value::decimal(t))
                } else {
                    Err(format!("'{t}' is not a decimal number"))
                }
            }
            DataKind::Date => parse_date(t).map(Value::Date).ok_or_else(|| format!("'{t}' is not a date (expected YYYY-MM-DD)")),
            DataKind::Time => parse_time(t).map(Value::Time).ok_or_else(|| format!("'{t}' is not a time (expected HH:MM:SS)")),
            DataKind::DateTime => parse_datetime(t).map(Value::DateTime).ok_or_else(|| format!("'{t}' is not a timestamp (expected YYYY-MM-DD HH:MM:SS)")),
            DataKind::DateTimeTz => parse_datetime_tz(t).map(Value::DateTimeTz).ok_or_else(|| format!("'{t}' is not a timestamp with time zone")),
            DataKind::Uuid => Uuid::parse_str(t).map(Value::Uuid).map_err(|_| format!("'{t}' is not a UUID")),
            DataKind::Json => serde_json::from_str::<serde_json::Value>(t).map(|_| Value::json(t)).map_err(|e| format!("Invalid JSON: {e}")),
            DataKind::Binary => parse_hex_bytes(t).map(Value::bytes).ok_or_else(|| "Binary values must be hexadecimal (optionally prefixed with 0x or \\x)".to_string()),
            _ => Ok(Value::text(input)),
        }
    }

    /// Total order used for client side sorting. Nulls sort first.
    pub fn cmp_for_sort(&self, other: &Value) -> Ordering {
        use Value::*;
        fn rank(v: &Value) -> u8 {
            match v {
                Null => 0,
                Bool(_) => 1,
                Int(_) | UInt(_) | Float(_) | Decimal(_) => 2,
                Date(_) | DateTime(_) | DateTimeTz(_) => 3,
                Time(_) => 4,
                Uuid(_) => 5,
                Text(_) | Json(_) => 6,
                Bytes(_) => 7,
                Array(_) => 8,
            }
        }
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Bool(a), Bool(b)) => a.cmp(b),
            (Int(a), Int(b)) => a.cmp(b),
            (UInt(a), UInt(b)) => a.cmp(b),
            (Int(a), UInt(b)) => (*a as i128).cmp(&(*b as i128)),
            (UInt(a), Int(b)) => (*a as i128).cmp(&(*b as i128)),
            (Text(a), Text(b)) | (Json(a), Json(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            (Time(a), Time(b)) => a.cmp(b),
            (DateTime(a), DateTime(b)) => a.cmp(b),
            (DateTimeTz(a), DateTimeTz(b)) => a.cmp(b),
            (Date(a), DateTime(b)) => a.and_hms_opt(0, 0, 0).unwrap().cmp(b),
            (DateTime(a), Date(b)) => a.cmp(&b.and_hms_opt(0, 0, 0).unwrap()),
            (Uuid(a), Uuid(b)) => a.cmp(b),
            (Bytes(a), Bytes(b)) => a.cmp(b),
            (Array(a), Array(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let o = x.cmp_for_sort(y);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) if rank(a) == 2 && rank(b) == 2 => {
                let (x, y) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
                x.partial_cmp(&y).unwrap_or_else(|| x.is_nan().cmp(&y.is_nan()))
            }
            (a, b) => rank(a).cmp(&rank(b)).then_with(|| a.to_text().cmp(&b.to_text())),
        }
    }

    /// Approximate heap bytes owned by this value (excluding the 24 inline bytes).
    pub fn heap_size(&self) -> usize {
        match self {
            Value::Decimal(s) | Value::Text(s) | Value::Json(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Array(items) => items.len() * std::mem::size_of::<Value>() + items.iter().map(Value::heap_size).sum::<usize>(),
            _ => 0,
        }
    }

    /// Convert to a JSON value (for JSON export, AI context, clipboard).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Int(i) => J::from(*i),
            Value::UInt(u) => J::from(*u),
            Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or_else(|| J::String(float_text(*f))),
            Value::Decimal(s) => match s.parse::<serde_json::Number>() {
                Ok(n) => J::Number(n),
                Err(_) => J::String(s.to_string()),
            },
            Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| J::String(s.to_string())),
            Value::Array(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Bytes(b) => {
                use base64::Engine;
                J::String(base64::engine::general_purpose::STANDARD.encode(b))
            }
            other => J::String(other.to_text().into_owned()),
        }
    }

    /// Append a compact binary encoding of the value (used for spilling rows to disk).
    pub fn encode(&self, out: &mut Vec<u8>) {
        fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        match self {
            Value::Null => out.push(0),
            Value::Bool(b) => {
                out.push(1);
                out.push(*b as u8);
            }
            Value::Int(i) => {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::UInt(u) => {
                out.push(3);
                out.extend_from_slice(&u.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(4);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Decimal(s) => {
                out.push(5);
                put_bytes(out, s.as_bytes());
            }
            Value::Text(s) => {
                out.push(6);
                put_bytes(out, s.as_bytes());
            }
            Value::Bytes(b) => {
                out.push(7);
                put_bytes(out, b);
            }
            Value::Date(d) => {
                out.push(8);
                out.extend_from_slice(&d.num_days_from_ce().to_le_bytes());
            }
            Value::Time(t) => {
                out.push(9);
                out.extend_from_slice(&t.num_seconds_from_midnight().to_le_bytes());
                out.extend_from_slice(&t.nanosecond().to_le_bytes());
            }
            Value::DateTime(dt) => {
                out.push(10);
                out.extend_from_slice(&dt.date().num_days_from_ce().to_le_bytes());
                out.extend_from_slice(&dt.time().num_seconds_from_midnight().to_le_bytes());
                out.extend_from_slice(&dt.time().nanosecond().to_le_bytes());
            }
            Value::DateTimeTz(dt) => {
                out.push(11);
                let local = dt.naive_local();
                out.extend_from_slice(&local.date().num_days_from_ce().to_le_bytes());
                out.extend_from_slice(&local.time().num_seconds_from_midnight().to_le_bytes());
                out.extend_from_slice(&local.time().nanosecond().to_le_bytes());
                out.extend_from_slice(&dt.offset().local_minus_utc().to_le_bytes());
            }
            Value::Uuid(u) => {
                out.push(12);
                out.extend_from_slice(u.as_bytes());
            }
            Value::Json(s) => {
                out.push(13);
                put_bytes(out, s.as_bytes());
            }
            Value::Array(items) => {
                out.push(14);
                out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for v in items.iter() {
                    v.encode(out);
                }
            }
        }
    }

    /// Decode a value previously written by [`Value::encode`], advancing `input`.
    pub fn decode(input: &mut &[u8]) -> Option<Value> {
        fn take<'a>(input: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if input.len() < n {
                return None;
            }
            let (a, b) = input.split_at(n);
            *input = b;
            Some(a)
        }
        fn u32_(input: &mut &[u8]) -> Option<u32> {
            Some(u32::from_le_bytes(take(input, 4)?.try_into().ok()?))
        }
        fn i32_(input: &mut &[u8]) -> Option<i32> {
            Some(i32::from_le_bytes(take(input, 4)?.try_into().ok()?))
        }
        fn bytes<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
            let n = u32_(input)? as usize;
            take(input, n)
        }
        fn string(input: &mut &[u8]) -> Option<Box<str>> {
            Some(String::from_utf8(bytes(input)?.to_vec()).ok()?.into_boxed_str())
        }
        fn date(input: &mut &[u8]) -> Option<NaiveDate> {
            NaiveDate::from_num_days_from_ce_opt(i32_(input)?)
        }
        fn time(input: &mut &[u8]) -> Option<NaiveTime> {
            let secs = u32_(input)?;
            let nanos = u32_(input)?;
            NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)
        }
        let tag = *take(input, 1)?.first()?;
        Some(match tag {
            0 => Value::Null,
            1 => Value::Bool(take(input, 1)?[0] != 0),
            2 => Value::Int(i64::from_le_bytes(take(input, 8)?.try_into().ok()?)),
            3 => Value::UInt(u64::from_le_bytes(take(input, 8)?.try_into().ok()?)),
            4 => Value::Float(f64::from_le_bytes(take(input, 8)?.try_into().ok()?)),
            5 => Value::Decimal(string(input)?),
            6 => Value::Text(string(input)?),
            7 => Value::Bytes(bytes(input)?.to_vec().into_boxed_slice()),
            8 => Value::Date(date(input)?),
            9 => Value::Time(time(input)?),
            10 => {
                let d = date(input)?;
                let t = time(input)?;
                Value::DateTime(NaiveDateTime::new(d, t))
            }
            11 => {
                let d = date(input)?;
                let t = time(input)?;
                let off = FixedOffset::east_opt(i32_(input)?)?;
                let local = NaiveDateTime::new(d, t);
                Value::DateTimeTz(local.and_local_timezone(off).single()?)
            }
            12 => Value::Uuid(Uuid::from_slice(take(input, 16)?).ok()?),
            13 => Value::Json(string(input)?),
            14 => {
                let n = u32_(input)? as usize;
                let mut items = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    items.push(Value::decode(input)?);
                }
                Value::Array(items.into_boxed_slice())
            }
            _ => return None,
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            other => f.write_str(&other.to_text()),
        }
    }
}

pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "f" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

pub fn is_decimal_text(t: &str) -> bool {
    let t = t.strip_prefix(['-', '+']).unwrap_or(t);
    if t.is_empty() {
        return false;
    }
    if t.eq_ignore_ascii_case("nan") || t.eq_ignore_ascii_case("infinity") {
        return true;
    }
    let (mantissa, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let mut dot = false;
    let mut digits = 0;
    for c in mantissa.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' if !dot => dot = true,
            _ => return false,
        }
    }
    if digits == 0 {
        return false;
    }
    match exp {
        None => true,
        Some(e) => {
            let e = e.strip_prefix(['-', '+']).unwrap_or(e);
            !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
        }
    }
}

pub fn parse_date(t: &str) -> Option<NaiveDate> {
    for f in ["%Y-%m-%d", "%Y/%m/%d", "%d.%m.%Y", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, f) {
            return Some(d);
        }
    }
    None
}

pub fn parse_time(t: &str) -> Option<NaiveTime> {
    for f in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M"] {
        if let Ok(v) = NaiveTime::parse_from_str(t, f) {
            return Some(v);
        }
    }
    None
}

pub fn parse_datetime(t: &str) -> Option<NaiveDateTime> {
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y/%m/%d %H:%M:%S%.f", "%d.%m.%Y %H:%M:%S"] {
        if let Ok(v) = NaiveDateTime::parse_from_str(t, f) {
            return Some(v);
        }
    }
    parse_date(t).and_then(|d| d.and_hms_opt(0, 0, 0))
}

pub fn parse_datetime_tz(t: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(v) = DateTime::parse_from_rfc3339(t) {
        return Some(v);
    }
    for f in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z", "%Y-%m-%d %H:%M:%S%#z"] {
        if let Ok(v) = DateTime::parse_from_str(t, f) {
            return Some(v);
        }
    }
    // No offset given: interpret in the local time zone.
    let naive = parse_datetime(t)?;
    let local = naive.and_local_timezone(chrono::Local).earliest()?;
    Some(local.fixed_offset())
}

pub fn parse_hex_bytes(t: &str) -> Option<Vec<u8>> {
    let h =
        t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).or_else(|| t.strip_prefix("\\x")).or_else(|| t.strip_prefix("X'").and_then(|s| s.strip_suffix('\''))).unwrap_or(t);
    let h: String = h.chars().filter(|c| !c.is_whitespace()).collect();
    hex::decode(h).ok()
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value { fn from(v: $t) -> Self { Value::Int(v as i64) } }
    )*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::UInt(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Value::Float(v as f64)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v.into_boxed_str())
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.into())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(v.into_boxed_slice())
    }
}
impl From<NaiveDate> for Value {
    fn from(v: NaiveDate) -> Self {
        Value::Date(v)
    }
}
impl From<NaiveTime> for Value {
    fn from(v: NaiveTime) -> Self {
        Value::Time(v)
    }
}
impl From<NaiveDateTime> for Value {
    fn from(v: NaiveDateTime) -> Self {
        Value::DateTime(v)
    }
}
impl From<DateTime<FixedOffset>> for Value {
    fn from(v: DateTime<FixedOffset>) -> Self {
        Value::DateTimeTz(v)
    }
}
impl From<Uuid> for Value {
    fn from(v: Uuid) -> Self {
        Value::Uuid(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map(Into::into).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_is_small() {
        assert!(std::mem::size_of::<Value>() <= 24, "{}", std::mem::size_of::<Value>());
    }

    #[test]
    fn encode_roundtrip() {
        let dt = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_micro_opt(13, 14, 15, 123456).unwrap();
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-42),
            Value::UInt(u64::MAX),
            Value::Float(3.25),
            Value::decimal("12345678901234567890.123"),
            Value::text("héllo"),
            Value::bytes(vec![0u8, 1, 255]),
            Value::Date(dt.date()),
            Value::Time(dt.time()),
            Value::DateTime(dt),
            Value::DateTimeTz(dt.and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap()),
            Value::Uuid(Uuid::new_v4()),
            Value::json(r#"{"a":1}"#),
            Value::Array(vec![Value::Int(1), Value::Null, Value::text("x")].into_boxed_slice()),
        ];
        let mut buf = Vec::new();
        for v in &values {
            v.encode(&mut buf);
        }
        let mut input = &buf[..];
        for v in &values {
            assert_eq!(&Value::decode(&mut input).unwrap(), v);
        }
        assert!(input.is_empty());
    }

    #[test]
    fn parse_and_text() {
        assert_eq!(Value::parse_as("42", DataKind::Int).unwrap(), Value::Int(42));
        assert_eq!(Value::parse_as("", DataKind::Int).unwrap(), Value::Null);
        assert!(Value::parse_as("x", DataKind::Int).is_err());
        assert_eq!(Value::parse_as("t", DataKind::Bool).unwrap(), Value::Bool(true));
        assert!(Value::parse_as("1.5e3", DataKind::Decimal).is_ok());
        assert!(Value::parse_as("1.5.3", DataKind::Decimal).is_err());
        let v = Value::parse_as("2024-01-02 03:04:05", DataKind::DateTime).unwrap();
        assert_eq!(v.to_text(), "2024-01-02 03:04:05");
        let v = Value::parse_as("2024-01-02 03:04:05.5+07:00", DataKind::DateTimeTz).unwrap();
        assert_eq!(v.to_text(), "2024-01-02 03:04:05.5+07");
        assert_eq!(Value::parse_as("0xff00", DataKind::Binary).unwrap(), Value::bytes(vec![255u8, 0]));
        assert!(Value::parse_as("{bad", DataKind::Json).is_err());
    }

    #[test]
    fn display_options() {
        let o = FormatOptions { thousands_separator: true, ..Default::default() };
        assert_eq!(Value::Int(1234567).display(&o), "1,234,567");
        assert_eq!(Value::Int(-1234).display(&o), "-1,234");
        assert_eq!(Value::decimal("1234.50").display(&o), "1,234.50");
        assert_eq!(Value::Null.display(&o), "[NULL]");
        assert_eq!(Value::text("a\nb").display(&o), "a¶");
    }

    #[test]
    fn sort_order() {
        let mut v = vec![Value::Int(3), Value::Null, Value::Float(1.5), Value::decimal("2")];
        v.sort_by(|a, b| a.cmp_for_sort(b));
        assert_eq!(v, vec![Value::Null, Value::Float(1.5), Value::decimal("2"), Value::Int(3)]);
    }

    #[test]
    fn guess_kinds() {
        assert_eq!(DataKind::guess_from_type_name("varchar(20)"), DataKind::Text);
        assert_eq!(DataKind::guess_from_type_name("timestamp with time zone"), DataKind::DateTimeTz);
        assert_eq!(DataKind::guess_from_type_name("int4[]"), DataKind::Array);
        assert_eq!(DataKind::guess_from_type_name("numeric(10,2)"), DataKind::Decimal);
        assert_eq!(DataKind::guess_from_type_name("boolean"), DataKind::Bool);
        assert_eq!(DataKind::guess_from_type_name("bit(1)"), DataKind::Bool);
        assert_eq!(DataKind::guess_from_type_name("bit(8)"), DataKind::Bit);
    }
}
