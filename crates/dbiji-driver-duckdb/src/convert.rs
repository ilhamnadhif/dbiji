//! Conversion between DuckDB values and DBiji [`Value`]s.
//!
//! * integers → `Int` (`UBIGINT` → `UInt`; `HUGEINT` / `UHUGEINT` outside
//!   64 bits → `Decimal` text), `FLOAT` / `DOUBLE` → `Float`,
//!   `DECIMAL` / `BIGNUM` → `Decimal`
//! * `DATE`, `TIME`, `TIMESTAMP[_S|_MS|_NS]` → `Date` / `Time` / `DateTime`,
//!   `TIMESTAMP WITH TIME ZONE` → `DateTimeTz` (UTC); infinite values → text
//! * `INTERVAL` → text (`1 year 2 months 3 days 04:05:06`), `UUID` → `Uuid`,
//!   `JSON` → `Json`, `ENUM` → text, `BLOB` / `GEOMETRY` → `Bytes`
//! * `LIST` / `ARRAY` / `STRUCT` / `MAP` / `UNION` → JSON text (`Json` values;
//!   column kind `Array` for lists and arrays, `Json` otherwise)

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbiji_core::value::{DataKind, Value};
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use duckdb::types::{Decimal, TimeUnit, ToSql, ToSqlOutput, Value as DValue, ValueRef};
use serde_json::Value as J;

/// Per column conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conv {
    Plain,
    Json,
    Uuid,
    TimestampTz,
    Nested,
}

/// Logical kind of a DuckDB type.
pub(crate) fn kind_of(t: &LogicalTypeHandle) -> DataKind {
    if t.get_alias()
        .is_some_and(|a| a.eq_ignore_ascii_case("JSON"))
    {
        return DataKind::Json;
    }
    match t.id() {
        LogicalTypeId::Boolean => DataKind::Bool,
        LogicalTypeId::Tinyint
        | LogicalTypeId::Smallint
        | LogicalTypeId::Integer
        | LogicalTypeId::Bigint
        | LogicalTypeId::UTinyint
        | LogicalTypeId::USmallint
        | LogicalTypeId::UInteger
        | LogicalTypeId::UBigint
        | LogicalTypeId::Hugeint
        | LogicalTypeId::UHugeint => DataKind::Int,
        LogicalTypeId::Float | LogicalTypeId::Double => DataKind::Float,
        LogicalTypeId::Decimal | LogicalTypeId::Bignum => DataKind::Decimal,
        LogicalTypeId::Varchar | LogicalTypeId::StringLiteral => DataKind::Text,
        LogicalTypeId::Blob => DataKind::Binary,
        LogicalTypeId::Bit => DataKind::Bit,
        LogicalTypeId::Date => DataKind::Date,
        LogicalTypeId::Time | LogicalTypeId::TimeNs => DataKind::Time,
        LogicalTypeId::TimeTZ => DataKind::TimeTz,
        LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampS
        | LogicalTypeId::TimestampMs
        | LogicalTypeId::TimestampNs => DataKind::DateTime,
        LogicalTypeId::TimestampTZ => DataKind::DateTimeTz,
        LogicalTypeId::Interval => DataKind::Interval,
        LogicalTypeId::Uuid => DataKind::Uuid,
        LogicalTypeId::List | LogicalTypeId::Array => DataKind::Array,
        LogicalTypeId::Struct | LogicalTypeId::Map | LogicalTypeId::Union => DataKind::Json,
        LogicalTypeId::Enum => DataKind::Enum,
        LogicalTypeId::Geometry => DataKind::Geometry,
        LogicalTypeId::IntegerLiteral => DataKind::Int,
        _ => DataKind::Other,
    }
}

/// Conversion for a column of type `t`.
pub(crate) fn conv_of(t: &LogicalTypeHandle) -> Conv {
    match kind_of(t) {
        DataKind::Json if t.id() == LogicalTypeId::Varchar => Conv::Json,
        DataKind::Json | DataKind::Array => Conv::Nested,
        DataKind::Uuid => Conv::Uuid,
        DataKind::DateTimeTz => Conv::TimestampTz,
        _ => Conv::Plain,
    }
}

