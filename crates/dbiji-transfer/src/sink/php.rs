//! PHP source code export: `$rows = array(array("col" => value, ...), ...);`

use super::{ExportSummary, RowSink, TextOutput, cell, source_table_name, unique_names};
use crate::options::{ExportOptions, PhpOptions};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row, Value};
use std::fmt::Write as _;

pub(crate) struct PhpSink {
    out: TextOutput,
    opts: PhpOptions,
    keys: Vec<String>,
    line: String,
    rows: u64,
}

/// Append `s` as a double quoted PHP string literal.
pub(crate) fn push_php_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0B}' => out.push_str("\\v"),
            '\u{0C}' => out.push_str("\\f"),
            '\u{1B}' => out.push_str("\\e"),
            c if (c as u32) < 0x20 || c == '\u{7F}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A valid PHP variable name.
fn php_identifier(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

fn push_php_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i64::MIN) => out.push_str("PHP_INT_MIN"),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::UInt(u) if *u <= i64::MAX as u64 => {
            let _ = write!(out, "{u}");
        }
        Value::Float(f) if f.is_nan() => out.push_str("NAN"),
        Value::Float(f) if f.is_infinite() => out.push_str(if *f > 0.0 { "INF" } else { "-INF" }),
        Value::Float(f) => {
            let s = format!("{f}");
            out.push_str(&s);
            if !s.contains(['.', 'e', 'E']) {
                out.push_str(".0");
            }
        }
        other => push_php_string(out, &other.to_text()),
    }
}

impl PhpSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        PhpSink { out, opts: options.php.clone(), keys: Vec::new(), line: String::new(), rows: 0 }
    }

    fn open(&self) -> &'static str {
        if self.opts.short_array_syntax { "[" } else { "array(" }
    }

    fn close(&self) -> &'static str {
        if self.opts.short_array_syntax { "]" } else { ")" }
    }
}

impl RowSink for PhpSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        self.keys = unique_names(columns)
            .iter()
            .map(|n| {
                let mut s = String::new();
                push_php_string(&mut s, n);
                s
            })
            .collect();
        let var = match &self.opts.variable_name {
            Some(v) if !v.trim().is_empty() => php_identifier(v.trim().trim_start_matches('$')),
            _ => source_table_name(columns, table).map(|(_, n)| php_identifier(&n)).unwrap_or_else(|| "rows".into()),
        };
        let mut head = String::new();
        if self.opts.open_tag {
            head.push_str("<?php\n");
        }
        let _ = writeln!(head, "${var} = {}", self.open());
        self.out.write_str(&head)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        line.push_str("    ");
        line.push_str(self.open());
        line.push('\n');
        for (i, key) in self.keys.iter().enumerate() {
            line.push_str("        ");
            line.push_str(key);
            line.push_str(" => ");
            push_php_value(&mut line, cell(row, i));
            line.push_str(",\n");
        }
        line.push_str("    ");
        line.push_str(self.close());
        line.push_str(",\n");
        let res = self.out.write_str(&line);
        self.line = line;
        res?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        let mut tail = format!("{};\n", self.close());
        if self.opts.open_tag {
            tail.push_str("?>\n");
        }
        self.out.write_str(&tail)?;
        self.out.finish(self.rows)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_strings() {
        let mut s = String::new();
        push_php_string(&mut s, "a\"b\\c$d{$e}\n\u{0}\u{7}x");
        assert_eq!(s, r#""a\"b\\c\$d{\$e}\n\x00\x07x""#);
        assert_eq!(php_identifier("my table"), "my_table");
        assert_eq!(php_identifier("1x"), "_1x");
    }

    #[test]
    fn php_values() {
        let r = |v: Value| {
            let mut s = String::new();
            push_php_value(&mut s, &v);
            s
        };
        assert_eq!(r(Value::Float(2.0)), "2.0");
        assert_eq!(r(Value::Float(f64::NEG_INFINITY)), "-INF");
        assert_eq!(r(Value::Int(i64::MIN)), "PHP_INT_MIN");
        assert_eq!(r(Value::UInt(u64::MAX)), "\"18446744073709551615\"");
        assert_eq!(r(Value::decimal("1.50")), "\"1.50\"");
    }
}
