//! Column type inference from sample rows (FR-T3 "type guess from sample rows").

use crate::convert::parse_tz_strict;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use dbiji_core::value::{is_decimal_text, parse_date, parse_datetime, parse_time};
use dbiji_core::{ColumnMeta, DataKind, Row, Value};
use serde::{Deserialize, Serialize};

/// Options of [`infer_columns`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InferOptions {
    /// Additional chrono patterns recognised as dates, e.g. `%d/%m/%Y`.
    pub date_formats: Vec<String>,
    /// Additional chrono patterns recognised as timestamps.
    pub datetime_formats: Vec<String>,
    /// Decimal separator of numbers in the input.
    pub decimal_separator: char,
    /// Grouping separator of numbers in the input.
    pub thousands_separator: Option<char>,
    /// Texts treated as missing values (in addition to empty text).
    pub null_values: Vec<String>,
    /// Recognise `true/false/yes/no/t/f/y/n/on/off` as booleans (`0`/`1` stay integers).
    pub detect_bool: bool,
    /// Recognise JSON objects and arrays.
    pub detect_json: bool,
    /// Recognise hyphenated UUIDs.
    pub detect_uuid: bool,
}

impl Default for InferOptions {
    fn default() -> Self {
        InferOptions {
            date_formats: Vec::new(),
            datetime_formats: Vec::new(),
            decimal_separator: '.',
            thousands_separator: None,
            null_values: Vec::new(),
            detect_bool: true,
            detect_json: true,
            detect_uuid: true,
        }
    }
}

/// Inferred description of one column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InferredColumn {
    /// Column name.
    pub name: String,
    /// Logical type.
    pub kind: DataKind,
    /// A missing / empty value was seen.
    pub nullable: bool,
    /// Longest text representation in characters.
    pub max_len: usize,
    /// Digits: total digits of decimals, maximum digits of integers.
    pub precision: Option<u32>,
    /// Fraction digits of decimals.
    pub scale: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Guess {
    Null,
    Bool,
    Int { digits: u32 },
    Decimal { int_digits: u32, scale: u32 },
    Float,
    Date,
    DateTime,
    DateTimeTz,
    Time,
    Uuid,
    Json,
    Binary,
    Text,
}

fn merge(a: Guess, b: Guess) -> Guess {
    use Guess::*;
    match (a, b) {
        (Null, x) | (x, Null) => x,
        (Int { digits: x }, Int { digits: y }) => Int { digits: x.max(y) },
        (Int { digits }, Decimal { int_digits, scale }) | (Decimal { int_digits, scale }, Int { digits }) => Decimal { int_digits: int_digits.max(digits), scale },
        (Decimal { int_digits: a1, scale: s1 }, Decimal { int_digits: a2, scale: s2 }) => Decimal { int_digits: a1.max(a2), scale: s1.max(s2) },
        (Int { .. } | Decimal { .. } | Float, Int { .. } | Decimal { .. } | Float) => Float,
        (Date, DateTime) | (DateTime, Date) => DateTime,
        (x, y) if x == y => x,
        _ => Text,
    }
}

fn count_digits(s: &str) -> u32 {
    s.chars().filter(char::is_ascii_digit).count() as u32
}

