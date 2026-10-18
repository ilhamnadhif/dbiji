//! Value formatting shared by the text based export formats.
//!
//! With the default [`ValueFormat`] every value is rendered as its canonical
//! text ([`Value::to_text`]), which is what round trips through import. The
//! options let users produce locale friendly output instead (date patterns,
//! decimal comma, thousands grouping ...).

use base64::Engine;
use dbiji_core::Value;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt::Write as _;

/// How binary values are rendered as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BinaryEncoding {
    /// Lowercase hexadecimal without prefix (canonical text, imported back as binary).
    #[default]
    Hex,
    /// Standard base64 with padding.
    Base64,
    /// Bytes interpreted as UTF-8 (lossy).
    Text,
}

impl BinaryEncoding {
    /// Render `bytes`.
    pub fn encode(self, bytes: &[u8]) -> String {
        match self {
            BinaryEncoding::Hex => hex::encode(bytes),
            BinaryEncoding::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
            BinaryEncoding::Text => String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

/// Formatting applied to values by text based export formats
/// (CSV, TSV, Markdown, HTML, XML, TXT, DbUnit). `None` options keep the
/// canonical representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValueFormat {
    /// Text written for NULL by formats that have no NULL notation of their
    /// own (Markdown, HTML, TXT). CSV uses [`CsvOptions::null_string`](crate::CsvOptions::null_string).
    pub null_text: String,
    /// chrono `strftime` pattern for dates, e.g. `%d.%m.%Y`.
    pub date_format: Option<String>,
    /// Pattern for times of day, e.g. `%H:%M`.
    pub time_format: Option<String>,
    /// Pattern for timestamps without time zone.
    pub datetime_format: Option<String>,
    /// Pattern for timestamps with time zone (`%z` prints the offset). When
    /// unset but `datetime_format` is set, that pattern is used and the offset appended.
    pub timestamptz_format: Option<String>,
    /// Decimal separator for numbers; `None` keeps `.`.
    pub decimal_separator: Option<char>,
    /// Thousands grouping separator; `None` disables grouping.
    pub thousands_separator: Option<char>,
    /// Maximum fraction digits of floating point numbers (rounded, trailing zeros removed).
    pub float_decimals: Option<usize>,
    /// Rendering of binary values.
    pub binary: BinaryEncoding,
    /// Text for `true`.
    pub true_text: String,
    /// Text for `false`.
    pub false_text: String,
    /// Trim leading and trailing whitespace of text values.
    pub trim: bool,
    /// Truncate text values to at most this many characters.
    pub max_length: Option<usize>,
}

impl Default for ValueFormat {
    fn default() -> Self {
        ValueFormat {
            null_text: String::new(),
            date_format: None,
            time_format: None,
            datetime_format: None,
            timestamptz_format: None,
            decimal_separator: None,
            thousands_separator: None,
            float_decimals: None,
            binary: BinaryEncoding::Hex,
            true_text: "true".into(),
            false_text: "false".into(),
            trim: false,
            max_length: None,
        }
    }
}

impl ValueFormat {
    /// True when the options only produce canonical text (fast path).
    pub fn is_canonical(&self) -> bool {
        self.date_format.is_none()
            && self.time_format.is_none()
            && self.datetime_format.is_none()
            && self.timestamptz_format.is_none()
            && self.decimal_separator.is_none_or(|c| c == '.')
            && self.thousands_separator.is_none()
            && self.float_decimals.is_none()
            && self.binary == BinaryEncoding::Hex
            && self.true_text == "true"
            && self.false_text == "false"
            && !self.trim
            && self.max_length.is_none()
    }

    fn number_text<'a>(&self, s: Cow<'a, str>) -> Cow<'a, str> {
        let dec = self.decimal_separator.filter(|c| *c != '.');
        if dec.is_none() && self.thousands_separator.is_none() {
            return s;
        }
        Cow::Owned(localize_number(&s, dec.unwrap_or('.'), self.thousands_separator))
    }

    fn text_value<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let s = if self.trim { s.trim() } else { s };
        match self.max_length {
            Some(max) if s.chars().count() > max => Cow::Owned(s.chars().take(max).collect()),
            _ => Cow::Borrowed(s),
        }
    }
}

/// Render `value` as text according to `fmt`. `Null` becomes `fmt.null_text`.
pub fn format_value<'a>(value: &'a Value, fmt: &'a ValueFormat) -> Cow<'a, str> {
    match value {
        Value::Null => Cow::Borrowed(fmt.null_text.as_str()),
        Value::Bool(b) => Cow::Borrowed(if *b { fmt.true_text.as_str() } else { fmt.false_text.as_str() }),
        Value::Int(_) | Value::UInt(_) | Value::Decimal(_) => fmt.number_text(value.to_text()),
        Value::Float(f) => {
            let text = match fmt.float_decimals {
                Some(d) if f.is_finite() => {
                    let mut s = format!("{f:.d$}");
                    if s.contains('.') {
                        let trimmed = s.trim_end_matches('0').trim_end_matches('.').len();
                        s.truncate(trimmed);
                    }
                    if s == "-0" {
                        s = "0".into();
                    }
                    Cow::Owned(s)
                }
                _ => value.to_text(),
            };
            if f.is_finite() { fmt.number_text(text) } else { text }
        }
        Value::Text(s) | Value::Json(s) => fmt.text_value(s),
        Value::Bytes(b) => match fmt.binary {
            BinaryEncoding::Hex => value.to_text(),
            other => Cow::Owned(other.encode(b)),
        },
        Value::Date(d) => match &fmt.date_format {
            Some(p) => Cow::Owned(safe_format(|out| write!(out, "{}", d.format(p)))),
            None => value.to_text(),
        },
        Value::Time(t) => match &fmt.time_format {
            Some(p) => Cow::Owned(safe_format(|out| write!(out, "{}", t.format(p)))),
            None => value.to_text(),
        },
        Value::DateTime(dt) => match &fmt.datetime_format {
            Some(p) => Cow::Owned(safe_format(|out| write!(out, "{}", dt.format(p)))),
            None => value.to_text(),
        },
        Value::DateTimeTz(dt) => match (&fmt.timestamptz_format, &fmt.datetime_format) {
            (Some(p), _) => Cow::Owned(safe_format(|out| write!(out, "{}", dt.format(p)))),
            (None, Some(p)) => Cow::Owned(safe_format(|out| write!(out, "{}{}", dt.naive_local().format(p), dt.format("%:z")))),
            (None, None) => value.to_text(),
        },
        Value::Uuid(_) | Value::Array(_) => value.to_text(),
    }
}

