//! ClickHouse type names: parsing into a type tree, mapping to DBiji
//! [`DataKind`]s and converting `JSONCompactEachRow` values into [`Value`]s.

use crate::json::Json;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use dbiji_core::value::{ColumnMeta, DataKind, Value, parse_datetime_tz};

/// Geometry type aliases of ClickHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoKind {
    /// `Point` = `Tuple(Float64, Float64)`.
    Point,
    /// `Ring` = `Array(Point)` (closed polygon without holes).
    Ring,
    /// `LineString` = `Array(Point)`.
    LineString,
    /// `MultiLineString` = `Array(LineString)`.
    MultiLineString,
    /// `Polygon` = `Array(Ring)`.
    Polygon,
    /// `MultiPolygon` = `Array(Polygon)`.
    MultiPolygon,
}

/// Parsed ClickHouse data type.
#[derive(Debug, Clone, PartialEq)]
pub enum ChType {
    /// `Bool` / `Boolean`.
    Bool,
    /// `Int8`..`Int256`, `UInt8`..`UInt256`.
    Int {
        /// Width in bits.
        bits: u16,
        /// Signed type.
        signed: bool,
    },
    /// `Float32`, `Float64`, `BFloat16`.
    Float(u16),
    /// `Decimal(P, S)` and the `DecimalNN(S)` aliases.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the decimal point.
        scale: u32,
    },
    /// `String`.
    String,
    /// `FixedString(N)`.
    FixedString(u32),
    /// `UUID`.
    Uuid,
    /// `Date` (1970..2149).
    Date,
    /// `Date32` (1900..2299).
    Date32,
    /// `DateTime[(tz)]`.
    DateTime(Option<String>),
    /// `DateTime64(p[, tz])`.
    DateTime64 {
        /// Sub-second digits.
        precision: u32,
        /// Explicit time zone.
        tz: Option<String>,
    },
    /// `Time`.
    Time,
    /// `Time64(p)`.
    Time64(u32),
    /// `IPv4`.
    Ipv4,
    /// `IPv6`.
    Ipv6,
    /// `Enum8(...)` / `Enum16(...)`.
    Enum(Vec<(String, i64)>),
    /// `Array(T)`.
    Array(Box<ChType>),
    /// `Tuple(T1, name T2, ...)`.
    Tuple(Vec<(Option<String>, ChType)>),
    /// `Map(K, V)`.
    Map(Box<ChType>, Box<ChType>),
    /// `Nullable(T)`.
    Nullable(Box<ChType>),
    /// `LowCardinality(T)`.
    LowCardinality(Box<ChType>),
    /// `SimpleAggregateFunction(f, T)`: values are plain `T`.
    SimpleAggregate(Box<ChType>),
    /// `AggregateFunction(...)`: opaque binary states.
    AggregateState,
    /// `Nested(name T, ...)`.
    Nested(Vec<(String, ChType)>),
    /// `JSON`, `Object('json')`.
    Json,
    /// `Variant(...)`.
    Variant(Vec<ChType>),
    /// `Dynamic`.
    Dynamic,
    /// `Interval*`.
    Interval,
    /// `Nothing` (type of a bare `NULL`).
    Nothing,
    /// Geometry aliases.
    Geo(GeoKind),
    /// Anything else, kept verbatim.
    Other(String),
}

// ------------------------------------------------------------------ parsing

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(String),
    Str(String),
    LParen,
    RParen,
    Comma,
    Eq,
    Other(char),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if c == ',' {
            out.push(Tok::Comma);
            i += 1;
        } else if c == '=' {
            out.push(Tok::Eq);
            i += 1;
        } else if c == '\'' {
            let mut v = String::new();
            i += 1;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    v.push(chars[i + 1]);
                    i += 2;
                } else if ch == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        v.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    v.push(ch);
                    i += 1;
                }
            }
            out.push(Tok::Str(v));
        } else if c == '`' || c == '"' {
            let q = c;
            let mut v = String::new();
            i += 1;
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                v.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Ident(v));
        } else if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(chars[start..i].iter().collect()));
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            out.push(Tok::Other(c));
            i += 1;
        }
    }
    out
}

struct TypeParser {
    toks: Vec<Tok>,
    pos: usize,
}