/// SQL name of a DuckDB type, e.g. `DECIMAL(10,2)`, `INTEGER[]`,
/// `STRUCT(a INTEGER, b VARCHAR)`.
pub(crate) fn type_name(t: &LogicalTypeHandle) -> String {
    type_name_depth(t, 0)
}

fn type_name_depth(t: &LogicalTypeHandle, depth: usize) -> String {
    if let Some(alias) = t.get_alias() {
        return alias.to_ascii_uppercase();
    }
    if depth > 32 {
        return "...".into();
    }
    let child = |i: usize| type_name_depth(&t.child(i), depth + 1);
    let fields = || {
        (0..t.num_children())
            .map(|i| format!("{} {}", quote_field(&t.child_name(i)), child(i)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match t.id() {
        LogicalTypeId::Boolean => "BOOLEAN".into(),
        LogicalTypeId::Tinyint => "TINYINT".into(),
        LogicalTypeId::Smallint => "SMALLINT".into(),
        LogicalTypeId::Integer => "INTEGER".into(),
        LogicalTypeId::Bigint => "BIGINT".into(),
        LogicalTypeId::Hugeint => "HUGEINT".into(),
        LogicalTypeId::UTinyint => "UTINYINT".into(),
        LogicalTypeId::USmallint => "USMALLINT".into(),
        LogicalTypeId::UInteger => "UINTEGER".into(),
        LogicalTypeId::UBigint => "UBIGINT".into(),
        LogicalTypeId::UHugeint => "UHUGEINT".into(),
        LogicalTypeId::Float => "FLOAT".into(),
        LogicalTypeId::Double => "DOUBLE".into(),
        LogicalTypeId::Decimal => format!("DECIMAL({},{})", t.decimal_width(), t.decimal_scale()),
        LogicalTypeId::Varchar => "VARCHAR".into(),
        LogicalTypeId::Blob => "BLOB".into(),
        LogicalTypeId::Bit => "BIT".into(),
        LogicalTypeId::Date => "DATE".into(),
        LogicalTypeId::Time => "TIME".into(),
        LogicalTypeId::TimeNs => "TIME_NS".into(),
        LogicalTypeId::TimeTZ => "TIME WITH TIME ZONE".into(),
        LogicalTypeId::Timestamp => "TIMESTAMP".into(),
        LogicalTypeId::TimestampS => "TIMESTAMP_S".into(),
        LogicalTypeId::TimestampMs => "TIMESTAMP_MS".into(),
        LogicalTypeId::TimestampNs => "TIMESTAMP_NS".into(),
        LogicalTypeId::TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        LogicalTypeId::Interval => "INTERVAL".into(),
        LogicalTypeId::Uuid => "UUID".into(),
        LogicalTypeId::Enum => "ENUM".into(),
        LogicalTypeId::Geometry => "GEOMETRY".into(),
        LogicalTypeId::Bignum => "BIGNUM".into(),
        LogicalTypeId::List => format!("{}[]", child(0)),
        LogicalTypeId::Array => format!("{}[]", child(0)),
        LogicalTypeId::Map => format!("MAP({}, {})", child(0), child(1)),
        LogicalTypeId::Struct => format!("STRUCT({})", fields()),
        LogicalTypeId::Union => format!("UNION({})", fields()),
        LogicalTypeId::SqlNull => "NULL".into(),
        LogicalTypeId::Variant => "VARIANT".into(),
        other => format!("{other:?}").to_ascii_uppercase(),
    }
}

fn quote_field(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty() {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Seconds and nanoseconds of a timestamp value.
fn split_timestamp(unit: TimeUnit, v: i64) -> (i64, u32) {
    let (per_sec, nanos_per) = match unit {
        TimeUnit::Second => (1, 1_000_000_000),
        TimeUnit::Millisecond => (1_000, 1_000_000),
        TimeUnit::Microsecond => (1_000_000, 1_000),
        TimeUnit::Nanosecond => (1_000_000_000, 1),
    };
    let secs = v.div_euclid(per_sec);
    let frac = v.rem_euclid(per_sec);
    (secs, u32::try_from(frac * nanos_per).unwrap_or(0))
}

fn timestamp(unit: TimeUnit, v: i64) -> Option<NaiveDateTime> {
    let (secs, nanos) = split_timestamp(unit, v);
    DateTime::from_timestamp(secs, nanos).map(|d| d.naive_utc())
}

fn infinite_text(v: i64) -> Value {
    Value::text(if v > 0 { "infinity" } else { "-infinity" })
}

fn date32(days: i32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(i64::from(days)))
}

/// Text of a time of day chrono cannot hold (`24:00:00`).
fn time_text(unit: TimeUnit, v: i64) -> Value {
    let (secs, nanos) = split_timestamp(unit, v);
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if nanos > 0 {
        s.push_str(format!(".{nanos:09}").trim_end_matches('0'));
    }
    Value::text(s)
}

fn time64(unit: TimeUnit, v: i64) -> Option<NaiveTime> {
    let (secs, nanos) = split_timestamp(unit, v);
    NaiveTime::from_num_seconds_from_midnight_opt(u32::try_from(secs).ok()?, nanos)
}

/// DuckDB style interval text: `1 year 2 months 3 days 04:05:06.5`.
pub(crate) fn interval_text(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    let years = months / 12;
    let mons = months % 12;
    let unit =
        |n: i64, one: &str, many: &str| format!("{n} {}", if n.abs() == 1 { one } else { many });
    if years != 0 {
        parts.push(unit(i64::from(years), "year", "years"));
    }
    if mons != 0 {
        parts.push(unit(i64::from(mons), "month", "months"));
    }
    if days != 0 {
        parts.push(unit(i64::from(days), "day", "days"));
    }
    if nanos != 0 || parts.is_empty() {
        let sign = if nanos < 0 { "-" } else { "" };
        let n = nanos.unsigned_abs();
        let secs = n / 1_000_000_000;
        let frac = n % 1_000_000_000;
        let mut t = format!(
            "{sign}{:02}:{:02}:{:02}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        );
        if frac != 0 {
            let f = format!("{frac:09}");
            t.push('.');
            t.push_str(f.trim_end_matches('0'));
        }
        parts.push(t);
    }
    parts.join(" ")
}

/// f32 → f64 through the shortest decimal form (`0.1f32` stays `0.1`).
fn widen(f: f32) -> f64 {
    f.to_string().parse().unwrap_or(f64::from(f))
}

fn blob_text(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &c in b {
        if c.is_ascii_graphic() || c == b' ' {
            if c == b'\\' {
                s.push_str("\\\\");
            } else {
                s.push(char::from(c));
            }
        } else {
            s.push_str(&format!("\\x{c:02X}"));
        }
    }
    s
}

fn decimal_json(d: &Decimal) -> J {
    let s = d.to_string();
    let digits = s.chars().filter(char::is_ascii_digit).count();
    if digits <= 15
        && let Ok(f) = s.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return J::Number(n);
    }
    J::String(s)
}

fn float_json(f: f64) -> J {
    serde_json::Number::from_f64(f)
        .map(J::Number)
        .unwrap_or_else(|| J::String(f.to_string()))
}

/// JSON form of an owned DuckDB value (nested types).
pub(crate) fn to_json(v: &DValue) -> J {
    match v {
        DValue::Null => J::Null,
        DValue::Boolean(b) => J::Bool(*b),
        DValue::TinyInt(i) => J::from(*i),
        DValue::SmallInt(i) => J::from(*i),
        DValue::Int(i) => J::from(*i),
        DValue::BigInt(i) => J::from(*i),
        DValue::UTinyInt(i) => J::from(*i),
        DValue::USmallInt(i) => J::from(*i),
        DValue::UInt(i) => J::from(*i),
        DValue::UBigInt(i) => J::from(*i),
        DValue::HugeInt(i) => i64::try_from(*i)
            .map(J::from)
            .unwrap_or_else(|_| J::String(i.to_string())),
        DValue::UHugeInt(i) => u64::try_from(*i)
            .map(J::from)
            .unwrap_or_else(|_| J::String(i.to_string())),
        DValue::Float(f) => float_json(widen(*f)),
        DValue::Double(f) => float_json(*f),
        DValue::Decimal(d) => decimal_json(d),
        DValue::Timestamp(unit, t) => J::String(
            timestamp(*unit, *t)
                .map(|d| Value::DateTime(d).to_text().into_owned())
                .unwrap_or_else(|| infinite_text(*t).to_text().into_owned()),
        ),
        DValue::Text(s) => J::String(s.clone()),
        DValue::Blob(b) | DValue::Geometry(b) => J::String(blob_text(b)),
        DValue::Date32(d) => J::String(
            date32(*d)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| infinite_text(i64::from(*d)).to_text().into_owned()),
        ),
        DValue::Time64(unit, t) => J::String(
            time64(*unit, *t)
                .map(|t| Value::Time(t).to_text().into_owned())
                .unwrap_or_default(),
        ),
        DValue::Interval {
            months,
            days,
            nanos,
        } => J::String(interval_text(*months, *days, *nanos)),
        DValue::List(items) | DValue::Array(items) => J::Array(items.iter().map(to_json).collect()),
        DValue::Enum(s) => J::String(s.clone()),
        DValue::Struct(fields) => J::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect(),
        ),
        DValue::Map(entries) => J::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match to_json(k) {
                        J::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, to_json(v))
                })
                .collect(),
        ),
        DValue::Union(inner) => to_json(inner),
        _ => J::Null,
    }
}

