//! Cell value formatting ("format data") with optional per-column overrides.

use dbiji_core::Value;
use dbiji_core::value::FormatOptions;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Per-column display overrides on top of the grid [`FormatOptions`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnFormat {
    /// Fixed number of fraction digits for numbers (pads with zeros).
    pub decimals: Option<usize>,
    /// Override the thousands separator setting.
    pub thousands_separator: Option<bool>,
    /// chrono pattern for dates.
    pub date_format: Option<String>,
    /// chrono pattern for times.
    pub time_format: Option<String>,
    /// chrono pattern for timestamps.
    pub datetime_format: Option<String>,
}

impl ColumnFormat {
    /// True when no override is set.
    pub fn is_empty(&self) -> bool {
        self == &ColumnFormat::default()
    }

    /// Effective options for this column (clones only the overridden fields' owners).
    pub fn apply(&self, base: &FormatOptions) -> FormatOptions {
        let mut o = base.clone();
        if let Some(t) = self.thousands_separator {
            o.thousands_separator = t;
        }
        if let Some(d) = &self.date_format {
            o.date_format = d.clone();
        }
        if let Some(t) = &self.time_format {
            o.time_format = t.clone();
        }
        if let Some(t) = &self.datetime_format {
            o.datetime_format = t.clone();
        }
        if let Some(d) = self.decimals {
            o.float_max_decimals = Some(d);
        }
        o
    }
}

/// Insert `,` thousands separators into the integer part of a plain number.
pub fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac) = match rest.find('.') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + int_part.len() / 3);
    out.push_str(sign);
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.push_str(frac);
    out
}

/// Format a number text with exactly `decimals` fraction digits (half away from zero).
pub fn fixed_decimals(text: &str, decimals: usize) -> Option<String> {
    let rounded = crate::expr::round_decimal_text(text.trim(), decimals as i32)?;
    let (int_part, frac) = match rounded.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (rounded.clone(), String::new()),
    };
    if decimals == 0 {
        return Some(int_part);
    }
    let mut frac = frac;
    while frac.len() < decimals {
        frac.push('0');
    }
    Some(format!("{int_part}.{frac}"))
}

/// Display text of a cell value.
///
/// `column` overrides apply on top of `opts`; the result honours
/// `max_text_len`, multi-line markers and binary previews like
/// [`Value::display`].
pub fn format_value<'a>(value: &'a Value, opts: &FormatOptions, column: Option<&ColumnFormat>) -> Cow<'a, str> {
    PreparedFormat::new(opts, column).format(value)
}

/// Formatting options of one column, prepared once (per frame) so formatting
/// a cell does not clone [`FormatOptions`].
#[derive(Debug, Clone)]
pub struct PreparedFormat<'o> {
    opts: Cow<'o, FormatOptions>,
    decimals: Option<usize>,
    thousands: bool,
}

/// True when `pattern` is a valid chrono strftime pattern. Formatting with an
/// invalid pattern panics inside chrono, so patterns are checked first.
pub fn valid_strftime(pattern: &str) -> bool {
    use chrono::format::{Item, StrftimeItems};
    !StrftimeItems::new(pattern).any(|i| matches!(i, Item::Error))
}

/// Replace invalid date / time patterns with the defaults.
fn sanitize(opts: &mut FormatOptions) {
    let d = FormatOptions::default();
    if !valid_strftime(&opts.date_format) {
        opts.date_format = d.date_format;
    }
    if !valid_strftime(&opts.time_format) {
        opts.time_format = d.time_format;
    }
    if !valid_strftime(&opts.datetime_format) {
        opts.datetime_format = d.datetime_format;
    }
}

