//! Excel reader (XLSX streaming; XLS, XLSB and ODS through calamine ranges).
//!
//! The workbook is read on a dedicated thread that streams rows through a
//! bounded channel, so only a few batches are in memory at a time (XLSX
//! worksheets are parsed cell by cell, never loaded as a whole).

use crate::error::{file_error, format_error};
use crate::source::RowSource;
use async_trait::async_trait;
use calamine::{Data, DataRef, Reader, Xlsx, open_workbook, open_workbook_auto};
use chrono::{NaiveDate, NaiveTime};
use dbiji_core::value::{parse_date, parse_datetime, parse_time};
use dbiji_core::{ColumnMeta, DataKind, DbError, DbResult, Row, Value};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::{Receiver, Sender, channel};

/// Which worksheet to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SheetSelector {
    /// Zero-based position.
    Index(usize),
    /// Sheet name.
    Name(String),
}

impl Default for SheetSelector {
    fn default() -> Self {
        SheetSelector::Index(0)
    }
}

/// Options of the XLSX reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XlsxReadOptions {
    /// Worksheet.
    pub sheet: SheetSelector,
    /// The first row (after `skip_rows`) holds the column names.
    pub header: bool,
    /// Rows skipped before the header / data.
    pub skip_rows: usize,
    /// Ignore rows without any value.
    pub skip_empty_rows: bool,
    /// Rows read ahead to determine the number of columns.
    pub sample_rows: usize,
}

impl Default for XlsxReadOptions {
    fn default() -> Self {
        XlsxReadOptions { sheet: SheetSelector::Index(0), header: true, skip_rows: 0, skip_empty_rows: true, sample_rows: 100 }
    }
}

/// Names of the worksheets of a workbook (for the import wizard).
pub fn xlsx_sheet_names(path: impl AsRef<Path>) -> DbResult<Vec<String>> {
    let path = path.as_ref();
    let wb = open_workbook_auto(path).map_err(|e| file_error("Cannot open", path, e))?;
    Ok(wb.sheet_names())
}

enum Msg {
    Rows(Vec<Row>),
    Warning(String),
    Error(DbError),
    /// The whole sheet was read.
    Done,
}

const BATCH: usize = 256;

/// Excel cell → value. Dates become `Date` (no time part), `Time` (serial
/// below one day) or `DateTime`.
fn cell_value(d: &DataRef<'_>, errors: &mut u64) -> Value {
    match d {
        DataRef::Empty => Value::Null,
        DataRef::Int(i) => Value::Int(*i),
        DataRef::Float(f) => Value::Float(*f),
        DataRef::String(s) => Value::text(s.as_str()),
        DataRef::SharedString(s) => Value::text(*s),
        DataRef::Bool(b) => Value::Bool(*b),
        DataRef::DateTime(dt) => {
            if dt.is_duration() {
                let total = (dt.as_f64() * 86_400.0).round() as i64;
                let (h, rest) = (total / 3600, total.rem_euclid(3600));
                return Value::text(format!("{h}:{:02}:{:02}", rest / 60, rest % 60));
            }
            let (y, mo, da, h, mi, s, ms) = dt.to_ymd_hms_milli();
            let time = NaiveTime::from_hms_milli_opt(u32::from(h), u32::from(mi), u32::from(s), u32::from(ms));
            if dt.as_f64() < 1.0 {
                return time.map(Value::Time).unwrap_or(Value::Null);
            }
            match (NaiveDate::from_ymd_opt(i32::from(y), u32::from(mo), u32::from(da)), time) {
                (Some(date), Some(t)) if t == NaiveTime::MIN => Value::Date(date),
                (Some(date), Some(t)) => Value::DateTime(date.and_time(t)),
                _ => Value::Float(dt.as_f64()),
            }
        }
        DataRef::DateTimeIso(s) => {
            if let Some(d) = parse_date(s) {
                Value::Date(d)
            } else if let Some(dt) = parse_datetime(s) {
                Value::DateTime(dt)
            } else if let Some(t) = parse_time(s) {
                Value::Time(t)
            } else {
                Value::text(s.as_str())
            }
        }
        DataRef::DurationIso(s) => Value::text(s.as_str()),
        DataRef::Error(_) => {
            *errors += 1;
            Value::Null
        }
    }
}

