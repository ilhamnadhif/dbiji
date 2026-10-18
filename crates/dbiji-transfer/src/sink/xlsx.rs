//! Excel (XLSX) export.
//!
//! Worksheets use rust_xlsxwriter's constant memory mode: every finished row
//! is flushed to a temporary file, so memory does not grow with the number
//! of rows. Values are written as typed cells (numbers, booleans, dates).

use super::{ExportSummary, ExportTarget, RowSink, cell, create_file, remove_partial, source_table_name};
use crate::error::{file_error, format_error};
use crate::format::BinaryEncoding;
use crate::options::{ExportOptions, XlsxOptions};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbiji_core::{ColumnMeta, DbError, DbResult, ObjectRef, Row, Value};
use rust_xlsxwriter::{Format, Workbook, Worksheet, XlsxError};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::Instant;

/// Excel's hard limits.
const EXCEL_MAX_ROWS: u32 = 1_048_576;
const EXCEL_MAX_COLS: usize = 16_384;
const EXCEL_MAX_STRING: usize = 32_767;
/// Integers beyond 15 digits lose precision as Excel numbers; they are written as text.
const EXCEL_MAX_EXACT: i64 = 999_999_999_999_999;

struct Formats {
    header: Format,
    date: Format,
    time: Format,
    datetime: Format,
    number: Option<Format>,
    integer: Option<Format>,
    binary: BinaryEncoding,
}

pub(crate) struct XlsxSink {
    workbook: Workbook,
    output: Option<ExportTarget>,
    opts: XlsxOptions,
    formats: Formats,
    headers: Vec<String>,
    ncols: usize,
    sheet_base: String,
    sheet_no: u32,
    sheet_index: usize,
    next_row: u32,
    rows_in_sheet: u32,
    rows_per_sheet: u32,
    widths: Vec<usize>,
    rows: u64,
    truncated: u64,
    started: Instant,
    poisoned: bool,
}

fn xlsx_err(e: XlsxError) -> DbError {
    format_error("XLSX", e)
}

/// Run a rust_xlsxwriter call, turning a library panic (e.g. a failed write to
/// its temporary file) into an error.
fn guarded<T>(f: impl FnOnce() -> Result<T, XlsxError>) -> DbResult<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r.map_err(xlsx_err),
        Err(_) => Err(DbError::io("XLSX writer failed unexpectedly (out of disk space?)")),
    }
}

impl XlsxSink {
    pub(crate) fn new(options: &ExportOptions, output: ExportTarget) -> DbResult<Self> {
        let opts = options.xlsx.clone();
        let mut header = Format::new();
        if opts.bold_header {
            header = header.set_bold();
        }
        let formats = Formats {
            header,
            date: Format::new().set_num_format(&opts.date_format),
            time: Format::new().set_num_format(&opts.time_format),
            datetime: Format::new().set_num_format(&opts.datetime_format),
            number: opts.number_format.as_ref().map(|f| Format::new().set_num_format(f)),
            integer: opts.integer_format.as_ref().map(|f| Format::new().set_num_format(f)),
            binary: options.values.binary,
        };
        Ok(XlsxSink {
            workbook: Workbook::new(),
            output: Some(output),
            opts,
            formats,
            headers: Vec::new(),
            ncols: 0,
            sheet_base: String::new(),
            sheet_no: 0,
            sheet_index: 0,
            next_row: 0,
            rows_in_sheet: 0,
            rows_per_sheet: EXCEL_MAX_ROWS - 1,
            widths: Vec::new(),
            rows: 0,
            truncated: 0,
            started: Instant::now(),
            poisoned: false,
        })
    }