/// Classify a text value.
pub(crate) fn classify_text(raw: &str, opts: &InferOptions) -> Guess {
    let t = raw.trim();
    if t.is_empty() || opts.null_values.iter().any(|n| n == raw) {
        return Guess::Null;
    }
    // Numbers (with the configured separators).
    let normalized = {
        let mut s: String = match opts.thousands_separator {
            Some(sep) => t.chars().filter(|c| *c != sep).collect(),
            None => t.to_string(),
        };
        if opts.decimal_separator != '.' {
            if s.contains('.') && opts.thousands_separator != Some('.') {
                // A '.' that is not a configured separator makes it non-numeric.
                s.push('x');
            }
            s = s.replace(opts.decimal_separator, ".");
        }
        s
    };
    let unsigned = normalized.strip_prefix(['-', '+']).unwrap_or(&normalized);
    let leading_zero = unsigned.len() > 1 && unsigned.starts_with('0') && !unsigned.starts_with("0.");
    if !unsigned.is_empty() && unsigned.chars().all(|c| c.is_ascii_digit()) {
        if leading_zero {
            return Guess::Text; // codes like 007 must keep their zeros
        }
        return if normalized.parse::<i64>().is_ok() { Guess::Int { digits: count_digits(unsigned) } } else { Guess::Decimal { int_digits: count_digits(unsigned), scale: 0 } };
    }
    if is_decimal_text(&normalized) && !leading_zero {
        let lower = unsigned.to_ascii_lowercase();
        if lower.contains('e') || lower == "nan" || lower == "infinity" {
            return Guess::Float;
        }
        let (int_part, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        return Guess::Decimal { int_digits: count_digits(int_part), scale: frac.len() as u32 };
    }
    if opts.detect_bool && matches!(t.to_ascii_lowercase().as_str(), "true" | "false" | "t" | "f" | "yes" | "no" | "y" | "n" | "on" | "off") {
        return Guess::Bool;
    }
    if opts.date_formats.iter().any(|f| NaiveDate::parse_from_str(t, f).is_ok()) {
        return Guess::Date;
    }
    if opts.datetime_formats.iter().any(|f| NaiveDateTime::parse_from_str(t, f).is_ok()) {
        return Guess::DateTime;
    }
    if t.len() >= 8 && t.as_bytes()[0].is_ascii_digit() {
        if parse_date(t).is_some() {
            return Guess::Date;
        }
        if parse_tz_strict(t).is_some() {
            return Guess::DateTimeTz;
        }
        if parse_datetime(t).is_some() {
            return Guess::DateTime;
        }
    }
    if t.contains(':') && t.len() <= 18 && parse_time(t).is_some() {
        return Guess::Time;
    }
    if opts.detect_uuid && t.len() == 36 && uuid::Uuid::parse_str(t).is_ok() {
        return Guess::Uuid;
    }
    if opts.detect_json && (t.starts_with('{') || t.starts_with('[')) && crate::sink::is_valid_json(t) {
        return Guess::Json;
    }
    Guess::Text
}

fn classify(v: &Value, opts: &InferOptions) -> Guess {
    match v {
        Value::Null => Guess::Null,
        Value::Text(s) => classify_text(s, opts),
        Value::Bool(_) => Guess::Bool,
        Value::Int(i) => Guess::Int { digits: count_digits(&i.to_string()) },
        Value::UInt(u) => {
            if i64::try_from(*u).is_ok() {
                Guess::Int { digits: count_digits(&u.to_string()) }
            } else {
                Guess::Decimal { int_digits: count_digits(&u.to_string()), scale: 0 }
            }
        }
        Value::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 => Guess::Int { digits: count_digits(&format!("{}", f.abs() as i64)) },
        Value::Float(_) => Guess::Float,
        Value::Decimal(s) => match classify_text(s, &InferOptions::default()) {
            g @ (Guess::Int { .. } | Guess::Decimal { .. } | Guess::Float) => g,
            _ => Guess::Text,
        },
        Value::Bytes(_) => Guess::Binary,
        Value::Date(_) => Guess::Date,
        Value::Time(_) => Guess::Time,
        Value::DateTime(dt) if dt.time() == NaiveTime::MIN => Guess::Date,
        Value::DateTime(_) => Guess::DateTime,
        Value::DateTimeTz(_) => Guess::DateTimeTz,
        Value::Uuid(_) => Guess::Uuid,
        Value::Json(_) | Value::Array(_) => Guess::Json,
    }
}

/// Guess the type of every column from sample rows.
///
/// Integers with leading zeros stay text, `0`/`1` are integers, mixed
/// integer/decimal columns become decimals with enough precision, mixed
/// date/timestamp columns timestamps; anything inconsistent is text.
/// Columns without any value become nullable text.
pub fn infer_columns(columns: &[ColumnMeta], sample: &[Row], opts: &InferOptions) -> Vec<InferredColumn> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut guess = Guess::Null;
            let mut nullable = false;
            let mut max_len = 0usize;
            for row in sample {
                let v = row.get(i).unwrap_or(&Value::Null);
                let g = classify(v, opts);
                if g == Guess::Null {
                    nullable = true;
                    continue;
                }
                max_len = max_len.max(v.to_text().chars().count());
                guess = merge(guess, g);
            }
            let (kind, precision, scale) = match guess {
                Guess::Null => {
                    nullable = true;
                    (DataKind::Text, None, None)
                }
                Guess::Bool => (DataKind::Bool, None, None),
                Guess::Int { digits } => (DataKind::Int, Some(digits), None),
                Guess::Decimal { int_digits, scale } => (DataKind::Decimal, Some((int_digits + scale).max(1)), Some(scale)),
                Guess::Float => (DataKind::Float, None, None),
                Guess::Date => (DataKind::Date, None, None),
                Guess::DateTime => (DataKind::DateTime, None, None),
                Guess::DateTimeTz => (DataKind::DateTimeTz, None, None),
                Guess::Time => (DataKind::Time, None, None),
                Guess::Uuid => (DataKind::Uuid, None, None),
                Guess::Json => (DataKind::Json, None, None),
                Guess::Binary => (DataKind::Binary, None, None),
                Guess::Text => (DataKind::Text, None, None),
            };
            InferredColumn { name: c.name.clone(), kind, nullable, max_len, precision, scale }
        })
        .collect()
}