impl TypeParser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    /// Skip one argument (balanced parentheses) and return nothing.
    fn skip_arg(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.peek() {
            match t {
                Tok::LParen => depth += 1,
                Tok::RParen if depth == 0 => return,
                Tok::RParen => depth -= 1,
                Tok::Comma if depth == 0 => return,
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Parse `T` or `name T` (named tuple / nested element).
    fn named_elem(&mut self) -> (Option<String>, ChType) {
        if let (Some(Tok::Ident(a)), Some(Tok::Ident(_))) = (self.toks.get(self.pos), self.toks.get(self.pos + 1)) {
            let name = a.clone();
            self.pos += 1;
            return (Some(name), self.ty());
        }
        (None, self.ty())
    }

    fn args<T>(&mut self, mut f: impl FnMut(&mut Self) -> T) -> Vec<T> {
        let mut out = Vec::new();
        if self.peek() != Some(&Tok::LParen) {
            return out;
        }
        self.pos += 1;
        if self.peek() == Some(&Tok::RParen) {
            self.pos += 1;
            return out;
        }
        loop {
            let before = self.pos;
            out.push(f(self));
            // make sure we always advance to the next separator
            if self.pos == before {
                self.skip_arg();
            }
            match self.next() {
                Some(Tok::Comma) => continue,
                Some(Tok::RParen) | None => break,
                Some(_) => {
                    // unexpected token: resync to the end of this argument
                    self.skip_arg();
                    match self.next() {
                        Some(Tok::Comma) => continue,
                        _ => break,
                    }
                }
            }
        }
        out
    }

    fn num_arg(&mut self) -> Option<u32> {
        match self.next() {
            Some(Tok::Num(n)) => n.parse().ok(),
            _ => None,
        }
    }

    fn ty(&mut self) -> ChType {
        let Some(Tok::Ident(name)) = self.next() else {
            return ChType::Other(String::new());
        };
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "nullable" => {
                let mut a = self.args(|p| p.ty());
                ChType::Nullable(Box::new(a.pop().unwrap_or(ChType::Nothing)))
            }
            "lowcardinality" => {
                let mut a = self.args(|p| p.ty());
                ChType::LowCardinality(Box::new(a.pop().unwrap_or(ChType::String)))
            }
            "array" => {
                let mut a = self.args(|p| p.ty());
                ChType::Array(Box::new(a.pop().unwrap_or(ChType::Nothing)))
            }
            "map" => {
                let mut a = self.args(|p| p.ty()).into_iter();
                let k = a.next().unwrap_or(ChType::String);
                let v = a.next().unwrap_or(ChType::String);
                ChType::Map(Box::new(k), Box::new(v))
            }
            "tuple" => ChType::Tuple(self.args(|p| p.named_elem())),
            "nested" => ChType::Nested(self.args(|p| p.named_elem()).into_iter().map(|(n, t)| (n.unwrap_or_default(), t)).collect()),
            "variant" => ChType::Variant(self.args(|p| p.ty())),
            "dynamic" => {
                self.args(|p| p.skip_arg());
                ChType::Dynamic
            }
            "json" | "object" => {
                self.args(|p| p.skip_arg());
                ChType::Json
            }
            "simpleaggregatefunction" => {
                // first argument: the (possibly parametric) function name
                let mut index = 0usize;
                let mut a = self.args(|p| {
                    index += 1;
                    if index == 1 {
                        p.skip_arg();
                        None
                    } else {
                        Some(p.ty())
                    }
                });
                let inner = a.pop().flatten().unwrap_or(ChType::Other(String::new()));
                ChType::SimpleAggregate(Box::new(inner))
            }
            "aggregatefunction" => {
                self.args(|p| p.skip_arg());
                ChType::AggregateState
            }
            "enum8" | "enum16" | "enum" => {
                let vals = self.args(|p| {
                    let label = match p.next() {
                        Some(Tok::Str(s)) => s,
                        _ => String::new(),
                    };
                    let mut v = 0i64;
                    if p.peek() == Some(&Tok::Eq) {
                        p.pos += 1;
                        if let Some(Tok::Num(n)) = p.next() {
                            v = n.parse().unwrap_or(0);
                        }
                    }
                    (label, v)
                });
                ChType::Enum(vals)
            }
            "decimal" | "numeric" | "dec" => {
                let a = self.args(|p| p.num_arg());
                let precision = a.first().copied().flatten().unwrap_or(10);
                let scale = a.get(1).copied().flatten().unwrap_or(0);
                ChType::Decimal { precision, scale }
            }
            "decimal32" | "decimal64" | "decimal128" | "decimal256" => {
                let a = self.args(|p| p.num_arg());
                let precision = match lower.as_str() {
                    "decimal32" => 9,
                    "decimal64" => 18,
                    "decimal128" => 38,
                    _ => 76,
                };
                ChType::Decimal { precision, scale: a.first().copied().flatten().unwrap_or(0) }
            }
            "datetime" | "timestamp" => {
                let a = self.args(|p| match p.next() {
                    Some(Tok::Str(s)) => Some(s),
                    _ => None,
                });
                ChType::DateTime(a.into_iter().flatten().next())
            }
            "datetime64" => {
                let mut precision = 3;
                let mut tz = None;
                for (i, a) in self
                    .args(|p| match p.next() {
                        Some(Tok::Num(n)) => (n.parse::<u32>().ok(), None),
                        Some(Tok::Str(s)) => (None, Some(s)),
                        _ => (None, None),
                    })
                    .into_iter()
                    .enumerate()
                {
                    match a {
                        (Some(n), _) if i == 0 => precision = n,
                        (_, Some(s)) => tz = Some(s),
                        _ => {}
                    }
                }
                ChType::DateTime64 { precision, tz }
            }
            "time64" => {
                let a = self.args(|p| p.num_arg());
                ChType::Time64(a.first().copied().flatten().unwrap_or(3))
            }
            "fixedstring" => {
                let a = self.args(|p| p.num_arg());
                ChType::FixedString(a.first().copied().flatten().unwrap_or(0))
            }
            _ => {
                // Unknown parametric types: skip their arguments.
                let had_args = self.peek() == Some(&Tok::LParen);
                if had_args {
                    self.args(|p| p.skip_arg());
                }
                simple_type(&name).unwrap_or(ChType::Other(name))
            }
        }
    }
}

fn simple_type(name: &str) -> Option<ChType> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "bool" | "boolean" => ChType::Bool,
        "int8" | "tinyint" | "int1" | "byte" => ChType::Int { bits: 8, signed: true },
        "int16" | "smallint" => ChType::Int { bits: 16, signed: true },
        "int32" | "int" | "integer" | "mediumint" => ChType::Int { bits: 32, signed: true },
        "int64" | "bigint" => ChType::Int { bits: 64, signed: true },
        "int128" => ChType::Int { bits: 128, signed: true },
        "int256" => ChType::Int { bits: 256, signed: true },
        "uint8" => ChType::Int { bits: 8, signed: false },
        "uint16" => ChType::Int { bits: 16, signed: false },
        "uint32" => ChType::Int { bits: 32, signed: false },
        "uint64" => ChType::Int { bits: 64, signed: false },
        "uint128" => ChType::Int { bits: 128, signed: false },
        "uint256" => ChType::Int { bits: 256, signed: false },
        "float32" | "float" | "real" | "single" => ChType::Float(32),
        "float64" | "double" => ChType::Float(64),
        "bfloat16" => ChType::Float(16),
        "string" | "text" | "varchar" | "char" | "blob" | "longtext" | "longblob" | "mediumtext" | "tinytext" | "bytea" | "varbinary" | "binary" | "clob" => ChType::String,
        "uuid" => ChType::Uuid,
        "date" => ChType::Date,
        "date32" => ChType::Date32,
        "time" => ChType::Time,
        "ipv4" => ChType::Ipv4,
        "ipv6" => ChType::Ipv6,
        "nothing" => ChType::Nothing,
        "point" => ChType::Geo(GeoKind::Point),
        "ring" => ChType::Geo(GeoKind::Ring),
        "linestring" => ChType::Geo(GeoKind::LineString),
        "multilinestring" => ChType::Geo(GeoKind::MultiLineString),
        "polygon" => ChType::Geo(GeoKind::Polygon),
        "multipolygon" => ChType::Geo(GeoKind::MultiPolygon),
        _ if lower.starts_with("interval") => ChType::Interval,
        _ => return None,
    })
}