    fn new_sheet(&mut self) -> DbResult<()> {
        self.sheet_no += 1;
        let name = if self.sheet_no == 1 {
            self.sheet_base.clone()
        } else {
            let suffix = format!(" ({})", self.sheet_no);
            let keep = 31usize.saturating_sub(suffix.chars().count());
            format!("{}{suffix}", self.sheet_base.chars().take(keep).collect::<String>())
        };
        let header = self.opts.header;
        let freeze = self.opts.freeze_header;
        let headers = self.headers.clone();
        let header_format = self.formats.header.clone();
        let workbook = &mut self.workbook;
        guarded(|| {
            let ws = workbook.add_worksheet_with_constant_memory()?;
            ws.set_name(&name)?;
            if header {
                for (i, h) in headers.iter().enumerate() {
                    ws.write_string_with_format(0, i as u16, h, &header_format)?;
                }
                if freeze {
                    ws.set_freeze_panes(1, 0)?;
                }
            }
            Ok(())
        })?;
        self.sheet_index = self.sheet_no as usize - 1;
        self.next_row = u32::from(header);
        self.rows_in_sheet = 0;
        self.widths = self.headers.iter().map(|h| h.chars().count() + if self.opts.autofilter { 3 } else { 0 }).collect();
        Ok(())
    }

    /// Column widths and autofilter of the current sheet (written when the file is saved).
    fn finish_sheet(&mut self) -> DbResult<()> {
        let auto = self.opts.auto_column_width;
        let max = usize::from(self.opts.max_column_width).max(4);
        let widths = self.widths.clone();
        let filter = self.opts.autofilter && self.opts.header && self.ncols > 0;
        let last_row = self.next_row.saturating_sub(1);
        let last_col = self.ncols.saturating_sub(1) as u16;
        let index = self.sheet_index;
        let workbook = &mut self.workbook;
        guarded(|| {
            let ws = workbook.worksheet_from_index(index)?;
            if auto {
                for (i, w) in widths.iter().enumerate() {
                    ws.set_column_width(i as u16, ((*w).clamp(4, max) + 2) as f64)?;
                }
            }
            if filter {
                ws.autofilter(0, 0, last_row, last_col)?;
            }
            Ok(())
        })
    }

    fn save(&mut self) -> DbResult<Vec<PathBuf>> {
        let output = self.output.take().ok_or_else(|| DbError::internal("XLSX export already finished"))?;
        let workbook = &mut self.workbook;
        match output {
            ExportTarget::Path(path) => {
                // Create the file first for a clear error message (missing folder, permissions).
                drop(create_file(&path)?);
                if let Err(e) = guarded(|| workbook.save(&path)) {
                    remove_partial(&path);
                    return Err(e);
                }
                Ok(vec![path])
            }
            ExportTarget::Writer(mut w) => {
                let tmp = tempfile::Builder::new().suffix(".xlsx").tempfile().map_err(|e| DbError::io(format!("Cannot create temporary file: {e}")))?;
                let tmp_path = tmp.path().to_path_buf();
                guarded(|| workbook.save(&tmp_path))?;
                let mut f = std::fs::File::open(&tmp_path).map_err(|e| file_error("Cannot read", &tmp_path, e))?;
                std::io::copy(&mut f, &mut w).map_err(|e| DbError::io(format!("Cannot write export output: {e}")))?;
                w.flush().map_err(|e| DbError::io(format!("Cannot write export output: {e}")))?;
                Ok(Vec::new())
            }
        }
    }
}

