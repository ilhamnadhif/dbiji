//! Value normalisation, comparison and key ordering for data compare.

use super::DataCompareOptions;
use dbiji_core::{DataKind, Value};
use std::cmp::Ordering;

/// Exact decimal number: `digits * 10^exp` (no leading / trailing zeros in
/// `digits`; zero has empty digits and is never negative).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decimal {
    neg: bool,
    digits: String,
    exp: i64,
}

impl Decimal {
    pub fn parse(s: &str) -> Option<Decimal> {
        let s = s.trim();
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if s.is_empty() {
            return None;
        }
        let (mantissa, exp) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
            None => (s, 0),
        };
        let (int, frac) = match mantissa.split_once('.') {
            Some((a, b)) => (a, b),
            None => (mantissa, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut digits = format!("{int}{frac}");
        let mut exp = exp.checked_sub(frac.len() as i64)?;
        let lead = digits.len() - digits.trim_start_matches('0').len();
        digits.drain(..lead);
        while digits.ends_with('0') {
            digits.pop();
            exp += 1;
        }
        if digits.is_empty() {
            return Some(Decimal { neg: false, digits, exp: 0 });
        }
        Some(Decimal { neg, digits, exp })
    }

    fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    fn cmp_magnitude(&self, other: &Decimal) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let msd_a = self.digits.len() as i64 + self.exp;
        let msd_b = other.digits.len() as i64 + other.exp;
        msd_a.cmp(&msd_b).then_with(|| {
            let n = self.digits.len().max(other.digits.len());
            let pa = format!("{:0<n$}", self.digits);
            let pb = format!("{:0<n$}", other.digits);
            pa.cmp(&pb)
        })
    }

    /// Canonical text, used for hashing.
    pub fn canonical(&self) -> String {
        if self.is_zero() {
            return "0".into();
        }
        format!("{}{}e{}", if self.neg { "-" } else { "" }, self.digits, self.exp)
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.neg, other.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.cmp_magnitude(other),
            (true, true) => other.cmp_magnitude(self),
        }
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Number for comparisons: exact decimals plus the float specials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Num {
    NegInf,
    Dec(Decimal),
    PosInf,
    NaN,
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(n: &Num) -> u8 {
            match n {
                Num::NegInf => 0,
                Num::Dec(_) => 1,
                Num::PosInf => 2,
                Num::NaN => 3,
            }
        }
        match (self, other) {
            (Num::Dec(a), Num::Dec(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Numeric view of a value (integers, floats, decimals, booleans as 0/1).
pub(crate) fn numeric(v: &Value) -> Option<Num> {
    match v {
        Value::Int(i) => Decimal::parse(&i.to_string()).map(Num::Dec),
        Value::UInt(u) => Decimal::parse(&u.to_string()).map(Num::Dec),
        Value::Bool(b) => Decimal::parse(if *b { "1" } else { "0" }).map(Num::Dec),
        Value::Float(f) if f.is_nan() => Some(Num::NaN),
        Value::Float(f) if f.is_infinite() => Some(if *f > 0.0 { Num::PosInf } else { Num::NegInf }),
        Value::Float(f) => Decimal::parse(&format!("{f:e}")).map(Num::Dec),
        Value::Decimal(s) => match s.trim().to_ascii_lowercase().as_str() {
            "nan" => Some(Num::NaN),
            "infinity" | "+infinity" | "inf" => Some(Num::PosInf),
            "-infinity" | "-inf" => Some(Num::NegInf),
            _ => Decimal::parse(s).map(Num::Dec),
        },
        _ => None,
    }
}

fn is_numeric_value(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) | Value::Bool(_))
}

fn norm_text(s: &str, o: &DataCompareOptions) -> String {
    let s = if o.trim_text { s.trim_end() } else { s };
    if o.ignore_case { s.to_lowercase() } else { s.to_string() }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
        other => other.as_f64(),
    }
}

/// Equality of two cell values under the comparison options.
pub(crate) fn values_equal(a: &Value, b: &Value, o: &DataCompareOptions) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Null, _) | (_, Null) => false,
        _ if is_numeric_value(a) && is_numeric_value(b) => numbers_equal(a, b, o),
        // numbers delivered as text by one side
        (Text(t), n) | (n, Text(t)) if is_numeric_value(n) && !matches!(n, Bool(_)) && self::Decimal::parse(t).is_some() => numbers_equal(&Value::Decimal(t.clone()), n, o),
        (Bool(x), Text(t)) | (Text(t), Bool(x)) => dbiji_core::value::parse_bool(t) == Some(*x),
        (Text(x), Text(y)) => norm_text(x, o) == norm_text(y, o),
        (Json(x), Json(y)) | (Json(x), Text(y)) | (Text(y), Json(x)) => match (serde_json::from_str::<serde_json::Value>(x), serde_json::from_str::<serde_json::Value>(y)) {
            (Ok(p), Ok(q)) => p == q,
            _ => norm_text(x, o) == norm_text(y, o),
        },
        (Bytes(x), Bytes(y)) => x == y,
        (Date(x), Date(y)) => x == y,
        (Time(x), Time(y)) => x == y,
        (DateTime(x), DateTime(y)) => x == y,
        (DateTimeTz(x), DateTimeTz(y)) => x == y,
        (Date(d), DateTime(dt)) | (DateTime(dt), Date(d)) => d.and_hms_opt(0, 0, 0).is_some_and(|x| x == *dt),
        (DateTime(n), DateTimeTz(z)) | (DateTimeTz(z), DateTime(n)) => z.naive_local() == *n,
        (Uuid(x), Uuid(y)) => x == y,
        (Uuid(u), Text(t)) | (Text(t), Uuid(u)) => uuid::Uuid::parse_str(t.trim()).is_ok_and(|p| p == *u),
        (Array(x), Array(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| values_equal(p, q, o)),
        _ => norm_text(&a.to_text(), o) == norm_text(&b.to_text(), o),
    }
}