/// Parse a ClickHouse type name. Never fails: unknown types become [`ChType::Other`].
pub fn parse_type(type_name: &str) -> ChType {
    let toks = tokenize(type_name.trim());
    if toks.is_empty() {
        return ChType::Other(type_name.to_string());
    }
    let mut p = TypeParser { toks, pos: 0 };
    let t = p.ty();
    if let ChType::Other(s) = &t
        && s.is_empty()
    {
        return ChType::Other(type_name.to_string());
    }
    t
}

// ------------------------------------------------------------ type queries

impl ChType {
    /// The type with `Nullable`, `LowCardinality` and
    /// `SimpleAggregateFunction` wrappers removed.
    pub fn base(&self) -> &ChType {
        match self {
            ChType::Nullable(t) | ChType::LowCardinality(t) | ChType::SimpleAggregate(t) => t.base(),
            other => other,
        }
    }

    /// The column accepts NULL values.
    pub fn is_nullable(&self) -> bool {
        match self {
            ChType::Nullable(_) | ChType::Nothing => true,
            ChType::LowCardinality(t) | ChType::SimpleAggregate(t) => t.is_nullable(),
            _ => false,
        }
    }

    /// Logical kind used by the grid and data transfer.
    pub fn kind(&self) -> DataKind {
        match self.base() {
            ChType::Bool => DataKind::Bool,
            ChType::Int { .. } | ChType::Interval => DataKind::Int,
            ChType::Float(_) => DataKind::Float,
            ChType::Decimal { .. } => DataKind::Decimal,
            ChType::String | ChType::FixedString(_) => DataKind::Text,
            ChType::Uuid => DataKind::Uuid,
            ChType::Date | ChType::Date32 => DataKind::Date,
            ChType::DateTime(_) | ChType::DateTime64 { .. } => DataKind::DateTime,
            ChType::Time | ChType::Time64(_) => DataKind::Time,
            ChType::Ipv4 | ChType::Ipv6 => DataKind::Network,
            ChType::Enum(_) => DataKind::Enum,
            ChType::Array(_) => DataKind::Array,
            ChType::Tuple(_) | ChType::Map(..) | ChType::Nested(_) | ChType::Json | ChType::Variant(_) | ChType::Dynamic => DataKind::Json,
            ChType::Geo(_) => DataKind::Geometry,
            ChType::AggregateState | ChType::Nothing => DataKind::Other,
            ChType::Other(_) => DataKind::Other,
            ChType::Nullable(_) | ChType::LowCardinality(_) | ChType::SimpleAggregate(_) => DataKind::Other,
        }
    }
}

