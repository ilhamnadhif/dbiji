//! Markdown (GitHub flavoured) table export.

use super::{ExportSummary, RowSink, TextOutput, cell};
use crate::format::{ValueFormat, format_value};
use crate::options::{ExportOptions, MarkdownOptions};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row};

pub(crate) struct MarkdownSink {
    out: TextOutput,
    opts: MarkdownOptions,
    values: ValueFormat,
    ncols: usize,
    line: String,
    rows: u64,
}

impl MarkdownSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        MarkdownSink {
            out,
            opts: options.markdown.clone(),
            values: options.values.clone(),
            ncols: 0,
            line: String::new(),
            rows: 0,
        }
    }
}

/// Cell text with pipes escaped and line breaks replaced.
fn push_cell(line: &mut String, text: &str, opts: &MarkdownOptions) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '|' if opts.escape_pipes => line.push_str("\\|"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line.push_str(&opts.line_break);
            }
            '\n' => line.push_str(&opts.line_break),
            c => line.push(c),
        }
    }
}

impl RowSink for MarkdownSink {
    fn begin(&mut self, columns: &[ColumnMeta], _table: Option<&ObjectRef>) -> DbResult<()> {
        self.ncols = columns.len();
        let mut head = String::from("|");
        let mut rule = String::from("|");
        for c in columns {
            head.push(' ');
            push_cell(&mut head, &c.name, &self.opts);
            head.push_str(" |");
            let width = c.name.chars().count().max(3);
            if self.opts.right_align_numbers && c.kind.is_numeric() {
                rule.push_str(&"-".repeat(width + 1));
                rule.push_str(":|");
            } else {
                rule.push_str(&"-".repeat(width + 2));
                rule.push('|');
            }
        }
        head.push('\n');
        rule.push('\n');
        self.out.write_str(&head)?;
        self.out.write_str(&rule)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        self.line.clear();
        self.line.push('|');
        for i in 0..self.ncols {
            self.line.push(' ');
            let text = format_value(cell(row, i), &self.values);
            push_cell(&mut self.line, &text, &self.opts);
            self.line.push_str(" |");
        }
        self.line.push('\n');
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