/// Write one cell; returns the approximate display width in characters.
fn write_cell(ws: &mut Worksheet, row: u32, col: u16, v: &Value, f: &Formats, truncated: &mut u64) -> Result<usize, XlsxError> {
    Ok(match v {
        Value::Null => 0,
        Value::Bool(b) => {
            ws.write_boolean(row, col, *b)?;
            5
        }
        Value::Int(i) if i.unsigned_abs() <= EXCEL_MAX_EXACT as u64 => write_int(ws, row, col, *i as f64, f, i.to_string().len())?,
        Value::UInt(u) if *u <= EXCEL_MAX_EXACT as u64 => write_int(ws, row, col, *u as f64, f, u.to_string().len())?,
        Value::Float(x) if x.is_finite() => {
            match &f.number {
                Some(fmt) => ws.write_number_with_format(row, col, *x, fmt)?,
                None => ws.write_number(row, col, *x)?,
            };
            format!("{x}").len().min(16)
        }
        Value::Decimal(s) => match excel_exact_number(s) {
            Some(x) => {
                match &f.number {
                    Some(fmt) => ws.write_number_with_format(row, col, x, fmt)?,
                    None => ws.write_number(row, col, x)?,
                };
                s.len()
            }
            None => write_text(ws, row, col, s, truncated)?,
        },
        Value::Text(s) => write_text(ws, row, col, s, truncated)?,
        Value::Bytes(b) => write_text(ws, row, col, &f.binary.encode(b), truncated)?,
        Value::Date(d) => match excel_serial(d.and_time(NaiveTime::MIN)) {
            Some(serial) => {
                ws.write_number_with_format(row, col, serial, &f.date)?;
                10
            }
            None => write_text(ws, row, col, &v.to_text(), truncated)?,
        },
        Value::Time(t) => {
            ws.write_number_with_format(row, col, time_fraction(t), &f.time)?;
            8
        }
        Value::DateTime(dt) => write_datetime(ws, row, col, *dt, v, f, truncated)?,
        Value::DateTimeTz(dt) => write_datetime(ws, row, col, dt.naive_local(), v, f, truncated)?,
        other => write_text(ws, row, col, &other.to_text(), truncated)?,
    })
}

fn write_int(ws: &mut Worksheet, row: u32, col: u16, x: f64, f: &Formats, len: usize) -> Result<usize, XlsxError> {
    match &f.integer {
        Some(fmt) => ws.write_number_with_format(row, col, x, fmt)?,
        None => ws.write_number(row, col, x)?,
    };
    Ok(len)
}

fn write_datetime(ws: &mut Worksheet, row: u32, col: u16, dt: NaiveDateTime, original: &Value, f: &Formats, truncated: &mut u64) -> Result<usize, XlsxError> {
    match excel_serial(dt) {
        Some(serial) => {
            ws.write_number_with_format(row, col, serial, &f.datetime)?;
            Ok(19)
        }
        None => write_text(ws, row, col, &original.to_text(), truncated),
    }
}

fn write_text(ws: &mut Worksheet, row: u32, col: u16, s: &str, truncated: &mut u64) -> Result<usize, XlsxError> {
    let chars = s.chars().count();
    if chars > EXCEL_MAX_STRING {
        *truncated += 1;
        let cut: String = s.chars().take(EXCEL_MAX_STRING).collect();
        ws.write_string(row, col, cut)?;
    } else {
        ws.write_string(row, col, s)?;
    }
    // The first line decides the visible width.
    Ok(s.lines().next().map_or(0, |l| l.chars().count()))
}

/// Excel serial date (days since 1899-12-30, fraction = time of day). Dates
/// before 1900-03-01 (Excel's leap year bug) and after 9999 are not representable.
pub(crate) fn excel_serial(dt: NaiveDateTime) -> Option<f64> {
    let min = NaiveDate::from_ymd_opt(1900, 3, 1)?;
    if dt.date() < min || dt.year() > 9999 {
        return None;
    }
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_time(NaiveTime::MIN);
    let ms = dt.signed_duration_since(epoch).num_milliseconds();
    Some(ms as f64 / 86_400_000.0)
}

fn time_fraction(t: &NaiveTime) -> f64 {
    let nanos = f64::from(t.nanosecond().min(999_999_999));
    (f64::from(t.num_seconds_from_midnight()) + nanos / 1e9) / 86_400.0
}

/// The decimal as f64 when Excel can hold it exactly (at most 15 significant digits).
fn excel_exact_number(s: &str) -> Option<f64> {
    let mantissa = s.split(['e', 'E']).next().unwrap_or(s);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let significant = digits.trim_start_matches('0').trim_end_matches('0').len();
    if significant > 15 || !mantissa.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+') {
        return None;
    }
    let x: f64 = s.parse().ok()?;
    x.is_finite().then_some(x)
}

