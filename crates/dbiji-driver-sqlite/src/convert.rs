//! Conversion between SQLite values and DBiji [`Value`]s.
//!
//! SQLite stores INTEGER, REAL, TEXT, BLOB or NULL in any column; the
//! declared column type only gives an affinity. Values are mapped by storage
//! class (INTEGER → `Int`, REAL → `Float`, TEXT → `Text`, BLOB → `Bytes`) and
//! refined conservatively and losslessly using the declared type:
//!
//! * `BOOLEAN` columns: integers 0 / 1 become `Bool`.
//! * `DATE` / `DATETIME` / `TIMESTAMP` / `TIME` columns: text in exactly the
//!   canonical layout (`YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`, `HH:MM:SS`) becomes
//!   `Date` / `DateTime` / `Time`; everything else (fractions, `T` separator,
//!   offsets, Julian days, Unix times) is kept as stored, so that the text form
//!   of every converted value reproduces the stored text exactly.
//! * `JSON` columns: valid JSON text becomes `Json`.
//! * `UUID` columns: canonical lower case UUID text becomes `Uuid`.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use dbiji_core::value::{DataKind, Value};
use rusqlite::types::{ToSql, ToSqlOutput, Value as SqlValue, ValueRef};
use std::borrow::Cow;

/// Per column conversion applied to fetched values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conv {
    Plain,
    Bool,
    Date,
    DateTime,
    Time,
    Json,
    Uuid,
}

impl Conv {
    /// Conversion for a column of logical kind `kind`.
    pub fn for_kind(kind: DataKind) -> Conv {
        match kind {
            DataKind::Bool => Conv::Bool,
            DataKind::Date => Conv::Date,
            DataKind::DateTime => Conv::DateTime,
            DataKind::Time => Conv::Time,
            DataKind::Json => Conv::Json,
            DataKind::Uuid => Conv::Uuid,
            _ => Conv::Plain,
        }
    }
}

/// Logical kind of a declared SQLite column type: well known type names
/// first, then SQLite's affinity rules (<https://sqlite.org/datatype3.html>).
pub(crate) fn declared_kind(decl: &str) -> DataKind {
    let upper = decl.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return DataKind::Other;
    }
    let base = upper.split('(').next().unwrap_or("");
    let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
    let specific = match base.as_str() {
        "BOOLEAN" | "BOOL" => Some(DataKind::Bool),
        "DATE" => Some(DataKind::Date),
        "DATETIME"
        | "TIMESTAMP"
        | "TIMESTAMP WITHOUT TIME ZONE"
        | "DATETIME2"
        | "SMALLDATETIME" => Some(DataKind::DateTime),
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" | "DATETIMEOFFSET" => Some(DataKind::DateTimeTz),
        "TIME" | "TIME WITHOUT TIME ZONE" => Some(DataKind::Time),
        "TIMETZ" | "TIME WITH TIME ZONE" => Some(DataKind::TimeTz),
        "JSON" | "JSONB" => Some(DataKind::Json),
        "UUID" | "GUID" | "UNIQUEIDENTIFIER" => Some(DataKind::Uuid),
        "DECIMAL" | "NUMERIC" | "MONEY" | "SMALLMONEY" | "NUMBER" | "DEC" => {
            Some(DataKind::Decimal)
        }
        "INTERVAL" => Some(DataKind::Interval),
        "XML" => Some(DataKind::Xml),
        "ANY" => Some(DataKind::Other),
        _ => None,
    };
    if let Some(k) = specific {
        return k;
    }
    match DataKind::guess_from_type_name(decl) {
        DataKind::Other => {}
        k => return k,
    }
    if upper.contains("INT") {
        DataKind::Int
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        DataKind::Text
    } else if upper.contains("BLOB") {
        DataKind::Binary
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        DataKind::Float
    } else {
        DataKind::Other
    }
}

/// `(precision, scale, max_length)` from type parameters: `VARCHAR(20)`
/// gives a max length, `DECIMAL(10,2)` precision and scale.
pub(crate) fn type_params(decl: &str, kind: DataKind) -> (Option<u32>, Option<u32>, Option<u32>) {
    let Some(open) = decl.find('(') else {
        return (None, None, None);
    };
    let Some(close) = decl[open..].find(')') else {
        return (None, None, None);
    };
    let inner = &decl[open + 1..open + close];
    let mut nums = inner.split(',').map(|s| s.trim().parse::<u32>().ok());
    let first = nums.next().flatten();
    let second = nums.next().flatten();
    match kind {
        DataKind::Decimal | DataKind::Float | DataKind::Int => (first, second, None),
        _ => (None, None, first),
    }
}

