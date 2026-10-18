//! CSV and TSV export.

use super::{ExportSummary, RowSink, TextOutput, cell};
use crate::format::{ValueFormat, format_value, is_numeric_value};
use crate::options::{CsvOptions, EscapeStyle, ExportOptions, HeaderStyle, QuoteMode};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row, Value};
use std::fmt::Write as _;

pub(crate) struct CsvSink {
    out: TextOutput,
    style: FieldStyle,
    values: ValueFormat,
    canonical: bool,
    /// Integers can be written without any quoting check.
    int_fast_path: bool,
    header: bool,
    header_style: HeaderStyle,
    line_ending: &'static str,
    ncols: usize,
    line: String,
    rows: u64,
}

/// Quoting rules of one CSV dialect.
struct FieldStyle {
    delimiter: String,
    quote: char,
    mode: QuoteMode,
    escape: EscapeStyle,
    null_string: String,
}

impl FieldStyle {
    fn needs_quotes(&self, text: &str) -> bool {
        if text.is_empty() {
            // An empty text must differ from NULL when NULL is written as nothing.
            return self.null_string.is_empty();
        }
        if !self.null_string.is_empty() && text == self.null_string {
            return true;
        }
        text.starts_with(char::is_whitespace)
            || text.ends_with(char::is_whitespace)
            || text.contains(self.delimiter.as_str())
            || text.contains(self.quote)
            || text.contains(['\n', '\r'])
            || (self.escape == EscapeStyle::Backslash && text.contains('\\'))
    }

    fn push(&self, line: &mut String, text: &str, numeric: bool) {
        let quote = match self.mode {
            QuoteMode::Always => true,
            QuoteMode::Never => false,
            QuoteMode::Needed => self.needs_quotes(text),
            QuoteMode::Strings => !numeric || self.needs_quotes(text),
        };
        if !quote {
            line.push_str(text);
            return;
        }
        line.push(self.quote);
        for c in text.chars() {
            if c == self.quote {
                line.push(match self.escape {
                    EscapeStyle::Doubled => self.quote,
                    EscapeStyle::Backslash => '\\',
                });
            } else if c == '\\' && self.escape == EscapeStyle::Backslash {
                line.push('\\');
            }
            line.push(c);
        }
        line.push(self.quote);
    }
}

impl CsvSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions, tab: bool) -> Self {
        let opts: &CsvOptions = &options.csv;
        let delimiter = if tab {
            "\t".to_string()
        } else if opts.delimiter.is_empty() {
            ",".to_string()
        } else {
            opts.delimiter.clone()
        };
        let canonical = options.values.is_canonical();
        let int_fast_path = canonical && opts.quote_mode != QuoteMode::Always && !delimiter.chars().any(|c| c.is_ascii_digit() || c == '-');
        CsvSink {
            out,
            style: FieldStyle { delimiter, quote: opts.quote_char, mode: opts.quote_mode, escape: opts.escape, null_string: opts.null_string.clone() },
            values: options.values.clone(),
            canonical,
            int_fast_path,
            header: opts.header,
            header_style: opts.header_style,
            line_ending: opts.line_ending.as_str(),
            ncols: 0,
            line: String::with_capacity(1024),
            rows: 0,
        }
    }

    fn push_value(&mut self, v: &Value) {
        match v {
            Value::Null => self.line.push_str(&self.style.null_string),
            Value::Int(i) if self.int_fast_path => {
                let _ = write!(self.line, "{i}");
            }
            Value::UInt(u) if self.int_fast_path => {
                let _ = write!(self.line, "{u}");
            }
            Value::Text(s) if self.canonical => self.style.push(&mut self.line, s, false),
            other => {
                let text = format_value(other, &self.values);
                let numeric = is_numeric_value(other) || matches!(other, Value::Bool(_));
                self.style.push(&mut self.line, &text, numeric);
            }
        }
    }
}

impl RowSink for CsvSink {
    fn begin(&mut self, columns: &[ColumnMeta], _table: Option<&ObjectRef>) -> DbResult<()> {
        self.ncols = columns.len();
        if self.header {
            self.line.clear();
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    self.line.push_str(&self.style.delimiter);
                }
                let text = match self.header_style {
                    HeaderStyle::Label => c.name.clone(),
                    HeaderStyle::Type => type_label(c),
                    HeaderStyle::Both => format!("{} ({})", c.name, type_label(c)),
                };
                self.style.push(&mut self.line, &text, false);
            }
            self.line.push_str(self.line_ending);
            self.out.write_str(&self.line)?;
        }
        Ok(())
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        self.line.clear();
        for i in 0..self.ncols {
            if i > 0 {
                self.line.push_str(&self.style.delimiter);
            }
            self.push_value(cell(row, i));
        }
        self.line.push_str(self.line_ending);
        self.out.write_str(&self.line)?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        self.out.finish(self.rows)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}

/// Type shown in headers: the database type, or the logical kind when unknown.
pub(crate) fn type_label(c: &ColumnMeta) -> String {
    if c.type_name.is_empty() { format!("{:?}", c.kind).to_lowercase() } else { c.type_name.clone() }
}