impl<'o> PreparedFormat<'o> {
    /// Prepare `base` with the overrides of `column`. Invalid date/time
    /// patterns fall back to the defaults instead of panicking.
    pub fn new(base: &'o FormatOptions, column: Option<&ColumnFormat>) -> Self {
        let base_ok = valid_strftime(&base.date_format)
            && valid_strftime(&base.time_format)
            && valid_strftime(&base.datetime_format);
        match column.filter(|c| !c.is_empty()) {
            None if base_ok => {
                PreparedFormat { opts: Cow::Borrowed(base), decimals: None, thousands: base.thousands_separator }
            }
            None => {
                let mut o = base.clone();
                sanitize(&mut o);
                PreparedFormat { opts: Cow::Owned(o), decimals: None, thousands: base.thousands_separator }
            }
            Some(c) => {
                let mut o = c.apply(base);
                sanitize(&mut o);
                PreparedFormat {
                    opts: Cow::Owned(o),
                    decimals: c.decimals,
                    thousands: c.thousands_separator.unwrap_or(base.thousands_separator),
                }
            }
        }
    }

    /// Effective options.
    pub fn options(&self) -> &FormatOptions {
        &self.opts
    }

    /// Display text of `value`.
    pub fn format<'v>(&self, value: &'v Value) -> Cow<'v, str> {
        if let Some(d) = self.decimals {
            let text = match value {
                Value::Int(_) | Value::UInt(_) => Some(value.to_text().into_owned()),
                Value::Float(f) if f.is_finite() => Some(format!("{f}")),
                Value::Decimal(s) => Some(s.to_string()),
                _ => None,
            };
            if let Some(f) = text.and_then(|t| fixed_decimals(&t, d)) {
                return Cow::Owned(if self.thousands { group_thousands(&f) } else { f });
            }
        }
        value.display(&self.opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn overrides() {
        let o = FormatOptions::default();
        let c = ColumnFormat { decimals: Some(2), ..Default::default() };
        assert_eq!(format_value(&Value::Float(2.345), &o, Some(&c)), "2.35");
        assert_eq!(format_value(&Value::Int(3), &o, Some(&c)), "3.00");
        assert_eq!(format_value(&Value::decimal("1234.5"), &o, Some(&c)), "1234.50");
        let c2 = ColumnFormat { decimals: Some(1), thousands_separator: Some(true), ..Default::default() };
        assert_eq!(format_value(&Value::decimal("1234567.25"), &o, Some(&c2)), "1,234,567.3");
        assert_eq!(format_value(&Value::decimal("-1234.25"), &o, Some(&c2)), "-1,234.3");
        let d = ColumnFormat { date_format: Some("%d.%m.%Y".into()), ..Default::default() };
        let date = Value::Date(NaiveDate::from_ymd_opt(2024, 3, 9).unwrap());
        assert_eq!(format_value(&date, &o, Some(&d)), "09.03.2024");
        assert_eq!(format_value(&date, &o, None), "2024-03-09");
        assert_eq!(format_value(&Value::Null, &o, Some(&c)), "[NULL]");
        assert_eq!(format_value(&Value::text("x"), &o, Some(&c)), "x");
        assert!(ColumnFormat::default().is_empty());
    }

    #[test]
    fn invalid_patterns_do_not_panic() {
        assert!(valid_strftime("%Y-%m-%d"));
        assert!(!valid_strftime("%Q"));
        let bad = FormatOptions { date_format: "%Q %Y".into(), ..Default::default() };
        let date = Value::Date(NaiveDate::from_ymd_opt(2024, 3, 9).unwrap());
        assert_eq!(format_value(&date, &bad, None), "2024-03-09");
        let col = ColumnFormat { date_format: Some("%!".into()), ..Default::default() };
        assert_eq!(format_value(&date, &FormatOptions::default(), Some(&col)), "2024-03-09");
    }

    #[test]
    fn helpers() {
        assert_eq!(group_thousands("1234567"), "1,234,567");
        assert_eq!(group_thousands("-123"), "-123");
        assert_eq!(group_thousands("12345.678"), "12,345.678");
        assert_eq!(group_thousands("abc"), "abc");
        assert_eq!(fixed_decimals("1.005", 2).unwrap(), "1.01");
        assert_eq!(fixed_decimals("7", 0).unwrap(), "7");
        assert_eq!(fixed_decimals("x", 2), None);
    }
}