/// Convert one fetched value of a column with conversion `conv`.
pub(crate) fn convert(v: ValueRef<'_>, conv: Conv) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::Int(i64::from(i)),
        ValueRef::SmallInt(i) => Value::Int(i64::from(i)),
        ValueRef::Int(i) => Value::Int(i64::from(i)),
        ValueRef::BigInt(i) => Value::Int(i),
        ValueRef::UTinyInt(i) => Value::Int(i64::from(i)),
        ValueRef::USmallInt(i) => Value::Int(i64::from(i)),
        ValueRef::UInt(i) => Value::Int(i64::from(i)),
        ValueRef::UBigInt(i) => Value::UInt(i),
        ValueRef::HugeInt(i) => i64::try_from(i)
            .map(Value::Int)
            .unwrap_or_else(|_| Value::decimal(i.to_string())),
        ValueRef::UHugeInt(i) => u64::try_from(i)
            .map(Value::UInt)
            .unwrap_or_else(|_| Value::decimal(i.to_string())),
        ValueRef::Float(f) => Value::Float(widen(f)),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Decimal(d) => Value::decimal(d.to_string()),
        ValueRef::Timestamp(unit, t) => match timestamp(unit, t) {
            Some(dt) if conv == Conv::TimestampTz => Value::DateTimeTz(dt.and_utc().fixed_offset()),
            Some(dt) => Value::DateTime(dt),
            None => infinite_text(t),
        },
        ValueRef::Text(b) => {
            let s = String::from_utf8_lossy(b);
            match conv {
                Conv::Uuid => match uuid::Uuid::parse_str(&s) {
                    Ok(u) => Value::Uuid(u),
                    Err(_) => Value::text(s),
                },
                Conv::Json => Value::json(s),
                _ => Value::text(s),
            }
        }
        ValueRef::Blob(b) if conv == Conv::Uuid && b.len() == 16 => uuid::Uuid::from_slice(b)
            .map(Value::Uuid)
            .unwrap_or_else(|_| Value::bytes(b.to_vec())),
        ValueRef::Blob(b) | ValueRef::Geometry(b) => Value::bytes(b.to_vec()),
        ValueRef::Date32(d) => date32(d)
            .map(Value::Date)
            .unwrap_or_else(|| infinite_text(i64::from(d))),
        // `TIME '24:00:00'` is valid in DuckDB but not a chrono time: kept as text.
        ValueRef::Time64(unit, t) => time64(unit, t)
            .map(Value::Time)
            .unwrap_or_else(|| time_text(unit, t)),
        ValueRef::Interval {
            months,
            days,
            nanos,
        } => Value::text(interval_text(months, days, nanos)),
        ValueRef::Enum(..) => match v.to_owned() {
            DValue::Enum(s) => Value::text(s),
            other => Value::text(to_json(&other).to_string()),
        },
        other => Value::json(to_json(&other.to_owned()).to_string()),
    }
}