fn data_value(d: &Data, errors: &mut u64) -> Value {
    let r = match d {
        Data::Int(i) => DataRef::Int(*i),
        Data::Float(f) => DataRef::Float(*f),
        Data::String(s) => DataRef::SharedString(s.as_str()),
        Data::Bool(b) => DataRef::Bool(*b),
        Data::DateTime(dt) => DataRef::DateTime(*dt),
        Data::DateTimeIso(s) => DataRef::DateTimeIso(s.clone()),
        Data::DurationIso(s) => DataRef::DurationIso(s.clone()),
        Data::Error(e) => DataRef::Error(e.clone()),
        Data::Empty => DataRef::Empty,
    };
    cell_value(&r, errors)
}

/// Sends rows in batches; returns false when the receiver is gone.
struct Emitter {
    tx: Sender<Msg>,
    batch: Vec<Row>,
    skip_empty: bool,
}

impl Emitter {
    fn row(&mut self, row: Row) -> bool {
        if self.skip_empty && row.iter().all(Value::is_null) {
            return true;
        }
        self.batch.push(row);
        if self.batch.len() >= BATCH {
            return self.flush();
        }
        true
    }

    fn flush(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        self.tx.blocking_send(Msg::Rows(std::mem::take(&mut self.batch))).is_ok()
    }
}

