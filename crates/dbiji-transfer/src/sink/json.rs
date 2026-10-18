//! JSON and JSON Lines export.
//!
//! Values keep their JSON types: numbers stay numbers (decimals are written
//! verbatim so no precision is lost), JSON documents are embedded as JSON,
//! arrays become JSON arrays, everything else is a string.

use super::{ExportSummary, RowSink, TextOutput, cell, source_table_name, unique_names};
use crate::format::{BinaryEncoding, ValueFormat, format_value};
use crate::options::{ExportOptions, JsonLayout, JsonOptions};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row, Value};
use std::fmt::Write as _;

pub(crate) struct JsonSink {
    out: TextOutput,
    opts: JsonOptions,
    values: ValueFormat,
    lines: bool,
    /// Pre-rendered `"name"` keys.
    keys: Vec<String>,
    ncols: usize,
    line: String,
    rows: u64,
    /// Closing text written by `finish`.
    epilogue: String,
}

impl JsonSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions, lines: bool) -> Self {
        JsonSink {
            out,
            opts: options.json.clone(),
            values: options.values.clone(),
            lines,
            keys: Vec::new(),
            ncols: 0,
            line: String::with_capacity(1024),
            rows: 0,
            epilogue: String::new(),
        }
    }

    fn render_value(&self, out: &mut String, v: &Value) {
        let fmt = if self.opts.format_dates { Some(&self.values) } else { None };
        push_json_value(out, v, self.opts.binary, fmt);
    }
}

impl RowSink for JsonSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        self.ncols = columns.len();
        self.keys = unique_names(columns)
            .iter()
            .map(|n| {
                let mut s = String::with_capacity(n.len() + 2);
                push_json_string(&mut s, n);
                s
            })
            .collect();
        if self.lines {
            return Ok(());
        }
        let table_name = source_table_name(columns, table).map(|(_, n)| n).unwrap_or_else(|| "query_result".into());
        let pretty = self.opts.pretty;
        let mut head = String::new();
        match self.opts.layout {
            JsonLayout::ArrayOfObjects => {
                if self.opts.include_table_name {
                    head.push('{');
                    if pretty {
                        head.push_str("\n  ");
                    }
                    push_json_string(&mut head, &table_name);
                    head.push_str(if pretty { ": [" } else { ":[" });
                    self.epilogue = if pretty { "\n  ]\n}\n".into() } else { "]}\n".into() };
                } else {
                    head.push('[');
                    self.epilogue = if pretty { "\n]\n".into() } else { "]\n".into() };
                }
            }
            JsonLayout::ColumnsAndRows => {
                head.push('{');
                let nl = if pretty { "\n  " } else { "" };
                let sep = if pretty { ": " } else { ":" };
                if self.opts.include_table_name {
                    head.push_str(nl);
                    let _ = write!(head, "\"table\"{sep}");
                    push_json_string(&mut head, &table_name);
                    head.push(',');
                }
                head.push_str(nl);
                let _ = write!(head, "\"columns\"{sep}[");
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        head.push(',');
                    }
                    if pretty {
                        head.push_str("\n    ");
                    }
                    let _ = write!(head, "{{\"name\"{sep}{}", self.keys[i]);
                    let _ = write!(head, ",{}\"type\"{sep}", if pretty { " " } else { "" });
                    push_json_string(&mut head, &c.type_name);
                    let _ = write!(head, ",{}\"kind\"{sep}", if pretty { " " } else { "" });
                    push_json_string(&mut head, &format!("{:?}", c.kind).to_lowercase());
                    head.push('}');
                }
                if pretty && !columns.is_empty() {
                    head.push_str("\n  ");
                }
                head.push_str("],");
                head.push_str(nl);
                let _ = write!(head, "\"rows\"{sep}[");
                self.epilogue = if pretty { "\n  ]\n}\n".into() } else { "]}\n".into() };
            }
        }
        self.out.write_str(&head)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let pretty = self.opts.pretty && !self.lines;
        let nested = self.opts.include_table_name || self.opts.layout == JsonLayout::ColumnsAndRows;
        let indent = if nested { "    " } else { "  " };
        if !self.lines {
            if self.rows > 0 {
                line.push(',');
            }
            if pretty {
                line.push('\n');
                line.push_str(indent);
            }
        }
        if self.opts.layout == JsonLayout::ColumnsAndRows && !self.lines {
            line.push('[');
            for i in 0..self.ncols {
                if i > 0 {
                    line.push_str(if pretty { ", " } else { "," });
                }
                self.render_value(&mut line, cell(row, i));
            }
            line.push(']');
        } else {
            line.push('{');
            for i in 0..self.ncols {
                if i > 0 {
                    line.push(',');
                }
                if pretty {
                    line.push('\n');
                    line.push_str(indent);
                    line.push_str("  ");
                }
                line.push_str(&self.keys[i]);
                line.push_str(if pretty { ": " } else { ":" });
                self.render_value(&mut line, cell(row, i));
            }
            if pretty && self.ncols > 0 {
                line.push('\n');
                line.push_str(indent);
            }
            line.push('}');
        }
        if self.lines {
            line.push('\n');
        }
        let res = self.out.write_str(&line);
        self.line = line;
        res?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        if !self.lines {
            let mut epilogue = std::mem::take(&mut self.epilogue);
            if self.rows == 0 && self.opts.pretty {
                // `[\n]` looks odd for an empty result; keep it compact.
                epilogue = epilogue.trim_start_matches('\n').trim_start().to_string();
            }
            self.out.write_str(&epilogue)?;
        }
        self.out.finish(self.rows)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}

