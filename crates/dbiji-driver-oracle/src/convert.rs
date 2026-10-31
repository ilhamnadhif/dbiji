//! Oracle values and column types to DBiji's [`Value`] / [`DataKind`], and
//! bind parameters the other way.

use chrono::{Datelike, NaiveDateTime, Timelike};
use dbiji_core::value::{ColumnMeta, DataKind};
use dbiji_core::{DbResult, Value};
use oracledb::{
    JsonValue, Metadata, OracleIntervalDS, OracleIntervalYM, OracleNumber, OracleTimestamp,
    ToDbValue,
};

use crate::error::OraResultExt;

/// How a result column is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    Int,
    Number,
    Float,
    Double,
    Text,
    Bytes,
    Timestamp,
    TimestampUtc,
    IntervalDs,
    IntervalYm,
    Json,
    Bool,
    Other,
}

/// Column description plus the way its values are read.
pub(crate) fn column(m: &Metadata) -> (ColumnMeta, Read) {
    let ty = m.db_type().name();
    let (kind, read) = match ty {
        "DB_TYPE_NUMBER" => {
            if m.scale() == 0 && m.precision() > 0 && m.precision() <= 18 {
                (DataKind::Int, Read::Int)
            } else {
                (DataKind::Decimal, Read::Number)
            }
        }
        "DB_TYPE_BINARY_INTEGER" => (DataKind::Int, Read::Int),
        "DB_TYPE_BINARY_FLOAT" => (DataKind::Float, Read::Float),
        "DB_TYPE_BINARY_DOUBLE" => (DataKind::Float, Read::Double),
        "DB_TYPE_VARCHAR"
        | "DB_TYPE_NVARCHAR"
        | "DB_TYPE_CHAR"
        | "DB_TYPE_NCHAR"
        | "DB_TYPE_LONG"
        | "DB_TYPE_LONG_NVARCHAR"
        | "DB_TYPE_CLOB"
        | "DB_TYPE_NCLOB"
        | "DB_TYPE_ROWID"
        | "DB_TYPE_UROWID" => (DataKind::Text, Read::Text),
        "DB_TYPE_RAW" | "DB_TYPE_LONG_RAW" | "DB_TYPE_BLOB" => (DataKind::Binary, Read::Bytes),
        "DB_TYPE_DATE" | "DB_TYPE_TIMESTAMP" => (DataKind::DateTime, Read::Timestamp),
        "DB_TYPE_TIMESTAMP_TZ" | "DB_TYPE_TIMESTAMP_LTZ" => {
            (DataKind::DateTimeTz, Read::TimestampUtc)
        }
        "DB_TYPE_INTERVAL_DS" => (DataKind::Interval, Read::IntervalDs),
        "DB_TYPE_INTERVAL_YM" => (DataKind::Interval, Read::IntervalYm),
        "DB_TYPE_JSON" => (DataKind::Json, Read::Json),
        "DB_TYPE_XMLTYPE" => (DataKind::Xml, Read::Text),
        "DB_TYPE_BOOLEAN" => (DataKind::Bool, Read::Bool),
        _ => (DataKind::Other, Read::Other),
    };
    let mut meta = ColumnMeta::new(m.name(), m.data_type(), kind);
    meta.nullable = Some(m.nullable());
    if m.precision() > 0 {
        meta.precision = Some(m.precision() as u32);
    }
    if m.scale() >= 0 && matches!(kind, DataKind::Int | DataKind::Decimal) {
        meta.scale = Some(m.scale() as u32);
    }
    if matches!(kind, DataKind::Text | DataKind::Binary) && m.max_size() > 0 {
        meta.max_length = Some(m.max_size());
    }
    (meta, read)
}

/// One row of a result.
pub(crate) fn row(row: &oracledb::Row, reads: &[Read]) -> DbResult<Vec<Value>> {
    let mut out = Vec::with_capacity(reads.len());
    for (i, read) in reads.iter().enumerate() {
        out.push(value(row, i, *read)?);
    }
    Ok(out)
}

fn value(row: &oracledb::Row, i: usize, read: Read) -> DbResult<Value> {
    Ok(match read {
        Read::Int => match row.get::<Option<OracleNumber>>(i).db()? {
            None => Value::Null,
            Some(n) => {
                let s = n.to_string();
                s.parse::<i64>()
                    .map(Value::Int)
                    .unwrap_or_else(|_| Value::Decimal(s.into()))
            }
        },
        Read::Number => match row.get::<Option<OracleNumber>>(i).db()? {
            None => Value::Null,
            Some(n) => {
                let s = n.to_string();
                match s.parse::<i64>() {
                    Ok(v) => Value::Int(v),
                    Err(_) => Value::Decimal(s.into()),
                }
            }
        },
        Read::Float => row
            .get::<Option<f32>>(i)
            .db()?
            .map_or(Value::Null, |f| Value::Float(f as f64)),
        Read::Double => row
            .get::<Option<f64>>(i)
            .db()?
            .map_or(Value::Null, Value::Float),
        Read::Text => row
            .get::<Option<String>>(i)
            .db()?
            .map_or(Value::Null, |s| Value::Text(s.into())),
        Read::Bytes => row
            .get::<Option<Vec<u8>>>(i)
            .db()?
            .map_or(Value::Null, |b| Value::Bytes(b.into())),
        Read::Timestamp => row
            .get::<Option<OracleTimestamp>>(i)
            .db()?
            .and_then(naive)
            .map_or(Value::Null, Value::DateTime),
        Read::TimestampUtc => row
            .get::<Option<OracleTimestamp>>(i)
            .db()?
            .and_then(naive)
            .map_or(Value::Null, |t| {
                Value::DateTimeTz(t.and_utc().fixed_offset())
            }),
        Read::IntervalDs => row
            .get::<Option<OracleIntervalDS>>(i)
            .db()?
            .map_or(Value::Null, |v| Value::Text(interval_ds(&v).into())),
        Read::IntervalYm => row
            .get::<Option<OracleIntervalYM>>(i)
            .db()?
            .map_or(Value::Null, |v| Value::Text(interval_ym(&v).into())),
        Read::Json => row
            .get::<Option<JsonValue>>(i)
            .db()?
            .map_or(Value::Null, |j| Value::Json(json(&j).to_string().into())),
        Read::Bool => row
            .get::<Option<bool>>(i)
            .db()?
            .map_or(Value::Null, Value::Bool),
        Read::Other => match row.get::<Option<String>>(i) {
            Ok(v) => v.map_or(Value::Null, |s| Value::Text(s.into())),
            Err(_) => Value::Text("(unsupported value)".into()),
        },
    })
}