/// Format with a chrono pattern without panicking on invalid patterns
/// (chrono's `Display` returns an error for unknown specifiers).
fn safe_format(f: impl FnOnce(&mut String) -> std::fmt::Result) -> String {
    let mut out = String::new();
    match f(&mut out) {
        Ok(()) => out,
        Err(_) => "<invalid format>".into(),
    }
}

/// Replace the decimal point and insert grouping separators in a plain
/// number text (`-1234567.5` → `-1.234.567,5`). Texts that are not plain
/// numbers (exponents, NaN) are only given the decimal separator.
pub(crate) fn localize_number(s: &str, decimal: char, thousands: Option<char>) -> String {
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac) = match body.find('.') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let plain = int_part.chars().all(|c| c.is_ascii_digit()) && frac.is_none_or(|f| f.chars().all(|c| c.is_ascii_digit()));
    if !plain {
        return s.replace('.', &decimal.to_string());
    }
    let mut out = String::with_capacity(s.len() + int_part.len() / 3 + 1);
    out.push_str(sign);
    match thousands {
        Some(sep) => {
            let len = int_part.len();
            for (i, c) in int_part.chars().enumerate() {
                if i > 0 && (len - i) % 3 == 0 {
                    out.push(sep);
                }
                out.push(c);
            }
        }
        None => out.push_str(int_part),
    }
    if let Some(f) = frac {
        out.push(decimal);
        out.push_str(f);
    }
    out
}

/// True when the value is rendered as a number (used for alignment and CSV quoting).
pub(crate) fn is_numeric_value(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    #[test]
    fn canonical_by_default() {
        let f = ValueFormat::default();
        assert!(f.is_canonical());
        let dt = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_micro_opt(1, 2, 3, 400).unwrap();
        for v in [
            Value::Int(-5),
            Value::Float(0.1),
            Value::decimal("12.50"),
            Value::DateTime(dt),
            Value::bytes(vec![1u8, 255]),
            Value::text(" x "),
        ] {
            assert_eq!(format_value(&v, &f), v.to_text());
        }
        assert_eq!(format_value(&Value::Null, &f), "");
    }

    #[test]
    fn custom_formats() {
        let f = ValueFormat {
            null_text: "NULL".into(),
            date_format: Some("%d.%m.%Y".into()),
            datetime_format: Some("%d.%m.%Y %H:%M".into()),
            decimal_separator: Some(','),
            thousands_separator: Some('.'),
            float_decimals: Some(2),
            binary: BinaryEncoding::Base64,
            true_text: "yes".into(),
            false_text: "no".into(),
            trim: true,
            max_length: Some(3),
            ..Default::default()
        };
        assert!(!f.is_canonical());
        let d = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        assert_eq!(format_value(&Value::Date(d), &f), "31.01.2024");
        let tz = d.and_hms_opt(10, 30, 0).unwrap().and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap();
        assert_eq!(format_value(&Value::DateTimeTz(tz), &f), "31.01.2024 10:30+07:00");
        assert_eq!(format_value(&Value::decimal("-1234567.891"), &f), "-1.234.567,891");
        assert_eq!(format_value(&Value::Float(1234.5678), &f), "1.234,57");
        assert_eq!(format_value(&Value::Float(2.0), &f), "2");
        assert_eq!(format_value(&Value::Int(1000), &f), "1.000");
        assert_eq!(format_value(&Value::Bool(true), &f), "yes");
        assert_eq!(format_value(&Value::bytes(vec![0u8, 1, 2]), &f), "AAEC");
        assert_eq!(format_value(&Value::text("  abcdef "), &f), "abc");
        assert_eq!(format_value(&Value::Null, &f), "NULL");
        assert_eq!(format_value(&Value::Float(f64::NAN), &f), "NaN");
    }

    #[test]
    fn invalid_pattern_does_not_panic() {
        let f = ValueFormat { date_format: Some("%Q%".into()), ..Default::default() };
        let d = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        assert_eq!(format_value(&Value::Date(d), &f), "<invalid format>");
    }

    #[test]
    fn localize() {
        assert_eq!(localize_number("1234", ',', Some(' ')), "1 234");
        assert_eq!(localize_number("123", '.', Some(',')), "123");
        assert_eq!(localize_number("1.5e10", ',', Some('.')), "1,5e10");
    }
}