fn numbers_equal(a: &Value, b: &Value, o: &DataCompareOptions) -> bool {
    if o.numeric_tolerance > 0.0
        && let (Some(x), Some(y)) = (as_f64(a), as_f64(b))
    {
        if x.is_nan() && y.is_nan() {
            return true;
        }
        return (x - y).abs() <= o.numeric_tolerance || x == y;
    }
    match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.to_text() == b.to_text(),
    }
}

/// Logical kinds whose `ORDER BY` order is the same in every database
/// (collation independent), so a merge join can be used.
pub(crate) fn orderable_kind(kind: DataKind, uuid_ok: bool) -> bool {
    match kind {
        DataKind::Int | DataKind::Float | DataKind::Decimal | DataKind::Bool => true,
        DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz | DataKind::Time => true,
        DataKind::Uuid => uuid_ok,
        _ => false,
    }
}

fn family(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => 1,
        Value::Date(_) | Value::DateTime(_) | Value::DateTimeTz(_) => 2,
        Value::Time(_) => 3,
        Value::Uuid(_) => 4,
        Value::Text(_) | Value::Json(_) => 5,
        Value::Bytes(_) => 6,
        Value::Array(_) => 7,
    }
}

/// Total order of key values consistent with `ORDER BY` for orderable kinds.
pub(crate) fn key_cmp(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    if let (Some(x), Some(y)) = (numeric(a), numeric(b)) {
        return x.cmp(&y);
    }
    let ts = |v: &Value| -> Option<chrono::NaiveDateTime> {
        match v {
            Date(d) => d.and_hms_opt(0, 0, 0),
            DateTime(dt) => Some(*dt),
            DateTimeTz(z) => Some(z.naive_utc()),
            _ => None,
        }
    };
    if let (Some(x), Some(y)) = (ts(a), ts(b)) {
        return x.cmp(&y);
    }
    match (a, b) {
        (Time(x), Time(y)) => x.cmp(y),
        (Uuid(x), Uuid(y)) => x.cmp(y),
        (Text(x), Text(y)) => x.cmp(y),
        (Bytes(x), Bytes(y)) => x.cmp(y),
        _ => family(a).cmp(&family(b)).then_with(|| a.to_text().cmp(&b.to_text())),
    }
}