fn read_workbook(path: PathBuf, sheet: SheetSelector, tx: Sender<Msg>, skip_empty: bool) {
    let mut em = Emitter { tx: tx.clone(), batch: Vec::with_capacity(BATCH), skip_empty };
    let mut errors = 0u64;
    let result = (|| -> DbResult<()> {
        let is_xlsx = path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "xlsx" | "xlsm"));
        if is_xlsx {
            let mut wb: Xlsx<_> = open_workbook(&path).map_err(|e| file_error("Cannot open", &path, e))?;
            let names = wb.sheet_names();
            let name = resolve_sheet(&names, &sheet)?;
            let mut cells = wb.worksheet_cells_reader(&name).map_err(|e| format_error("XLSX", e))?;
            let mut current: Option<u32> = None;
            let mut row: Row = Vec::new();
            while let Some(c) = cells.next_cell().map_err(|e| format_error("XLSX", e))? {
                let (r, col) = c.get_position();
                if current != Some(r) {
                    // Blank rows above the first cell count too (as in the other formats).
                    if current.is_none() && !skip_empty {
                        for _ in 0..r {
                            if !em.row(Vec::new()) {
                                return Ok(());
                            }
                        }
                    }
                    if let Some(prev) = current {
                        if !em.row(std::mem::take(&mut row)) {
                            return Ok(());
                        }
                        if !skip_empty {
                            for _ in prev + 1..r {
                                if !em.row(Vec::new()) {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    current = Some(r);
                }
                let col = col as usize;
                if row.len() <= col {
                    row.resize(col + 1, Value::Null);
                }
                row[col] = cell_value(c.get_value(), &mut errors);
            }
            if current.is_some() && !em.row(row) {
                return Ok(());
            }
        } else {
            let mut wb = open_workbook_auto(&path).map_err(|e| file_error("Cannot open", &path, e))?;
            let names = wb.sheet_names();
            let name = resolve_sheet(&names, &sheet)?;
            let range = wb.worksheet_range(&name).map_err(|e| format_error("spreadsheet", e))?;
            let (start_col, start_row) = range.start().map(|(r, c)| (c as usize, r)).unwrap_or((0, 0));
            if !skip_empty {
                for _ in 0..start_row {
                    if !em.row(Vec::new()) {
                        return Ok(());
                    }
                }
            }
            for r in range.rows() {
                let mut row: Row = vec![Value::Null; start_col];
                row.extend(r.iter().map(|d| data_value(d, &mut errors)));
                if !em.row(row) {
                    return Ok(());
                }
            }
        }
        em.flush();
        Ok(())
    })();
    if errors > 0 {
        let _ = tx.blocking_send(Msg::Warning(format!("{errors} cell(s) contained Excel errors and were read as NULL")));
    }
    let _ = tx.blocking_send(match result {
        Ok(()) => Msg::Done,
        Err(e) => Msg::Error(e),
    });
}

fn resolve_sheet(names: &[String], sheet: &SheetSelector) -> DbResult<String> {
    match sheet {
        SheetSelector::Index(i) => names.get(*i).cloned().ok_or_else(|| DbError::config(format!("The workbook has no sheet number {}", i + 1))),
        SheetSelector::Name(n) => names
            .iter()
            .find(|s| s == &n)
            .or_else(|| names.iter().find(|s| s.eq_ignore_ascii_case(n)))
            .cloned()
            .ok_or_else(|| DbError::config(format!("The workbook has no sheet named '{n}'"))),
    }
}

/// Streams the rows of one worksheet.
pub struct XlsxSource {
    rx: Receiver<Msg>,
    columns: Vec<ColumnMeta>,
    first_col: usize,
    pending: VecDeque<Row>,
    done: bool,
    warnings: Vec<String>,
    /// The warning about cells outside the columns was given.
    cut_warned: bool,
}

impl XlsxSource {
    /// Open a workbook (`.xlsx`, `.xlsm`, `.xls`, `.xlsb`, `.ods`).
    pub async fn open(path: impl AsRef<Path>, options: XlsxReadOptions) -> DbResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(file_error("Cannot open", &path, "file not found"));
        }
        let (tx, rx) = channel::<Msg>(4);
        let sheet = options.sheet.clone();
        let skip_empty = options.skip_empty_rows;
        std::thread::Builder::new()
            .name("dbiji-xlsx-reader".into())
            .spawn(move || read_workbook(path, sheet, tx, skip_empty))
            .map_err(|e| DbError::io(format!("Cannot start the XLSX reader: {e}")))?;
        let mut src = XlsxSource { rx, columns: Vec::new(), first_col: 0, pending: VecDeque::new(), done: false, warnings: Vec::new(), cut_warned: false };
        // Read the header and a sample to determine the columns.
        let needed = options.skip_rows + usize::from(options.header) + options.sample_rows.max(1);
        let mut head: Vec<Row> = Vec::new();
        while head.len() < needed {
            match src.recv().await? {
                Some(rows) => head.extend(rows),
                None => break,
            }
        }
        let mut head: VecDeque<Row> = head.into_iter().skip(options.skip_rows).collect();
        let header = if options.header { head.pop_front() } else { None };
        let mut first_col = usize::MAX;
        let mut last_col = 0usize;
        for r in header.iter().chain(head.iter()) {
            if let Some(f) = r.iter().position(|v| !v.is_null()) {
                first_col = first_col.min(f);
            }
            if let Some(l) = r.iter().rposition(|v| !v.is_null()) {
                last_col = last_col.max(l + 1);
            }
        }
        if first_col == usize::MAX {
            first_col = 0;
        }
        let width = last_col.saturating_sub(first_col);
        let mut names: Vec<String> = Vec::with_capacity(width);
        for i in 0..width {
            let raw = header
                .as_ref()
                .and_then(|h| h.get(first_col + i))
                .filter(|v| !v.is_null())
                .map(|v| v.to_text().trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("column{}", i + 1));
            let mut name = raw.clone();
            let mut n = 2;
            while names.contains(&name) {
                name = format!("{raw}_{n}");
                n += 1;
            }
            names.push(name);
        }
        src.columns = names.into_iter().map(|n| ColumnMeta::new(n, "", DataKind::Other)).collect();
        src.first_col = first_col;
        let rows: Vec<Row> = head.into_iter().map(|r| src.shape(r)).collect();
        src.pending.extend(rows);
        Ok(src)
    }

    fn shape(&mut self, row: Row) -> Row {
        let width = self.columns.len();
        // The columns come from the header and the first rows: say so when later cells fall outside.
        if !self.cut_warned && row.iter().skip(self.first_col + width).any(|v| !v.is_null()) {
            self.cut_warned = true;
            self.warnings
                .push("Some rows have values to the right of the columns found in the header and the first rows; those cells were not imported (give them a header)".into());
        }
        let mut out: Row = row.into_iter().skip(self.first_col).take(width).collect();
        out.resize(width, Value::Null);
        out
    }

    /// Next batch from the reader thread.
    async fn recv(&mut self) -> DbResult<Option<Vec<Row>>> {
        loop {
            if self.done {
                return Ok(None);
            }
            match self.rx.recv().await {
                Some(Msg::Rows(rows)) => return Ok(Some(rows)),
                Some(Msg::Warning(w)) => self.warnings.push(w),
                Some(Msg::Error(e)) => {
                    self.done = true;
                    return Err(e);
                }
                Some(Msg::Done) => {
                    self.done = true;
                    return Ok(None);
                }
                None => {
                    // The reader thread ended without finishing (it panicked).
                    self.done = true;
                    return Err(DbError::internal("The spreadsheet reader stopped unexpectedly"));
                }
            }
        }
    }
}

#[async_trait]
impl RowSource for XlsxSource {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let max = max.max(1);
        while self.pending.len() < max && !self.done {
            match self.recv().await? {
                Some(rows) => {
                    let shaped: Vec<Row> = rows.into_iter().map(|r| self.shape(r)).collect();
                    self.pending.extend(shaped);
                }
                None => break,
            }
        }
        let n = max.min(self.pending.len());
        Ok(self.pending.drain(..n).collect())
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}