pub(crate) fn naive(t: OracleTimestamp) -> Option<NaiveDateTime> {
    NaiveDateTime::try_from(t).ok()
}

/// `+1 02:03:04.500000`, as Oracle prints `INTERVAL DAY TO SECOND`.
fn interval_ds(v: &OracleIntervalDS) -> String {
    let neg =
        v.days() < 0 || v.hours() < 0 || v.minutes() < 0 || v.seconds() < 0 || v.nanoseconds() < 0;
    let mut s = format!(
        "{}{} {:02}:{:02}:{:02}",
        if neg { '-' } else { '+' },
        v.days().unsigned_abs(),
        v.hours().unsigned_abs(),
        v.minutes().unsigned_abs(),
        v.seconds().unsigned_abs()
    );
    if v.nanoseconds() != 0 {
        s.push_str(&format!(".{:06}", v.nanoseconds().unsigned_abs() / 1000));
    }
    s
}

fn interval_ym(v: &OracleIntervalYM) -> String {
    let neg = v.years() < 0 || v.months() < 0;
    format!(
        "{}{}-{:02}",
        if neg { '-' } else { '+' },
        v.years().unsigned_abs(),
        v.months().unsigned_abs()
    )
}

fn json(v: &JsonValue) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        JsonValue::Null => J::Null,
        JsonValue::Boolean(b) => J::Bool(*b),
        JsonValue::String(s) => J::String(s.clone()),
        JsonValue::Number(n) => {
            let s = n.to_string();
            serde_json::from_str(&s).unwrap_or(J::String(s))
        }
        JsonValue::BinaryDouble(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        JsonValue::BinaryFloat(f) => {
            serde_json::Number::from_f64(*f as f64).map_or(J::Null, J::Number)
        }
        JsonValue::Timestamp(t) => naive(*t).map_or(J::Null, |t| {
            J::String(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
        }),
        JsonValue::IntervalDS(i) => J::String(interval_ds(i)),
        JsonValue::IntervalYM(i) => J::String(interval_ym(i)),
        JsonValue::Raw(b) | JsonValue::JsonId(b) => J::String(hex::encode_upper(b)),
        JsonValue::JsonArray(a) => J::Array(a.iter().map(json).collect()),
        JsonValue::JsonObject(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            J::Object(keys.into_iter().map(|k| (k.clone(), json(&o[k]))).collect())
        }
        JsonValue::Vector(v) => J::String(format!("{v:?}")),
    }
}

/// A bind parameter for `value`.
pub(crate) fn bind(value: &Value) -> Box<dyn ToDbValue + Send + Sync> {
    match value {
        Value::Null => Box::new(None::<String>),
        // Numbers convert to BOOLEAN on 23ai and work with NUMBER(1) flags before.
        Value::Bool(b) => Box::new(i64::from(*b)),
        Value::Int(i) => Box::new(*i),
        Value::UInt(u) => Box::new(*u),
        Value::Float(f) => Box::new(*f),
        Value::Decimal(s) => match s.parse::<OracleNumber>() {
            Ok(n) => Box::new(n),
            Err(_) => Box::new(s.to_string()),
        },
        Value::Text(s) | Value::Json(s) => Box::new(s.to_string()),
        Value::Bytes(b) => Box::new(b.to_vec()),
        Value::Date(d) => Box::new(OracleTimestamp::new_date(
            d.year() as i16,
            d.month() as u8,
            d.day() as u8,
        )),
        Value::DateTime(t) => Box::new(timestamp(t, false)),
        Value::DateTimeTz(t) => Box::new(timestamp(&t.naive_utc(), true)),
        other => Box::new(other.to_text().into_owned()),
    }
}

fn timestamp(t: &NaiveDateTime, utc: bool) -> OracleTimestamp {
    let (y, mo, d, h, mi, s, ns) = (
        t.year() as i16,
        t.month() as u8,
        t.day() as u8,
        t.hour() as u8,
        t.minute() as u8,
        t.second() as u8,
        t.nanosecond(),
    );
    if utc {
        OracleTimestamp::new_utc_timestamp(y, mo, d, h, mi, s, ns)
    } else {
        OracleTimestamp::new_timestamp(y, mo, d, h, mi, s, ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_print_like_oracle() {
        assert_eq!(
            interval_ds(&OracleIntervalDS::new(1, 2, 3, 4, 500_000_000)),
            "+1 02:03:04.500000"
        );
        assert_eq!(
            interval_ds(&OracleIntervalDS::new(-3, 0, 0, 0, 0)),
            "-3 00:00:00"
        );
        assert_eq!(interval_ym(&OracleIntervalYM::new(2, 3)), "+2-03");
    }
}