/// Logical kind of a ClickHouse type name.
pub fn kind_of(type_name: &str) -> DataKind {
    match parse_type(type_name) {
        ChType::Other(_) => DataKind::guess_from_type_name(type_name),
        t => t.kind(),
    }
}

/// Result column description for a column `name` of ClickHouse type `type_name`.
pub fn column_meta(name: &str, type_name: &str, ty: &ChType) -> ColumnMeta {
    let mut m = ColumnMeta::new(name, type_name, ty.kind());
    m.nullable = Some(ty.is_nullable());
    match ty.base() {
        ChType::Decimal { precision, scale } => {
            m.precision = Some(*precision);
            m.scale = Some(*scale);
        }
        ChType::DateTime64 { precision, .. } | ChType::Time64(precision) => m.scale = Some(*precision),
        ChType::FixedString(n) => m.max_length = Some(*n),
        ChType::Int { bits, .. } => m.precision = Some(int_digits(*bits)),
        _ => {}
    }
    m
}

fn int_digits(bits: u16) -> u32 {
    match bits {
        8 => 3,
        16 => 5,
        32 => 10,
        64 => 20,
        128 => 39,
        _ => 78,
    }
}

/// Strip one `Nullable(...)` wrapper (also inside `LowCardinality`) from a
/// type name: returns the inner type text and whether it was nullable.
pub fn strip_nullable(type_name: &str) -> (String, bool) {
    let t = type_name.trim();
    if let Some(inner) = strip_wrapper(t, "Nullable") {
        return (inner.to_string(), true);
    }
    if let Some(inner) = strip_wrapper(t, "LowCardinality")
        && let Some(innermost) = strip_wrapper(inner, "Nullable")
    {
        return (format!("LowCardinality({innermost})"), true);
    }
    (t.to_string(), false)
}

