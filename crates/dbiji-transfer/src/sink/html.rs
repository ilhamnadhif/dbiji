//! HTML table export. Every piece of text is escaped.

use super::{ExportSummary, RowSink, TextOutput, cell, source_table_name};
use crate::format::{ValueFormat, format_value, is_numeric_value};
use crate::options::{ExportOptions, HtmlOptions};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row, Value};

const CSS: &str = "body{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,sans-serif;margin:16px;color:#1f2328;background:#fff}\
table{border-collapse:collapse;font-size:13px}\
caption{text-align:left;font-weight:600;padding:4px 0}\
th,td{border:1px solid #d0d7de;padding:4px 8px;vertical-align:top;white-space:pre-wrap}\
th{background:#f6f8fa;text-align:left}\
td.num{text-align:right}\
td.null{color:#8c959f;font-style:italic}\
@media (prefers-color-scheme:dark){body{color:#e6edf3;background:#0d1117}th{background:#161b22}th,td{border-color:#30363d}}";

pub(crate) struct HtmlSink {
    out: TextOutput,
    opts: HtmlOptions,
    values: ValueFormat,
    ncols: usize,
    line: String,
    rows: u64,
}

/// Escape text for HTML element content and attribute values.
pub(crate) fn push_escaped_html(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

impl HtmlSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        HtmlSink { out, opts: options.html.clone(), values: options.values.clone(), ncols: 0, line: String::new(), rows: 0 }
    }
}

impl RowSink for HtmlSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        self.ncols = columns.len();
        let title = match &self.opts.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => source_table_name(columns, table).map(|(_, n)| n).unwrap_or_else(|| "Export".into()),
        };
        let mut head = String::from("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"");
        head.push_str(self.out.encoding_name());
        head.push_str("\">\n<title>");
        push_escaped_html(&mut head, &title);
        head.push_str("</title>\n");
        if self.opts.embed_css {
            head.push_str("<style>");
            head.push_str(CSS);
            head.push_str("</style>\n");
        }
        head.push_str("</head>\n<body>\n<table>\n<caption>");
        push_escaped_html(&mut head, &title);
        head.push_str("</caption>\n");
        if self.opts.header {
            head.push_str("<thead>\n<tr>");
            for c in columns {
                head.push_str("<th>");
                push_escaped_html(&mut head, &c.name);
                head.push_str("</th>");
            }
            head.push_str("</tr>\n</thead>\n");
        }
        head.push_str("<tbody>\n");
        self.out.write_str(&head)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        self.line.clear();
        self.line.push_str("<tr>");
        for i in 0..self.ncols {
            let v = cell(row, i);
            match v {
                Value::Null => self.line.push_str("<td class=\"null\">"),
                v if is_numeric_value(v) => self.line.push_str("<td class=\"num\">"),
                _ => self.line.push_str("<td>"),
            }
            let text = if v.is_null() && self.values.null_text.is_empty() {
                std::borrow::Cow::Borrowed("NULL")
            } else {
                format_value(v, &self.values)
            };
            push_escaped_html(&mut self.line, &text);
            self.line.push_str("</td>");
        }
        self.line.push_str("</tr>\n");
        self.out.write_str(&self.line)?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        self.out.write_str("</tbody>\n</table>\n</body>\n</html>\n")?;
        self.out.finish(self.rows)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}