/// Approximate size of a converted value.
pub(crate) fn value_bytes(v: &Value) -> usize {
    std::mem::size_of::<Value>() + v.heap_size()
}

fn decimal_param(s: &str) -> Option<Decimal> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac.is_empty()
        || !int_part
            .chars()
            .chain(frac.chars())
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let all = format!("{int_part}{frac}");
    let trimmed = all.trim_start_matches('0');
    let width = trimmed.len().max(frac.len()).max(1);
    if width > 38 {
        return None;
    }
    let mut value: i128 = all.parse().ok()?;
    if neg {
        value = -value;
    }
    Decimal::new(
        u8::try_from(width).ok()?,
        u8::try_from(frac.len()).ok()?,
        value,
    )
    .ok()
}

/// A [`Value`] bound as a statement parameter.
pub(crate) struct Param<'a>(pub &'a Value);

impl ToSql for Param<'_> {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Value::Bool(b) => ToSqlOutput::Borrowed(ValueRef::Boolean(*b)),
            Value::Int(i) => ToSqlOutput::Borrowed(ValueRef::BigInt(*i)),
            Value::UInt(u) => ToSqlOutput::Borrowed(ValueRef::UBigInt(*u)),
            Value::Float(f) => ToSqlOutput::Borrowed(ValueRef::Double(*f)),
            Value::Decimal(s) => match decimal_param(s) {
                Some(d) => ToSqlOutput::Borrowed(ValueRef::Decimal(d)),
                None => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            },
            Value::Text(s) | Value::Json(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Value::Bytes(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Value::Date(d) => {
                let days = d
                    .signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default())
                    .num_days();
                ToSqlOutput::Owned(DValue::Date32(i32::try_from(days).unwrap_or(i32::MAX)))
            }
            Value::Time(t) => {
                let micros = i64::from(t.num_seconds_from_midnight()) * 1_000_000
                    + i64::from(t.nanosecond() / 1_000);
                ToSqlOutput::Owned(DValue::Time64(TimeUnit::Microsecond, micros))
            }
            Value::DateTime(dt) => ToSqlOutput::Owned(DValue::Timestamp(
                TimeUnit::Microsecond,
                dt.and_utc().timestamp_micros(),
            )),
            Value::DateTimeTz(dt) => ToSqlOutput::Owned(DValue::Timestamp(
                TimeUnit::Microsecond,
                dt.timestamp_micros(),
            )),
            Value::Uuid(u) => ToSqlOutput::Owned(DValue::Text(u.hyphenated().to_string())),
            Value::Array(_) => ToSqlOutput::Owned(DValue::Text(self.0.to_json().to_string())),
        })
    }
}