/// `Name(inner)` → `inner` when the outer parentheses enclose everything.
pub fn strip_wrapper<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let t = t.trim();
    // byte comparison: a match means the prefix is ASCII, so the split
    // point is a char boundary
    let prefix_matches = t.as_bytes().get(..name.len()).is_some_and(|p| p.eq_ignore_ascii_case(name.as_bytes()));
    if t.len() <= name.len() + 2 || !prefix_matches {
        return None;
    }
    let rest = t.get(name.len()..)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    // verify the parentheses are balanced (so `Nullable(A)(B)` is rejected)
    let mut depth = 0i32;
    let mut in_str = false;
    for c in inner.chars() {
        match c {
            '\'' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    (depth == 0).then_some(inner.trim())
}

/// Wrap `type_name` in `Nullable(...)` where ClickHouse allows it
/// (`LowCardinality(T)` becomes `LowCardinality(Nullable(T))`; composite types
/// that cannot be nullable are returned unchanged).
pub fn make_nullable(type_name: &str) -> String {
    let t = type_name.trim();
    match parse_type(t) {
        ChType::Nullable(_)
        | ChType::Array(_)
        | ChType::Map(..)
        | ChType::Tuple(_)
        | ChType::Nested(_)
        | ChType::Json
        | ChType::Variant(_)
        | ChType::Dynamic
        | ChType::AggregateState
        | ChType::Geo(_) => t.to_string(),
        ChType::LowCardinality(inner) if !matches!(*inner, ChType::Nullable(_)) => match strip_wrapper(t, "LowCardinality") {
            Some(i) => format!("LowCardinality(Nullable({i}))"),
            None => t.to_string(),
        },
        ChType::LowCardinality(_) => t.to_string(),
        _ => format!("Nullable({t})"),
    }
}

// ------------------------------------------------------------ value mapping

/// Convert one JSON cell of a column of type `ty` into a [`Value`].
pub fn json_to_value(ty: &ChType, j: Json) -> Value {
    if matches!(j, Json::Null) {
        return Value::Null;
    }
    match ty.base() {
        ChType::Bool => match &j {
            Json::Bool(b) => Value::Bool(*b),
            other => match other.scalar_text() {
                Some("1") | Some("true") => Value::Bool(true),
                Some("0") | Some("false") => Value::Bool(false),
                _ => fallback(j),
            },
        },
        ChType::Int { signed, .. } => match j.scalar_text() {
            Some(t) => int_value(t, *signed),
            None => fallback(j),
        },
        ChType::Interval => match j.scalar_text() {
            Some(t) => int_value(t, true),
            None => fallback(j),
        },
        ChType::Float(_) => match j.scalar_text() {
            Some(t) => match parse_float(t) {
                Some(f) => Value::Float(f),
                None => Value::text(t),
            },
            None => fallback(j),
        },
        ChType::Decimal { .. } => match j.scalar_text() {
            Some(t) => Value::decimal(t),
            None => fallback(j),
        },
        ChType::String | ChType::FixedString(_) | ChType::Enum(_) | ChType::Ipv4 | ChType::Ipv6 => match j {
            Json::String(s) => Value::Text(s.into_boxed_str()),
            Json::Bytes(b) => Value::Bytes(b.into_boxed_slice()),
            other => fallback(other),
        },
        ChType::Uuid => match &j {
            Json::String(s) => match uuid::Uuid::parse_str(s) {
                Ok(u) => Value::Uuid(u),
                Err(_) => Value::text(s.as_str()),
            },
            _ => fallback(j),
        },
        ChType::Date | ChType::Date32 => match &j {
            Json::String(s) => match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                Ok(d) => Value::Date(d),
                Err(_) => Value::text(s.as_str()),
            },
            _ => fallback(j),
        },
        ChType::DateTime(_) | ChType::DateTime64 { .. } => match &j {
            Json::String(s) => datetime_value(s),
            // unix_timestamp output format
            Json::Number(n) => Value::text(n.as_str()),
            _ => fallback(j),
        },
        ChType::Time | ChType::Time64(_) => match &j {
            Json::String(s) => match NaiveTime::parse_from_str(s, "%H:%M:%S%.f") {
                Ok(t) => Value::Time(t),
                Err(_) => Value::text(s.as_str()),
            },
            _ => fallback(j),
        },
        ChType::Array(inner) => match j {
            Json::Array(items) => Value::Array(items.into_iter().map(|v| json_to_value(inner, v)).collect::<Vec<_>>().into_boxed_slice()),
            other => fallback(other),
        },
        ChType::Geo(kind) => match geo_wkt(*kind, &j) {
            Some(wkt) => Value::Text(wkt.into_boxed_str()),
            None => fallback(j),
        },
        ChType::Tuple(_) | ChType::Map(..) | ChType::Nested(_) | ChType::Json => match j {
            Json::String(s) => Value::Json(Json::String(s).to_json_string().into_boxed_str()),
            other => Value::Json(other.to_json_string().into_boxed_str()),
        },
        ChType::Variant(_) | ChType::Dynamic => match j {
            Json::String(s) => Value::Text(s.into_boxed_str()),
            Json::Number(n) => Value::Decimal(n.into_boxed_str()),
            Json::Bool(b) => Value::Bool(b),
            other => Value::Json(other.to_json_string().into_boxed_str()),
        },
        ChType::Nothing => Value::Null,
        ChType::AggregateState | ChType::Other(_) => fallback(j),
        ChType::Nullable(_) | ChType::LowCardinality(_) | ChType::SimpleAggregate(_) => fallback(j),
    }
}