/// Append `s` as a JSON string literal.
pub(crate) fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: Option<&str> = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            '\u{08}' => Some("\\b"),
            '\u{0C}' => Some("\\f"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => None,
            _ => continue,
        };
        out.push_str(&s[start..i]);
        match esc {
            Some(e) => out.push_str(e),
            None => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
        }
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// True when `s` is a valid JSON number token.
pub(crate) fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == b.len()
}

/// True when `s` is a complete JSON document.
pub(crate) fn is_valid_json(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

/// Append the JSON rendering of `v`. With `fmt`, temporal values use its patterns.
pub(crate) fn push_json_value(out: &mut String, v: &Value, binary: BinaryEncoding, fmt: Option<&ValueFormat>) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::UInt(u) => {
            let _ = write!(out, "{u}");
        }
        Value::Float(f) if f.is_finite() => {
            let _ = write!(out, "{f}");
        }
        Value::Decimal(s) if is_json_number(s) => out.push_str(s),
        Value::Json(s) if is_valid_json(s) => out.push_str(s.trim()),
        Value::Text(s) => push_json_string(out, s),
        Value::Bytes(b) => push_json_string(out, &binary.encode(b)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_value(out, item, binary, fmt);
            }
            out.push(']');
        }
        Value::Date(_) | Value::Time(_) | Value::DateTime(_) | Value::DateTimeTz(_) if fmt.is_some() => {
            if let Some(f) = fmt {
                push_json_string(out, &format_value(v, f));
            }
        }
        other => push_json_string(out, &other.to_text()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_numbers() {
        for ok in ["0", "-0", "12", "-1.5", "1e10", "1.5E-3", "123456789012345678901234567890.123456789"] {
            assert!(is_json_number(ok), "{ok}");
        }
        for bad in ["", "-", "01", "1.", ".5", "+1", "1e", "NaN", "Infinity", "1.2.3", "0x10", " 1"] {
            assert!(!is_json_number(bad), "{bad}");
        }
    }

    #[test]
    fn string_escapes() {
        let mut s = String::new();
        push_json_string(&mut s, "a\"b\\c\nd\te\u{1}f\u{2028}😀");
        let expected = format!("\"a\\\"b\\\\c\\nd\\te\\u0001f\\u2028{}\"", "\u{1F600}");
        assert_eq!(s, expected);
        let back: String = serde_json::from_str(&s).unwrap();
        assert_eq!(back, "a\"b\\c\nd\te\u{1}f\u{2028}😀");
    }

    #[test]
    fn values() {
        let render = |v: Value| {
            let mut s = String::new();
            push_json_value(&mut s, &v, BinaryEncoding::Base64, None);
            s
        };
        assert_eq!(render(Value::decimal("12345678901234567890.123")), "12345678901234567890.123");
        assert_eq!(render(Value::decimal("007")), "\"007\"");
        assert_eq!(render(Value::Float(f64::NAN)), "\"NaN\"");
        assert_eq!(render(Value::json(" {\"a\": [1, 2]} ")), "{\"a\": [1, 2]}");
        assert_eq!(render(Value::json("{bad")), "\"{bad\"");
        assert_eq!(render(Value::bytes(vec![0u8, 1, 2])), "\"AAEC\"");
        assert_eq!(
            render(Value::Array(vec![Value::Int(1), Value::Null, Value::text("x")].into_boxed_slice())),
            "[1,null,\"x\"]"
        );
    }
}