/// Logical kind of a DuckDB type name (catalog `data_type` strings).
pub(crate) fn kind_of_name(name: &str) -> DataKind {
    let upper = name.trim().to_ascii_uppercase();
    if upper.ends_with(']') {
        return DataKind::Array;
    }
    let base = upper.split(['(', ' ']).next().unwrap_or("");
    match base {
        "BOOLEAN" | "BOOL" | "LOGICAL" => DataKind::Bool,
        "TINYINT" | "SMALLINT" | "INTEGER" | "INT" | "BIGINT" | "HUGEINT" | "UTINYINT"
        | "USMALLINT" | "UINTEGER" | "UBIGINT" | "UHUGEINT" | "INT1" | "INT2" | "INT4" | "INT8"
        | "INT16" | "SHORT" | "LONG" | "SIGNED" => DataKind::Int,
        "FLOAT" | "DOUBLE" | "REAL" | "FLOAT4" | "FLOAT8" => DataKind::Float,
        "DECIMAL" | "NUMERIC" | "BIGNUM" | "VARINT" => DataKind::Decimal,
        "VARCHAR" | "TEXT" | "STRING" | "CHAR" | "BPCHAR" | "NVARCHAR" => DataKind::Text,
        "BLOB" | "BYTEA" | "BINARY" | "VARBINARY" => DataKind::Binary,
        "BIT" | "BITSTRING" => DataKind::Bit,
        "DATE" => DataKind::Date,
        "TIME" if upper.contains("WITH TIME ZONE") => DataKind::TimeTz,
        "TIME" | "TIME_NS" => DataKind::Time,
        "TIMETZ" => DataKind::TimeTz,
        "TIMESTAMP" if upper.contains("WITH TIME ZONE") => DataKind::DateTimeTz,
        "TIMESTAMPTZ" => DataKind::DateTimeTz,
        "TIMESTAMP" | "DATETIME" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS"
        | "TIMESTAMP_US" => DataKind::DateTime,
        "INTERVAL" => DataKind::Interval,
        "UUID" => DataKind::Uuid,
        "JSON" => DataKind::Json,
        "STRUCT" | "MAP" | "UNION" => DataKind::Json,
        "ENUM" => DataKind::Enum,
        "GEOMETRY" => DataKind::Geometry,
        _ => DataKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(
            interval_text(
                14,
                3,
                4 * 3_600_000_000_000 + 5 * 60_000_000_000 + 6_500_000_000
            ),
            "1 year 2 months 3 days 04:05:06.5"
        );
        assert_eq!(interval_text(0, 0, 0), "00:00:00");
        assert_eq!(interval_text(1, 1, 0), "1 month 1 day");
        assert_eq!(interval_text(0, -2, -1_000_000_000), "-2 days -00:00:01");
    }

    #[test]
    fn scalar_conversions() {
        assert_eq!(convert(ValueRef::HugeInt(5), Conv::Plain), Value::Int(5));
        assert_eq!(
            convert(ValueRef::HugeInt(i128::MAX), Conv::Plain),
            Value::decimal(i128::MAX.to_string())
        );
        assert_eq!(
            convert(ValueRef::UBigInt(u64::MAX), Conv::Plain),
            Value::UInt(u64::MAX)
        );
        assert_eq!(
            convert(ValueRef::Float(0.1), Conv::Plain),
            Value::Float(0.1)
        );
        let ts = convert(
            ValueRef::Timestamp(TimeUnit::Microsecond, 1_700_000_000_123_456),
            Conv::Plain,
        );
        assert_eq!(ts.to_text(), "2023-11-14 22:13:20.123456");
        let tz = convert(
            ValueRef::Timestamp(TimeUnit::Microsecond, 0),
            Conv::TimestampTz,
        );
        assert_eq!(tz.to_text(), "1970-01-01 00:00:00+00");
        assert_eq!(
            convert(
                ValueRef::Timestamp(TimeUnit::Microsecond, i64::MAX),
                Conv::Plain
            ),
            Value::text("infinity")
        );
        assert_eq!(
            convert(ValueRef::Date32(19_000), Conv::Plain).to_text(),
            "2022-01-08"
        );
        assert_eq!(
            convert(
                ValueRef::Time64(TimeUnit::Microsecond, 3_723_000_000),
                Conv::Plain
            )
            .to_text(),
            "01:02:03"
        );
        let u = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        assert!(matches!(
            convert(ValueRef::Text(u.as_bytes()), Conv::Uuid),
            Value::Uuid(_)
        ));
        assert_eq!(
            convert(ValueRef::Text(b"{\"a\":1}"), Conv::Json),
            Value::json("{\"a\":1}")
        );
    }

    #[test]
    fn nested_json() {
        let v = DValue::Struct(duckdb::types::OrderedMap::from(vec![
            ("a".to_string(), DValue::Int(1)),
            (
                "b".to_string(),
                DValue::List(vec![DValue::Text("x".into()), DValue::Null]),
            ),
        ]));
        assert_eq!(to_json(&v).to_string(), r#"{"a":1,"b":["x",null]}"#);
        let m = DValue::Map(duckdb::types::OrderedMap::from(vec![(
            DValue::Int(1),
            DValue::Text("one".into()),
        )]));
        assert_eq!(to_json(&m).to_string(), r#"{"1":"one"}"#);
    }

    #[test]
    fn decimals() {
        let d = decimal_param("-12.50").unwrap();
        assert_eq!(d.to_string(), "-12.50");
        assert!(decimal_param("1e5").is_none());
        assert!(decimal_param("abc").is_none());
        assert_eq!(decimal_param("0.001").unwrap().to_string(), "0.001");
    }

    #[test]
    fn kinds_from_names() {
        assert_eq!(kind_of_name("INTEGER[]"), DataKind::Array);
        assert_eq!(kind_of_name("DECIMAL(10,2)"), DataKind::Decimal);
        assert_eq!(
            kind_of_name("TIMESTAMP WITH TIME ZONE"),
            DataKind::DateTimeTz
        );
        assert_eq!(kind_of_name("STRUCT(a INTEGER)"), DataKind::Json);
        assert_eq!(kind_of_name("VARCHAR"), DataKind::Text);
    }
}