/// Logical kind and type name for a column without declared type, from the
/// storage class of its first value.
pub(crate) fn storage_class_kind(v: &ValueRef<'_>) -> (DataKind, &'static str) {
    match v {
        ValueRef::Null => (DataKind::Other, ""),
        ValueRef::Integer(_) => (DataKind::Int, "INTEGER"),
        ValueRef::Real(_) => (DataKind::Float, "REAL"),
        ValueRef::Text(_) => (DataKind::Text, "TEXT"),
        ValueRef::Blob(_) => (DataKind::Binary, "BLOB"),
    }
}

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    b.iter().try_fold(0u32, |acc, d| {
        acc.checked_mul(10)?.checked_add(u32::from(d - b'0'))
    })
}

/// `YYYY-MM-DD` exactly.
pub(crate) fn exact_date(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = i32::try_from(digits(&b[0..4])?).ok()?;
    NaiveDate::from_ymd_opt(y, digits(&b[5..7])?, digits(&b[8..10])?)
}

/// `HH:MM:SS` exactly.
pub(crate) fn exact_time(s: &str) -> Option<NaiveTime> {
    let b = s.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    NaiveTime::from_hms_opt(digits(&b[0..2])?, digits(&b[3..5])?, digits(&b[6..8])?)
}

/// `YYYY-MM-DD HH:MM:SS` exactly.
pub(crate) fn exact_datetime(s: &str) -> Option<NaiveDateTime> {
    if s.len() != 19 || s.as_bytes()[10] != b' ' {
        return None;
    }
    Some(NaiveDateTime::new(
        exact_date(s.get(..10)?)?,
        exact_time(s.get(11..)?)?,
    ))
}

/// Canonical lower case hyphenated UUID text.
pub(crate) fn exact_uuid(s: &str) -> Option<uuid::Uuid> {
    let b = s.as_bytes();
    if b.len() != 36 || [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
        return None;
    }
    if b.iter().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    uuid::Uuid::parse_str(s).ok()
}

fn is_valid_json(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

fn text_of(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// Convert one fetched SQLite value.
pub(crate) fn convert(v: ValueRef<'_>, conv: Conv) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => match conv {
            Conv::Bool if i == 0 || i == 1 => Value::Bool(i == 1),
            _ => Value::Int(i),
        },
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(b) => {
            let s = text_of(b);
            match conv {
                Conv::Date => {
                    if let Some(d) = exact_date(&s) {
                        return Value::Date(d);
                    }
                }
                Conv::DateTime => {
                    if let Some(d) = exact_datetime(&s) {
                        return Value::DateTime(d);
                    }
                }
                Conv::Time => {
                    if let Some(t) = exact_time(&s) {
                        return Value::Time(t);
                    }
                }
                Conv::Uuid => {
                    if let Some(u) = exact_uuid(&s) {
                        return Value::Uuid(u);
                    }
                }
                Conv::Json => {
                    if is_valid_json(&s) {
                        return Value::Json(s.into_owned().into_boxed_str());
                    }
                }
                Conv::Plain | Conv::Bool => {}
            }
            Value::Text(s.into_owned().into_boxed_str())
        }
        ValueRef::Blob(b) => Value::Bytes(b.into()),
    }
}

/// Approximate bytes a fetched value occupies (for batch size limits).
pub(crate) fn value_bytes(v: &Value) -> usize {
    std::mem::size_of::<Value>() + v.heap_size()
}

/// `YYYY-MM-DD HH:MM:SS[.fff]+HH:MM`, the layout SQLite date functions accept.
fn datetime_tz_text(dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string()
}

/// Text stored for temporal, UUID and array values.
pub(crate) fn value_as_sql_text(v: &Value) -> String {
    match v {
        Value::DateTimeTz(dt) => datetime_tz_text(dt),
        Value::Array(_) => v.to_json().to_string(),
        other => other.to_text().into_owned(),
    }
}

/// A [`Value`] bound as a statement parameter. Dates, times, UUIDs and JSON
/// are bound as text, bytes as blobs, booleans as 0 / 1, arrays as JSON text.
pub(crate) struct Param<'a>(pub &'a Value);