/// Generic conversion when the JSON shape does not match the declared type.
fn fallback(j: Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(b),
        Json::Number(n) => Value::Decimal(n.into_boxed_str()),
        Json::String(s) => Value::Text(s.into_boxed_str()),
        Json::Bytes(b) => Value::Bytes(b.into_boxed_slice()),
        other => Value::Json(other.to_json_string().into_boxed_str()),
    }
}

fn int_value(t: &str, signed: bool) -> Value {
    let t = t.trim();
    if signed {
        if let Ok(i) = t.parse::<i64>() {
            return Value::Int(i);
        }
    } else if let Ok(u) = t.parse::<u64>() {
        // keep small unsigned values as Int so they sort/compare like other ints
        return match i64::try_from(u) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::UInt(u),
        };
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(u) = t.parse::<u64>() {
        return Value::UInt(u);
    }
    // 128/256-bit values: exact text
    Value::decimal(t)
}

fn parse_float(t: &str) -> Option<f64> {
    match t.trim().to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

fn datetime_value(s: &str) -> Value {
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return Value::DateTime(dt);
        }
    }
    // `date_time_output_format = 'iso'` produces `2024-01-02T03:04:05Z`.
    if let Some(tz) = parse_datetime_tz(s).filter(|_| s.contains('T') || s.ends_with('Z')) {
        return Value::DateTimeTz(tz);
    }
    Value::text(s)
}

fn point_text(j: &Json) -> Option<String> {
    let Json::Array(xy) = j else { return None };
    if xy.len() != 2 {
        return None;
    }
    Some(format!("{} {}", xy[0].scalar_text()?, xy[1].scalar_text()?))
}

fn points_text(j: &Json) -> Option<String> {
    let Json::Array(pts) = j else { return None };
    let parts: Option<Vec<String>> = pts.iter().map(point_text).collect();
    Some(format!("({})", parts?.join(", ")))
}

fn rings_text(j: &Json) -> Option<String> {
    let Json::Array(rings) = j else { return None };
    let parts: Option<Vec<String>> = rings.iter().map(points_text).collect();
    Some(format!("({})", parts?.join(", ")))
}