/// Excel sheet names: at most 31 characters, none of `[]:*?/\`, not quoted.
pub(crate) fn sanitize_sheet_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') || c.is_control() { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_matches('\'').trim();
    let cut: String = cleaned.chars().take(31).collect();
    if cut.is_empty() || cut.eq_ignore_ascii_case("history") { "Sheet1".into() } else { cut }
}

impl RowSink for XlsxSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        if columns.len() > EXCEL_MAX_COLS {
            return Err(DbError::unsupported(format!("Excel supports at most {EXCEL_MAX_COLS} columns, the result has {}", columns.len())));
        }
        self.ncols = columns.len();
        self.headers = columns.iter().map(|c| c.name.clone()).collect();
        let base = match &self.opts.sheet_name {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => source_table_name(columns, table).map(|(_, n)| n).unwrap_or_else(|| "Sheet1".into()),
        };
        self.sheet_base = sanitize_sheet_name(&base);
        let header_rows = u32::from(self.opts.header);
        self.rows_per_sheet = self.opts.max_rows_per_sheet.clamp(1, EXCEL_MAX_ROWS - header_rows);
        self.new_sheet()
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        if self.poisoned {
            return Err(DbError::io("XLSX writer is in an inconsistent state"));
        }
        if self.rows_in_sheet >= self.rows_per_sheet {
            self.finish_sheet()?;
            self.new_sheet()?;
        }
        let r = self.next_row;
        let ncols = self.ncols;
        let index = self.sheet_index;
        let formats = &self.formats;
        let widths = &mut self.widths;
        let truncated = &mut self.truncated;
        let workbook = &mut self.workbook;
        let result = guarded(|| {
            let ws = workbook.worksheet_from_index(index)?;
            for i in 0..ncols {
                let w = write_cell(ws, r, i as u16, cell(row, i), formats, truncated)?;
                if let Some(slot) = widths.get_mut(i) {
                    *slot = (*slot).max(w);
                }
            }
            Ok(())
        });
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.next_row += 1;
        self.rows_in_sheet += 1;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        if self.sheet_no == 0 {
            return Err(DbError::internal("XLSX export finished before it began"));
        }
        self.finish_sheet()?;
        let files = self.save()?;
        let mut warnings = Vec::new();
        if self.truncated > 0 {
            warnings.push(format!("{} text value(s) longer than {EXCEL_MAX_STRING} characters were truncated", self.truncated));
        }
        if self.sheet_no > 1 {
            warnings.push(format!("The data was split into {} sheets", self.sheet_no));
        }
        let bytes = files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        Ok(ExportSummary { rows: self.rows, bytes, files, warnings, duration_ms: self.started.elapsed().as_millis() as u64 })
    }

    fn abort(&mut self) {
        // Nothing has reached the target yet: the workbook is only saved by `finish`.
        self.output = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_dates() {
        let d = NaiveDate::from_ymd_opt(2025, 10, 13).unwrap().and_hms_opt(12, 0, 0).unwrap();
        assert_eq!(excel_serial(d), Some(45943.5));
        let early = NaiveDate::from_ymd_opt(1900, 2, 28).unwrap().and_time(NaiveTime::MIN);
        assert_eq!(excel_serial(early), None);
    }

    #[test]
    fn exact_numbers() {
        assert_eq!(excel_exact_number("12.50"), Some(12.5));
        assert_eq!(excel_exact_number("-0.001"), Some(-0.001));
        assert_eq!(excel_exact_number("12345678901234567890.123"), None);
        assert_eq!(excel_exact_number("NaN"), None);
    }

    #[test]
    fn sheet_names() {
        assert_eq!(sanitize_sheet_name("a/b:c"), "a_b_c");
        assert_eq!(sanitize_sheet_name("'x'"), "x");
        assert_eq!(sanitize_sheet_name(""), "Sheet1");
        assert_eq!(sanitize_sheet_name(&"x".repeat(40)).len(), 31);
    }
}