/// Lexicographic comparison of composite keys.
pub(crate) fn keys_cmp(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = key_cmp(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Canonical hashable text of a key value.
fn key_part(v: &Value, o: &DataCompareOptions) -> String {
    use Value::*;
    if let Some(n) = numeric(v) {
        return match n {
            Num::Dec(d) => format!("n:{}", d.canonical()),
            Num::NaN => "n:nan".into(),
            Num::PosInf => "n:inf".into(),
            Num::NegInf => "n:-inf".into(),
        };
    }
    match v {
        Null => "\u{0}".into(),
        Text(t) => format!("s:{}", norm_text(t, o)),
        Uuid(u) => format!("s:{}", u.hyphenated()),
        Json(j) => match serde_json::from_str::<serde_json::Value>(j) {
            Ok(p) => format!("j:{p}"),
            Err(_) => format!("j:{j}"),
        },
        Bytes(b) => format!("b:{}", hex_string(b)),
        DateTimeTz(z) => format!("t:{}", z.naive_utc()),
        Date(_) | DateTime(_) | Time(_) => format!("t:{}", v.to_text()),
        Array(items) => format!("a:[{}]", items.iter().map(|i| key_part(i, o)).collect::<Vec<_>>().join(",")),
        other => format!("x:{}", other.to_text()),
    }
}

fn hex_string(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Hash key of a composite key.
pub(crate) fn key_repr(key: &[Value], o: &DataCompareOptions) -> String {
    key.iter().map(|v| key_part(v, o)).collect::<Vec<_>>().join("\u{1f}")
}

/// [`key_repr`] with text read as what it spells (`"1"` → the number 1, a UUID's
/// text → the UUID), the way [`values_equal`] compares text with numbers and
/// UUIDs: a TEXT key on one side matches an INTEGER or UUID key on the other.
pub(crate) fn cross_key_repr(key: &[Value], o: &DataCompareOptions) -> String {
    key.iter()
        .map(|v| match v {
            Value::Text(t) if Decimal::parse(t.trim()).is_some() => key_part(&Value::Decimal(t.trim().into()), o),
            Value::Text(t) => match uuid::Uuid::parse_str(t.trim()) {
                Ok(u) => key_part(&Value::Uuid(u), o),
                Err(_) => key_part(v, o),
            },
            other => key_part(other, o),
        })
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn o() -> DataCompareOptions {
        DataCompareOptions::default()
    }

    #[test]
    fn decimals() {
        let d = |s: &str| Decimal::parse(s).unwrap();
        assert_eq!(d("1"), d("1.0"));
        assert_eq!(d("1.50"), d("001.5"));
        assert_eq!(d("1e3"), d("1000"));
        assert_eq!(d("-0.0"), d("0"));
        assert!(d("2") > d("1.999"));
        assert!(d("-2") < d("-1.5"));
        assert!(d("10") > d("9.99"));
        assert!(d("0.001") < d("0.01"));
        assert!(Decimal::parse("1.2.3").is_none());
        assert!(Decimal::parse("abc").is_none());
        assert_eq!(d("12.3400").canonical(), "1234e-2");
    }

    #[test]
    fn value_equality() {
        assert!(values_equal(&Value::Int(1), &Value::decimal("1.0"), &o()));
        assert!(values_equal(&Value::Float(0.1), &Value::decimal("0.1"), &o()));
        assert!(!values_equal(&Value::Int(1), &Value::decimal("1.01"), &o()));
        let tol = DataCompareOptions { numeric_tolerance: 0.05, ..o() };
        assert!(values_equal(&Value::Float(1.0), &Value::decimal("1.04"), &tol));
        assert!(values_equal(&Value::Bool(true), &Value::Int(1), &o()));
        assert!(!values_equal(&Value::text("a "), &Value::text("a"), &o()));
        let trim = DataCompareOptions { trim_text: true, ignore_case: true, ..o() };
        assert!(values_equal(&Value::text("Abc  "), &Value::text("aBC"), &trim));
        assert!(!values_equal(&Value::text(" abc"), &Value::text("abc"), &trim));
        assert!(values_equal(&Value::json(r#"{"a":1,"b":[1,2]}"#), &Value::json(r#"{ "b": [1, 2], "a": 1 }"#), &o()));
        let u = uuid::Uuid::new_v4();
        assert!(values_equal(&Value::Uuid(u), &Value::text(u.to_string().to_uppercase()), &o()));
        let d = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        assert!(values_equal(&Value::Date(d), &Value::DateTime(d.and_hms_opt(0, 0, 0).unwrap()), &o()));
        assert!(values_equal(&Value::text("42"), &Value::Int(42), &o()));
        assert!(!values_equal(&Value::Null, &Value::text(""), &o()));
    }

    #[test]
    fn key_order() {
        let mut keys = [Value::decimal("10"), Value::Int(2), Value::Float(2.5), Value::UInt(1), Value::Float(f64::NAN)];
        keys.sort_by(key_cmp);
        assert_eq!(keys[0], Value::UInt(1));
        assert_eq!(keys[3], Value::decimal("10"));
        assert!(matches!(keys[4], Value::Float(f) if f.is_nan()));
        assert_eq!(key_repr(&[Value::Int(5)], &o()), key_repr(&[Value::decimal("5.00")], &o()));
        let ci = DataCompareOptions { ignore_case: true, ..o() };
        assert_eq!(key_repr(&[Value::text("ABC")], &ci), key_repr(&[Value::text("abc")], &ci));
        assert_ne!(key_repr(&[Value::text("ABC")], &o()), key_repr(&[Value::text("abc")], &o()));
        // text spelling a number meets the number (not another spelling of it as text)
        assert_eq!(cross_key_repr(&[Value::text("1")], &o()), key_repr(&[Value::Int(1)], &o()));
        assert_ne!(key_repr(&[Value::text("01")], &o()), key_repr(&[Value::text("1")], &o()));
    }
}