impl ToSql for Param<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Value::Bool(b) => ToSqlOutput::Borrowed(ValueRef::Integer(i64::from(*b))),
            Value::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Value::UInt(u) => match i64::try_from(*u) {
                Ok(i) => ToSqlOutput::Borrowed(ValueRef::Integer(i)),
                Err(_) => ToSqlOutput::Owned(SqlValue::Text(u.to_string())),
            },
            Value::Float(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            Value::Decimal(s) => match s.trim().parse::<i64>() {
                Ok(i) => ToSqlOutput::Borrowed(ValueRef::Integer(i)),
                Err(_) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            },
            Value::Text(s) | Value::Json(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Value::Bytes(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            other => ToSqlOutput::Owned(SqlValue::Text(value_as_sql_text(other))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_declared_types() {
        assert_eq!(declared_kind("INTEGER"), DataKind::Int);
        assert_eq!(declared_kind("unsigned big int"), DataKind::Int);
        assert_eq!(declared_kind("VARCHAR(255)"), DataKind::Text);
        assert_eq!(declared_kind("NATIVE CHARACTER(70)"), DataKind::Text);
        assert_eq!(declared_kind("DOUBLE PRECISION"), DataKind::Float);
        assert_eq!(declared_kind("BOOLEAN"), DataKind::Bool);
        assert_eq!(declared_kind("datetime"), DataKind::DateTime);
        assert_eq!(declared_kind("TIMESTAMP"), DataKind::DateTime);
        assert_eq!(declared_kind("DATE"), DataKind::Date);
        assert_eq!(declared_kind("JSON"), DataKind::Json);
        assert_eq!(declared_kind("uuid"), DataKind::Uuid);
        assert_eq!(declared_kind("DECIMAL(10,2)"), DataKind::Decimal);
        assert_eq!(declared_kind("BLOB"), DataKind::Binary);
        assert_eq!(declared_kind(""), DataKind::Other);
        assert_eq!(declared_kind("WHATEVER"), DataKind::Other);
    }

    #[test]
    fn params_of_types() {
        assert_eq!(
            type_params("VARCHAR(20)", DataKind::Text),
            (None, None, Some(20))
        );
        assert_eq!(
            type_params("DECIMAL(10, 2)", DataKind::Decimal),
            (Some(10), Some(2), None)
        );
        assert_eq!(type_params("TEXT", DataKind::Text), (None, None, None));
    }

    #[test]
    fn conversions_are_lossless() {
        assert_eq!(convert(ValueRef::Integer(1), Conv::Bool), Value::Bool(true));
        assert_eq!(convert(ValueRef::Integer(2), Conv::Bool), Value::Int(2));
        let d = convert(ValueRef::Text(b"2024-02-29"), Conv::Date);
        assert!(matches!(d, Value::Date(_)));
        assert_eq!(d.to_text(), "2024-02-29");
        assert_eq!(
            convert(ValueRef::Text(b"2024-02-30"), Conv::Date),
            Value::text("2024-02-30")
        );
        let dt = convert(ValueRef::Text(b"2024-01-02 03:04:05"), Conv::DateTime);
        assert!(matches!(dt, Value::DateTime(_)));
        assert_eq!(dt.to_text(), "2024-01-02 03:04:05");
        for keep in [
            "2024-01-02T03:04:05",
            "2024-01-02 03:04:05.120",
            "2024-01-02 03:04:05+02:00",
        ] {
            assert_eq!(
                convert(ValueRef::Text(keep.as_bytes()), Conv::DateTime),
                Value::text(keep)
            );
        }
        assert!(matches!(
            convert(ValueRef::Text(b"23:59:59"), Conv::Time),
            Value::Time(_)
        ));
        assert_eq!(
            convert(ValueRef::Text(b"{\"a\":1}"), Conv::Json),
            Value::json("{\"a\":1}")
        );
        assert_eq!(
            convert(ValueRef::Text(b"{bad"), Conv::Json),
            Value::text("{bad")
        );
        let u = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        assert!(matches!(
            convert(ValueRef::Text(u.as_bytes()), Conv::Uuid),
            Value::Uuid(_)
        ));
        let upper = u.to_ascii_uppercase();
        assert_eq!(
            convert(ValueRef::Text(upper.as_bytes()), Conv::Uuid),
            Value::text(upper.clone())
        );
        assert_eq!(
            convert(ValueRef::Text(&[0x61, 0xff]), Conv::Plain),
            Value::text("a\u{fffd}")
        );
        assert_eq!(
            convert(ValueRef::Blob(&[1, 2]), Conv::Plain),
            Value::bytes(vec![1u8, 2])
        );
        assert_eq!(convert(ValueRef::Integer(7), Conv::DateTime), Value::Int(7));
    }

    #[test]
    fn binds() {
        fn out(v: &Value) -> SqlValue {
            match Param(v).to_sql().unwrap() {
                ToSqlOutput::Borrowed(r) => SqlValue::try_from(r).unwrap(),
                ToSqlOutput::Owned(o) => o,
                _ => unreachable!(),
            }
        }
        assert_eq!(out(&Value::Bool(true)), SqlValue::Integer(1));
        assert_eq!(
            out(&Value::UInt(u64::MAX)),
            SqlValue::Text(u64::MAX.to_string())
        );
        assert_eq!(out(&Value::decimal("12")), SqlValue::Integer(12));
        assert_eq!(
            out(&Value::decimal("12.50")),
            SqlValue::Text("12.50".into())
        );
        let dt = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05+07:00").unwrap();
        assert_eq!(
            out(&Value::DateTimeTz(dt)),
            SqlValue::Text("2024-01-02 03:04:05+07:00".into())
        );
        let arr = Value::Array(vec![Value::Int(1), Value::text("x")].into_boxed_slice());
        assert_eq!(out(&arr), SqlValue::Text("[1,\"x\"]".into()));
        assert_eq!(out(&Value::bytes(vec![9u8])), SqlValue::Blob(vec![9]));
    }
}