/// ANSI type name for an inferred column (`VARCHAR(100)`, `NUMERIC(12,2)` ...),
/// to be mapped to a dialect with [`DdlGenerator::map_type`](dbiji_core::ddl::DdlGenerator::map_type)
/// using [`DbKind::Generic`](dbiji_core::DbKind::Generic) as the source.
pub fn generic_type_name(col: &InferredColumn) -> String {
    match col.kind {
        DataKind::Bool => "BOOLEAN".into(),
        DataKind::Int => {
            if col.precision.is_some_and(|p| p <= 9) {
                "INTEGER".into()
            } else {
                "BIGINT".into()
            }
        }
        DataKind::Decimal => match (col.precision, col.scale) {
            (Some(p), Some(s)) if p <= 38 => format!("NUMERIC({p},{s})"),
            _ => "NUMERIC".into(),
        },
        DataKind::Float => "DOUBLE PRECISION".into(),
        DataKind::Date => "DATE".into(),
        DataKind::Time => "TIME".into(),
        DataKind::DateTime => "TIMESTAMP".into(),
        DataKind::DateTimeTz => "TIMESTAMP WITH TIME ZONE".into(),
        DataKind::Uuid => "UUID".into(),
        DataKind::Json => "JSON".into(),
        DataKind::Binary => "BLOB".into(),
        _ => {
            const SIZES: [usize; 9] = [10, 20, 50, 100, 255, 500, 1000, 2000, 4000];
            match SIZES.iter().find(|s| **s >= col.max_len.max(1)) {
                Some(n) => format!("VARCHAR({n})"),
                None => "TEXT".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer(values: &[&str]) -> InferredColumn {
        let cols = vec![ColumnMeta::new("c", "", DataKind::Text)];
        let rows: Vec<Row> = values.iter().map(|v| vec![if v.is_empty() { Value::Null } else { Value::text(*v) }]).collect();
        infer_columns(&cols, &rows, &InferOptions::default()).remove(0)
    }

    #[test]
    fn numbers() {
        assert_eq!(infer(&["1", "22", "-333"]).kind, DataKind::Int);
        assert_eq!(infer(&["1", "22", "-333"]).precision, Some(3));
        let d = infer(&["1.5", "22", "-333.25"]);
        assert_eq!((d.kind, d.precision, d.scale), (DataKind::Decimal, Some(5), Some(2)));
        assert_eq!(infer(&["1e5", "2"]).kind, DataKind::Float);
        assert_eq!(infer(&["007", "1"]).kind, DataKind::Text);
        assert_eq!(infer(&["0", "1"]).kind, DataKind::Int);
        assert_eq!(infer(&["0.5"]).kind, DataKind::Decimal);
        assert_eq!(infer(&["123456789012345678901234567890"]).kind, DataKind::Decimal);
    }

    #[test]
    fn temporal_and_other() {
        assert_eq!(infer(&["2024-01-02", "2024-12-31"]).kind, DataKind::Date);
        assert_eq!(infer(&["2024-01-02", "2024-12-31 10:00:00"]).kind, DataKind::DateTime);
        assert_eq!(infer(&["2024-01-02 10:00:00+07", "2024-01-02T10:00:00Z"]).kind, DataKind::DateTimeTz);
        assert_eq!(infer(&["10:30", "23:59:59"]).kind, DataKind::Time);
        assert_eq!(infer(&["true", "no"]).kind, DataKind::Bool);
        assert_eq!(infer(&["8c5f3c2e-1b2a-4d3e-9f00-112233445566"]).kind, DataKind::Uuid);
        assert_eq!(infer(&["{\"a\":1}", "[1,2]"]).kind, DataKind::Json);
        assert_eq!(infer(&["1", "x"]).kind, DataKind::Text);
        let t = infer(&["abc", "", "abcdef"]);
        assert_eq!((t.kind, t.nullable, t.max_len), (DataKind::Text, true, 6));
        assert_eq!(generic_type_name(&t), "VARCHAR(10)");
        let empty = infer(&["", ""]);
        assert_eq!((empty.kind, empty.nullable), (DataKind::Text, true));
    }

    #[test]
    fn localized_numbers_and_custom_dates() {
        let opts = InferOptions { decimal_separator: ',', thousands_separator: Some('.'), date_formats: vec!["%d/%m/%Y".into()], ..Default::default() };
        assert_eq!(classify_text("1.234,5", &opts), Guess::Decimal { int_digits: 4, scale: 1 });
        assert_eq!(classify_text("31/12/2024", &opts), Guess::Date);
        assert_eq!(classify_text("1.5", &InferOptions { decimal_separator: ',', ..Default::default() }), Guess::Text);
    }

    #[test]
    fn typed_values() {
        let cols = vec![ColumnMeta::new("n", "", DataKind::Other), ColumnMeta::new("d", "", DataKind::Other)];
        let d = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let rows = vec![vec![Value::Float(1.0), Value::DateTime(d.and_time(NaiveTime::MIN))], vec![Value::Float(2.5), Value::Date(d)]];
        let r = infer_columns(&cols, &rows, &InferOptions::default());
        assert_eq!(r[0].kind, DataKind::Float);
        assert_eq!(r[1].kind, DataKind::Date);
        assert_eq!(generic_type_name(&r[0]), "DOUBLE PRECISION");
    }
}
