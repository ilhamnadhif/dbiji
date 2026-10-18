//! Fixed width plain text table export.
//!
//! Column widths are computed from the header and the first
//! `width_sample_rows` rows (which are buffered); later rows are streamed and
//! truncated to those widths, so memory stays bounded.

use super::{ExportSummary, RowSink, TextOutput, cell};
use crate::format::{ValueFormat, format_value, is_numeric_value};
use crate::options::{ExportOptions, TxtOptions};
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row};

struct Cell {
    text: String,
    numeric: bool,
}

pub(crate) struct TxtSink {
    out: TextOutput,
    opts: TxtOptions,
    values: ValueFormat,
    headers: Vec<String>,
    widths: Vec<usize>,
    sample: Vec<Vec<Cell>>,
    streaming: bool,
    line: String,
    rows: u64,
}

impl TxtSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        TxtSink {
            out,
            opts: options.txt.clone(),
            values: options.values.clone(),
            headers: Vec::new(),
            widths: Vec::new(),
            sample: Vec::new(),
            streaming: false,
            line: String::new(),
            rows: 0,
        }
    }

    fn cells(&self, row: &Row) -> Vec<Cell> {
        (0..self.headers.len())
            .map(|i| {
                let v = cell(row, i);
                let text = if v.is_null() {
                    if self.opts.show_nulls { self.opts.null_text.clone() } else { String::new() }
                } else {
                    single_line(&format_value(v, &self.values))
                };
                Cell { text, numeric: is_numeric_value(v) }
            })
            .collect()
    }

    fn rule(&self) -> String {
        let mut s = String::new();
        if self.opts.borders {
            s.push('+');
            for w in &self.widths {
                s.push_str(&"-".repeat(w + 2));
                s.push('+');
            }
        } else {
            for (i, w) in self.widths.iter().enumerate() {
                if i > 0 {
                    s.push_str("  ");
                }
                s.push_str(&"-".repeat(*w));
            }
        }
        s.push('\n');
        s
    }

    fn render(&self, out: &mut String, cells: &[Cell]) {
        if self.opts.borders {
            out.push('|');
        }
        for (i, w) in self.widths.iter().enumerate() {
            let (text, numeric) = cells.get(i).map_or(("", false), |c| (c.text.as_str(), c.numeric));
            if self.opts.borders {
                out.push(' ');
            } else if i > 0 {
                out.push_str("  ");
            }
            let fitted = fit(text, *w);
            let pad = w.saturating_sub(fitted.chars().count());
            let last = i + 1 == self.widths.len();
            if numeric && self.opts.right_align_numbers {
                out.push_str(&" ".repeat(pad));
                out.push_str(&fitted);
            } else {
                out.push_str(&fitted);
                if self.opts.borders || !last {
                    out.push_str(&" ".repeat(pad));
                }
            }
            if self.opts.borders {
                out.push_str(" |");
            }
        }
        out.push('\n');
    }

    /// Fix the widths and write header plus buffered rows.
    fn start_streaming(&mut self) -> DbResult<()> {
        let max = self.opts.max_column_width.max(1);
        let min = self.opts.min_column_width.clamp(1, max);
        self.widths = (0..self.headers.len())
            .map(|i| {
                let header = if self.opts.header { self.headers[i].chars().count() } else { 0 };
                let data =
                    self.sample.iter().filter_map(|r| r.get(i)).map(|c| c.text.chars().count()).max().unwrap_or(0);
                header.max(data).clamp(min, max)
            })
            .collect();
        let mut text = String::new();
        let rule = self.rule();
        if self.opts.borders {
            text.push_str(&rule);
        }
        if self.opts.header {
            let header_cells: Vec<Cell> =
                self.headers.iter().map(|h| Cell { text: single_line(h), numeric: false }).collect();
            self.render(&mut text, &header_cells);
            text.push_str(&rule);
        }
        let sample = std::mem::take(&mut self.sample);
        for r in &sample {
            self.render(&mut text, r);
        }
        self.out.write_str(&text)?;
        self.streaming = true;
        Ok(())
    }
}

/// Replace line breaks and tabs so every row stays on one line.
fn single_line(s: &str) -> String {
    if !s.contains(['\n', '\r', '\t']) {
        return s.to_string();
    }
    s.replace("\r\n", " ").replace(['\n', '\r', '\t'], " ")
}

/// Truncate to `width` characters, marking cut values with `…`.
fn fit(s: &str, width: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= width {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut t: String = s.chars().take(width.saturating_sub(1)).collect();
    t.push('…');
    std::borrow::Cow::Owned(t)
}

impl RowSink for TxtSink {
    fn begin(&mut self, columns: &[ColumnMeta], _table: Option<&ObjectRef>) -> DbResult<()> {
        self.headers = columns.iter().map(|c| c.name.clone()).collect();
        if self.opts.width_sample_rows == 0 {
            self.start_streaming()?;
        }
        Ok(())
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        let cells = self.cells(row);
        self.rows += 1;
        if !self.streaming {
            self.sample.push(cells);
            if self.sample.len() >= self.opts.width_sample_rows {
                self.start_streaming()?;
            }
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        self.render(&mut line, &cells);
        let res = self.out.write_str(&line);
        self.line = line;
        res
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        if !self.streaming {
            self.start_streaming()?;
        }
        if self.opts.borders && !self.widths.is_empty() {
            let rule = self.rule();
            self.out.write_str(&rule)?;
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