/// Render ClickHouse geometry values as WKT.
fn geo_wkt(kind: GeoKind, j: &Json) -> Option<String> {
    Some(match kind {
        GeoKind::Point => format!("POINT({})", point_text(j)?),
        GeoKind::LineString => format!("LINESTRING{}", points_text(j)?),
        GeoKind::Ring => format!("POLYGON({})", points_text(j)?),
        GeoKind::MultiLineString => format!("MULTILINESTRING{}", rings_text(j)?),
        GeoKind::Polygon => format!("POLYGON{}", rings_text(j)?),
        GeoKind::MultiPolygon => {
            let Json::Array(polys) = j else { return None };
            let parts: Option<Vec<String>> = polys.iter().map(rings_text).collect();
            format!("MULTIPOLYGON({})", parts?.join(", "))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn parses_simple_and_nested_types() {
        assert_eq!(parse_type("UInt64"), ChType::Int { bits: 64, signed: false });
        assert_eq!(parse_type("Decimal(18, 3)"), ChType::Decimal { precision: 18, scale: 3 });
        assert_eq!(parse_type("Decimal64(4)"), ChType::Decimal { precision: 18, scale: 4 });
        assert_eq!(parse_type("DateTime64(6, 'Asia/Jakarta')"), ChType::DateTime64 { precision: 6, tz: Some("Asia/Jakarta".into()) });
        assert_eq!(parse_type("DateTime('UTC')"), ChType::DateTime(Some("UTC".into())));
        assert_eq!(parse_type("DateTime"), ChType::DateTime(None));
        assert_eq!(parse_type("Array(Nullable(Int32))"), ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int { bits: 32, signed: true })))));
        assert_eq!(parse_type("Map(String, Array(UInt8))"), ChType::Map(Box::new(ChType::String), Box::new(ChType::Array(Box::new(ChType::Int { bits: 8, signed: false })))));
        assert_eq!(parse_type("Tuple(a UInt8, b String)"), ChType::Tuple(vec![(Some("a".into()), ChType::Int { bits: 8, signed: false }), (Some("b".into()), ChType::String)]));
        assert_eq!(parse_type("Tuple(UInt8, String)"), ChType::Tuple(vec![(None, ChType::Int { bits: 8, signed: false }), (None, ChType::String)]));
        assert_eq!(parse_type("Enum8('a' = 1, 'it''s' = -2)"), ChType::Enum(vec![("a".into(), 1), ("it's".into(), -2)]));
        assert_eq!(parse_type("LowCardinality(Nullable(String))"), ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))));
        assert_eq!(parse_type("FixedString(16)"), ChType::FixedString(16));
        assert_eq!(parse_type("IntervalDay"), ChType::Interval);
        assert_eq!(parse_type("Point"), ChType::Geo(GeoKind::Point));
        assert_eq!(parse_type("JSON(max_dynamic_paths=10, a.b UInt32)"), ChType::Json);
        assert_eq!(parse_type("Object('json')"), ChType::Json);
        assert_eq!(parse_type("AggregateFunction(uniq, UInt64)"), ChType::AggregateState);
        assert_eq!(parse_type("SimpleAggregateFunction(sum, UInt64)"), ChType::SimpleAggregate(Box::new(ChType::Int { bits: 64, signed: false })));
        assert_eq!(parse_type("SimpleAggregateFunction(anyLast, Nullable(String))").kind(), DataKind::Text);
        assert_eq!(parse_type("Nested(a UInt8, b String)").kind(), DataKind::Json);
        assert_eq!(parse_type("Variant(String, UInt64)").kind(), DataKind::Json);
        assert_eq!(parse_type("Dynamic(max_types=8)"), ChType::Dynamic);
        assert!(matches!(parse_type("SomethingNew(1, 'x')"), ChType::Other(_)));
        assert!(matches!(parse_type(""), ChType::Other(_)));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("Nullable(Float32)"), DataKind::Float);
        assert_eq!(kind_of("Bool"), DataKind::Bool);
        assert_eq!(kind_of("UUID"), DataKind::Uuid);
        assert_eq!(kind_of("Date32"), DataKind::Date);
        assert_eq!(kind_of("DateTime64(3)"), DataKind::DateTime);
        assert_eq!(kind_of("IPv6"), DataKind::Network);
        assert_eq!(kind_of("Enum16('x' = 1)"), DataKind::Enum);
        assert_eq!(kind_of("Array(String)"), DataKind::Array);
        assert_eq!(kind_of("Map(String, UInt8)"), DataKind::Json);
        assert_eq!(kind_of("Polygon"), DataKind::Geometry);
        assert_eq!(kind_of("Time64(3)"), DataKind::Time);
    }

    #[test]
    fn nullable_helpers() {
        assert_eq!(strip_nullable("Nullable(String)"), ("String".to_string(), true));
        assert_eq!(strip_nullable("LowCardinality(Nullable(String))"), ("LowCardinality(String)".to_string(), true));
        assert_eq!(strip_nullable("Array(Nullable(Int8))"), ("Array(Nullable(Int8))".to_string(), false));
        assert_eq!(make_nullable("String"), "Nullable(String)");
        assert_eq!(make_nullable("LowCardinality(String)"), "LowCardinality(Nullable(String))");
        assert_eq!(make_nullable("Array(UInt8)"), "Array(UInt8)");
        assert_eq!(make_nullable("Nullable(UInt8)"), "Nullable(UInt8)");
        assert_eq!(strip_wrapper("Nullable(A)(B)", "Nullable"), None);
    }

    fn conv(ty: &str, json_text: &str) -> Value {
        json_to_value(&parse_type(ty), json::parse(json_text.as_bytes()).unwrap())
    }

    #[test]
    fn converts_values() {
        assert_eq!(conv("UInt64", "\"18446744073709551615\""), Value::UInt(u64::MAX));
        assert_eq!(conv("UInt64", "42"), Value::Int(42));
        assert_eq!(conv("Int64", "\"-9223372036854775808\""), Value::Int(i64::MIN));
        assert_eq!(
            conv("Int256", "\"-57896044618658097711785492504343953926634992332820282019728792003956564819968\""),
            Value::decimal("-57896044618658097711785492504343953926634992332820282019728792003956564819968")
        );
        assert_eq!(conv("Float64", "\"nan\"").as_f64().map(f64::is_nan), Some(true));
        assert_eq!(conv("Float32", "\"-inf\""), Value::Float(f64::NEG_INFINITY));
        assert_eq!(conv("Float64", "1.5"), Value::Float(1.5));
        assert_eq!(conv("Decimal(18, 3)", "\"12.340\""), Value::decimal("12.340"));
        assert_eq!(conv("Decimal(38, 2)", "123456789012345678901234567890.12"), Value::decimal("123456789012345678901234567890.12"));
        assert_eq!(conv("Bool", "true"), Value::Bool(true));
        assert_eq!(conv("Nullable(String)", "null"), Value::Null);
        assert_eq!(conv("String", "\"a\\nb\""), Value::text("a\nb"));
        assert_eq!(conv("UUID", "\"61f0c404-5cb3-11e7-907b-a6006ad3dba0\""), Value::Uuid(uuid::Uuid::parse_str("61f0c404-5cb3-11e7-907b-a6006ad3dba0").unwrap()));
        assert_eq!(conv("Date", "\"2020-02-02\""), Value::Date(NaiveDate::from_ymd_opt(2020, 2, 2).unwrap()));
        assert_eq!(
            conv("DateTime64(6, 'Asia/Jakarta')", "\"2024-01-02 03:04:05.123456\""),
            Value::DateTime(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_micro_opt(3, 4, 5, 123_456).unwrap())
        );
        assert!(matches!(conv("DateTime", "\"2024-01-01T20:04:05Z\""), Value::DateTimeTz(_)));
        assert_eq!(conv("Time64(3)", "\"12:00:00.123\""), Value::Time(NaiveTime::from_hms_milli_opt(12, 0, 0, 123).unwrap()));
        assert_eq!(conv("Time", "\"-01:00:00\""), Value::text("-01:00:00"));
        assert_eq!(conv("Array(Nullable(Int32))", "[null, 1]"), Value::Array(vec![Value::Null, Value::Int(1)].into_boxed_slice()));
        assert_eq!(conv("Map(String, UInt8)", "{\"a\":1}"), Value::json("{\"a\":1}"));
        assert_eq!(conv("Tuple(a UInt8, b String)", "{\"a\":1,\"b\":\"x\"}"), Value::json("{\"a\":1,\"b\":\"x\"}"));
        assert_eq!(conv("Point", "[1.5,2.5]"), Value::text("POINT(1.5 2.5)"));
        assert_eq!(conv("Polygon", "[[[0,0],[1,0],[1,1]]]"), Value::text("POLYGON((0 0, 1 0, 1 1))"));
        assert_eq!(conv("MultiPolygon", "[[[[0,0],[1,0],[1,1]]],[[[5,5],[6,5],[6,6]]]]"), Value::text("MULTIPOLYGON(((0 0, 1 0, 1 1)), ((5 5, 6 5, 6 6)))"));
        assert_eq!(conv("LowCardinality(Nullable(String))", "\"lc\""), Value::text("lc"));
        assert_eq!(conv("Enum8('a' = 1)", "\"a\""), Value::text("a"));
        assert_eq!(conv("IntervalDay", "\"3\""), Value::Int(3));
        assert_eq!(conv("Nothing", "null"), Value::Null);
        assert_eq!(conv("Dynamic", "\"x\""), Value::text("x"));
        assert_eq!(conv("Dynamic", "[1,2]"), Value::json("[1,2]"));
        assert_eq!(conv("Weird", "5"), Value::decimal("5"));
    }

    #[test]
    fn meta_details() {
        let t = parse_type("Nullable(Decimal(10, 2))");
        let m = column_meta("d", "Nullable(Decimal(10, 2))", &t);
        assert_eq!(m.kind, DataKind::Decimal);
        assert_eq!(m.nullable, Some(true));
        assert_eq!((m.precision, m.scale), (Some(10), Some(2)));
        let m = column_meta("f", "FixedString(4)", &parse_type("FixedString(4)"));
        assert_eq!(m.max_length, Some(4));
        assert_eq!(m.nullable, Some(false));
    }
}
