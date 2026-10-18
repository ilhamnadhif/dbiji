//! Statement parameters sent in PostgreSQL TEXT format.
//!
//! Every [`Value`] is rendered as the canonical text the server's input
//! function understands, so parameters work for every column type, including
//! extension types (PostGIS, citext, hstore ...), without client side binary
//! encoders. `NULL` is sent as a SQL NULL.

use crate::decode::{date_text, pg_float_text, quote_array_element, time_text, timestamp_text};
use bytes::BytesMut;
use dbiji_core::Value;
use postgres_types::{Format, IsNull, Kind, ToSql, Type, to_sql_checked};
use std::borrow::Cow;
use std::error::Error;
use std::fmt;

/// A [`Value`] bound as a text-format parameter.
pub(crate) struct TextParam<'a>(pub &'a Value);

impl fmt::Debug for TextParam<'_> {
    // Never print parameter values: they may contain personal data and
    // tokio-postgres logs parameters at debug level.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.0.variant_name())
    }
}

impl ToSql for TextParam<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match param_text(self.0, ty) {
            None => Ok(IsNull::Yes),
            Some(t) => {
                out.extend_from_slice(t.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

fn is_int_type(ty: &Type) -> bool {
    matches!(ty.name(), "int2" | "int4" | "int8" | "oid" | "numeric" | "float4" | "float8")
}

/// Text representation of `v` for a parameter of type `ty`; `None` = SQL NULL.
pub(crate) fn param_text<'a>(v: &'a Value, ty: &Type) -> Option<Cow<'a, str>> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => {
            if is_int_type(ty) {
                Cow::Borrowed(if *b { "1" } else { "0" })
            } else {
                Cow::Borrowed(if *b { "t" } else { "f" })
            }
        }
        Value::Int(i) => {
            if ty.name() == "bool" {
                Cow::Borrowed(if *i != 0 { "t" } else { "f" })
            } else {
                Cow::Owned(i.to_string())
            }
        }
        Value::UInt(u) => Cow::Owned(u.to_string()),
        Value::Float(f) => Cow::Owned(pg_float_text(*f)),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) => Cow::Borrowed(s),
        Value::Bytes(b) => Cow::Owned(format!("\\x{}", hex::encode(b))),
        Value::Date(d) => Cow::Owned(date_text(*d)),
        Value::Time(t) => Cow::Owned(time_text(*t)),
        Value::DateTime(dt) => Cow::Owned(timestamp_text(*dt)),
        Value::DateTimeTz(dt) => {
            let base = timestamp_text(dt.naive_local());
            let off = offset_param(dt.offset().local_minus_utc());
            Cow::Owned(match base.strip_suffix(" BC") {
                Some(b) => format!("{b}{off} BC"),
                None => format!("{base}{off}"),
            })
        }
        Value::Uuid(u) => Cow::Owned(u.hyphenated().to_string()),
        Value::Array(items) => {
            let elem = match ty.kind() {
                Kind::Array(e) => e.clone(),
                _ => ty.clone(),
            };
            Cow::Owned(array_literal(items, &elem))
        }
    })
}

fn offset_param(east: i32) -> String {
    let sign = if east < 0 { '-' } else { '+' };
    let a = east.unsigned_abs();
    let (h, m, s) = (a / 3600, (a / 60) % 60, a % 60);
    if s != 0 { format!("{sign}{h:02}:{m:02}:{s:02}") } else { format!("{sign}{h:02}:{m:02}") }
}

/// PostgreSQL array literal (`{1,NULL,"a b"}`) for a list of values.
pub(crate) fn array_literal(items: &[Value], elem: &Type) -> String {
    let mut out = String::from("{");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Value::Null => out.push_str("NULL"),
            Value::Array(inner) => out.push_str(&array_literal(inner, elem)),
            other => {
                let t = param_text(other, elem).unwrap_or(Cow::Borrowed(""));
                out.push_str(&quote_array_element(&t));
            }
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    fn txt(v: &Value, ty: &Type) -> Option<String> {
        param_text(v, ty).map(|c| c.into_owned())
    }

    #[test]
    fn scalar_texts() {
        assert_eq!(txt(&Value::Null, &Type::TEXT), None);
        assert_eq!(txt(&Value::Bool(true), &Type::BOOL).as_deref(), Some("t"));
        assert_eq!(txt(&Value::Bool(true), &Type::INT4).as_deref(), Some("1"));
        assert_eq!(txt(&Value::Int(0), &Type::BOOL).as_deref(), Some("f"));
        assert_eq!(txt(&Value::Float(f64::INFINITY), &Type::FLOAT8).as_deref(), Some("Infinity"));
        assert_eq!(txt(&Value::bytes(vec![1u8, 0xab]), &Type::BYTEA).as_deref(), Some("\\x01ab"));
        let d = NaiveDate::from_ymd_opt(-43, 3, 15).unwrap();
        assert_eq!(txt(&Value::Date(d), &Type::DATE).as_deref(), Some("0044-03-15 BC"));
        let dt = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_micro_opt(3, 4, 5, 500_000).unwrap();
        let tz = dt.and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap();
        assert_eq!(txt(&Value::DateTimeTz(tz), &Type::TIMESTAMPTZ).as_deref(), Some("2024-01-02 03:04:05.5+07:00"));
        assert_eq!(txt(&Value::DateTime(dt), &Type::TIMESTAMP).as_deref(), Some("2024-01-02 03:04:05.5"));
    }

    #[test]
    fn arrays() {
        let a = Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)].into_boxed_slice());
        assert_eq!(txt(&a, &Type::INT4_ARRAY).as_deref(), Some("{1,NULL,3}"));
        let t = Value::Array(
            vec![Value::text("a b"), Value::text(""), Value::text("q\"x"), Value::text("null"), Value::text("plain")]
                .into_boxed_slice(),
        );
        assert_eq!(txt(&t, &Type::TEXT_ARRAY).as_deref(), Some("{\"a b\",\"\",\"q\\\"x\",\"null\",plain}"));
        let nested = Value::Array(
            vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)].into_boxed_slice()),
                Value::Array(vec![Value::Int(3), Value::Int(4)].into_boxed_slice()),
            ]
            .into_boxed_slice(),
        );
        assert_eq!(txt(&nested, &Type::INT4_ARRAY).as_deref(), Some("{{1,2},{3,4}}"));
        let b = Value::Array(vec![Value::bytes(vec![1u8])].into_boxed_slice());
        assert_eq!(txt(&b, &Type::BYTEA_ARRAY).as_deref(), Some("{\"\\\\x01\"}"));
    }

    #[test]
    fn debug_hides_values() {
        let v = Value::text("secret data");
        assert_eq!(format!("{:?}", TextParam(&v)), "<text>");
    }
}
