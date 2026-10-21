//! CSV / TSV reader.
//!
//! A streaming parser that remembers whether a field was quoted: an unquoted
//! field equal to the NULL string (empty by default) is NULL, a quoted one is
//! always text, so `,"",` (empty text) and `,,` (NULL) stay distinguishable
//! and the CSV export round trips exactly.

use super::infer::{Guess, InferOptions, classify_text};
use crate::encoding::DecodingReader;
use crate::error::{file_error, parse_error};
use crate::options::EscapeStyle;
use crate::sink::unique_names;
use crate::source::RowSource;
use async_trait::async_trait;
use dbiji_core::{ColumnMeta, DataKind, DbError, DbResult, Row, Value};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;

/// Where column names come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CsvHeader {
    /// The first record (after `skip_rows`) holds the column names.
    #[default]
    FirstRow,
    /// No header; columns are named `column1`, `column2` ...
    None,
    /// Detect: the first record is a header when all its fields are distinct,
    /// non-empty texts that do not look like numbers, dates or booleans.
    Auto,
}

/// Options of the CSV reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvReadOptions {
    /// Input encoding label; `auto` detects BOM / UTF-8 / windows-1252.
    pub encoding: String,
    /// Field delimiter (may be several characters, e.g. `||`).
    pub delimiter: String,
    /// Quote character (`None` disables quoting). Must be ASCII.
    pub quote_char: Option<char>,
    /// Escaping inside quoted fields.
    pub escape: EscapeStyle,
    /// Header handling.
    pub header: CsvHeader,
    /// Records skipped before the header (or the data).
    pub skip_rows: usize,
    /// Trim spaces around unquoted fields (and before / after quotes).
    pub trim: bool,
    /// Unquoted fields equal to this text are NULL.
    pub null_string: String,
    /// Lines starting with this character are ignored. Must be ASCII.
    pub comment_char: Option<char>,
    /// Records read ahead to determine the number of columns.
    pub sample_rows: usize,
}

impl Default for CsvReadOptions {
    fn default() -> Self {
        CsvReadOptions {
            encoding: "auto".into(),
            delimiter: ",".into(),
            quote_char: Some('"'),
            escape: EscapeStyle::Doubled,
            header: CsvHeader::FirstRow,
            skip_rows: 0,
            trim: false,
            null_string: String::new(),
            comment_char: None,
            sample_rows: 100,
        }
    }
}

impl CsvReadOptions {
    /// Options for tab separated files.
    pub fn tsv() -> Self {
        CsvReadOptions { delimiter: "\t".into(), ..Default::default() }
    }
}

pub(crate) struct Field {
    pub(crate) text: String,
    pub(crate) quoted: bool,
}

/// Streaming CSV record parser over UTF-8 bytes.
pub(crate) struct CsvParser<R: Read> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    delimiter: Vec<u8>,
    quote: Option<u8>,
    escape: EscapeStyle,
    comment: Option<u8>,
    trim: bool,
    /// A blank line is a record with one empty field (one-column files: the
    /// CSV export writes a NULL as an empty line) instead of being skipped.
    pub(crate) keep_blank_lines: bool,
    /// Blank lines read ahead (followed by data) still to be returned.
    blank_pending: usize,
    /// Current line number (1-based).
    pub(crate) line: u64,
    pub(crate) unterminated_quotes: u64,
}

fn ascii_option(c: Option<char>, what: &str) -> DbResult<Option<u8>> {
    match c {
        None => Ok(None),
        Some(c) if c.is_ascii() => Ok(Some(c as u8)),
        Some(c) => Err(DbError::config(format!("The CSV {what} must be an ASCII character, not '{c}'"))),
    }
}

impl<R: Read> CsvParser<R> {
    pub(crate) fn new(reader: R, opts: &CsvReadOptions) -> DbResult<Self> {
        if opts.delimiter.is_empty() {
            return Err(DbError::config("The CSV delimiter must not be empty"));
        }
        Ok(CsvParser {
            reader,
            buf: vec![0u8; 64 * 1024],
            pos: 0,
            len: 0,
            eof: false,
            delimiter: opts.delimiter.as_bytes().to_vec(),
            quote: ascii_option(opts.quote_char, "quote character")?,
            escape: opts.escape,
            comment: ascii_option(opts.comment_char, "comment character")?,
            trim: opts.trim,
            keep_blank_lines: false,
            blank_pending: 0,
            line: 1,
            unterminated_quotes: 0,
        })
    }

    /// Make at least `n` bytes available unless the input ends first.
    fn fill(&mut self, n: usize) -> DbResult<bool> {
        while self.len - self.pos < n && !self.eof {
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.len, 0);
                self.len -= self.pos;
                self.pos = 0;
            }
            if self.len == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
            }
            match self.reader.read(&mut self.buf[self.len..]) {
                Ok(0) => self.eof = true,
                Ok(k) => self.len += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(DbError::io(format!("Cannot read CSV input: {e}"))),
            }
        }
        Ok(self.len - self.pos >= n)
    }

    fn peek(&mut self) -> DbResult<Option<u8>> {
        if self.pos < self.len || self.fill(1)? { Ok(Some(self.buf[self.pos])) } else { Ok(None) }
    }

    fn bump(&mut self) -> DbResult<Option<u8>> {
        let b = self.peek()?;
        if let Some(c) = b {
            self.pos += 1;
            if c == b'\n' {
                self.line += 1;
            }
        }
        Ok(b)
    }

    fn at_delimiter(&mut self) -> DbResult<bool> {
        let n = self.delimiter.len();
        if !self.fill(n)? {
            return Ok(false);
        }
        Ok(self.buf[self.pos..self.pos + n] == self.delimiter[..])
    }

    /// Consume a line terminator (`\n`, `\r\n` or `\r`) if one follows.
    fn eat_line_end(&mut self) -> DbResult<bool> {
        match self.peek()? {
            Some(b'\n') => {
                self.bump()?;
                Ok(true)
            }
            Some(b'\r') => {
                self.pos += 1;
                if self.peek()? == Some(b'\n') {
                    self.pos += 1;
                }
                self.line += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn skip_line(&mut self) -> DbResult<()> {
        loop {
            match self.peek()? {
                None => return Ok(()),
                Some(b'\n' | b'\r') => {
                    self.eat_line_end()?;
                    return Ok(());
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn skip_spaces(&mut self) -> DbResult<()> {
        while let Some(b' ' | b'\t') = self.peek()? {
            if self.delimiter.first() == Some(&b'\t') && self.peek()? == Some(b'\t') {
                break;
            }
            self.pos += 1;
        }
        Ok(())
    }

    /// Next record, or `None` at the end of the input. Blank lines and comment lines are skipped.
    pub(crate) fn read_record(&mut self) -> DbResult<Option<Vec<Field>>> {
        let blank = || vec![Field { text: String::new(), quoted: false }];
        if self.blank_pending > 0 {
            self.blank_pending -= 1;
            return Ok(Some(blank()));
        }
        loop {
            match self.peek()? {
                None => return Ok(None),
                Some(b'\n' | b'\r') => {
                    self.eat_line_end()?;
                    if self.keep_blank_lines {
                        // Blank lines at the end of the file are no NULL values.
                        let mut more = 0;
                        while matches!(self.peek()?, Some(b'\n' | b'\r')) {
                            self.eat_line_end()?;
                            more += 1;
                        }
                        if self.peek()?.is_none() {
                            return Ok(None);
                        }
                        self.blank_pending = more;
                        return Ok(Some(blank()));
                    }
                    continue;
                }
                Some(c) if Some(c) == self.comment => {
                    self.skip_line()?;
                    continue;
                }
                Some(_) => break,
            }
        }
        let mut fields = Vec::new();
        let mut bytes: Vec<u8> = Vec::with_capacity(64);
        loop {
            bytes.clear();
            let mut quoted = false;
            if self.trim {
                self.skip_spaces()?;
            }
            if self.quote.is_some() && self.peek()? == self.quote {
                quoted = true;
                self.pos += 1;
                self.read_quoted(&mut bytes)?;
                // Anything between the closing quote and the delimiter is kept (lenient).
                loop {
                    if self.at_delimiter()? {
                        break;
                    }
                    match self.peek()? {
                        None | Some(b'\n' | b'\r') => break,
                        Some(b' ' | b'\t') if self.trim => self.pos += 1,
                        Some(c) => {
                            bytes.push(c);
                            self.pos += 1;
                        }
                    }
                }
            } else {
                self.read_unquoted(&mut bytes)?;
                if self.trim {
                    while bytes.last().is_some_and(|b| *b == b' ' || *b == b'\t') {
                        bytes.pop();
                    }
                }
            }
            let text = match String::from_utf8(std::mem::take(&mut bytes)) {
                Ok(s) => s,
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            };
            fields.push(Field { text, quoted });
            if self.at_delimiter()? {
                self.pos += self.delimiter.len();
                continue;
            }
            self.eat_line_end()?;
            return Ok(Some(fields));
        }
    }

    /// Copy an unquoted field up to the delimiter or line end (not consumed).
    fn read_unquoted(&mut self, out: &mut Vec<u8>) -> DbResult<()> {
        let first = self.delimiter[0];
        loop {
            // Fast path: copy bytes that cannot start a delimiter or a line end in bulk.
            let start = self.pos;
            let mut i = self.pos;
            while i < self.len {
                let b = self.buf[i];
                if b == first || b == b'\n' || b == b'\r' {
                    break;
                }
                i += 1;
            }
            out.extend_from_slice(&self.buf[start..i]);
            self.pos = i;
            if self.pos >= self.len {
                if !self.fill(1)? {
                    return Ok(());
                }
                continue;
            }
            if self.at_delimiter()? {
                return Ok(());
            }
            match self.buf[self.pos] {
                b'\n' | b'\r' => return Ok(()),
                c => {
                    // First byte of the delimiter without the rest of it.
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn read_quoted(&mut self, out: &mut Vec<u8>) -> DbResult<()> {
        let Some(q) = self.quote else { return Ok(()) };
        loop {
            // Fast path: copy plain bytes in bulk.
            let start = self.pos;
            let mut i = self.pos;
            while i < self.len {
                let b = self.buf[i];
                if b == q || (b == b'\\' && self.escape == EscapeStyle::Backslash) {
                    break;
                }
                if b == b'\n' {
                    self.line += 1;
                }
                i += 1;
            }
            out.extend_from_slice(&self.buf[start..i]);
            self.pos = i;
            let Some(b) = self.peek()? else {
                self.unterminated_quotes += 1;
                return Ok(());
            };
            if b == q {
                self.pos += 1;
                if self.escape == EscapeStyle::Doubled && self.peek()? == Some(q) {
                    self.pos += 1;
                    out.push(q);
                    continue;
                }
                return Ok(());
            }
            if b == b'\\' && self.escape == EscapeStyle::Backslash {
                self.pos += 1;
                match self.bump()? {
                    Some(c) => out.push(c),
                    None => out.push(b'\\'),
                }
                continue;
            }
            // Other bytes are handled by the fast path after a refill.
            if self.pos >= self.len {
                self.fill(1)?;
            }
        }
    }
}

/// Streams the rows of a CSV file.
pub struct CsvSource {
    parser: CsvParser<DecodingReader<Box<dyn Read + Send>>>,
    columns: Vec<ColumnMeta>,
    pending: VecDeque<Row>,
    null_string: String,
    records: u64,
    ragged: u64,
    first_data_line: u64,
    done: bool,
    warnings: Vec<String>,
}

impl CsvSource {
    /// Open a CSV file.
    pub fn open(path: impl AsRef<Path>, options: CsvReadOptions) -> DbResult<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|e| file_error("Cannot open", path, e))?;
        Self::from_reader(file, options)
    }

    /// Read CSV from any reader (clipboard text, network stream ...).
    pub fn from_reader(reader: impl Read + Send + 'static, options: CsvReadOptions) -> DbResult<Self> {
        let decoded = DecodingReader::new(Box::new(reader) as Box<dyn Read + Send>, &options.encoding)?;
        let mut parser = CsvParser::new(decoded, &options)?;
        for _ in 0..options.skip_rows {
            if parser.read_record()?.is_none() {
                break;
            }
        }
        let mut sample: Vec<Vec<Field>> = Vec::new();
        let first = parser.read_record()?;
        // One column: its blank lines are NULL values, not separators.
        parser.keep_blank_lines = first.as_ref().is_some_and(|f| f.len() == 1);
        let header_line = parser.line;
        let mut header: Option<Vec<Field>> = None;
        if let Some(first) = first {
            match options.header {
                CsvHeader::FirstRow => header = Some(first),
                CsvHeader::None => sample.push(first),
                CsvHeader::Auto => {
                    let mut rest = Vec::new();
                    while rest.len() < options.sample_rows.max(1) {
                        match parser.read_record()? {
                            Some(r) => rest.push(r),
                            None => break,
                        }
                    }
                    if looks_like_header(&first, &rest) {
                        header = Some(first);
                    } else {
                        sample.push(first);
                    }
                    sample.extend(rest);
                }
            }
        }
        while sample.len() < options.sample_rows.max(1) {
            match parser.read_record()? {
                Some(r) => sample.push(r),
                None => break,
            }
        }
        let width = header.as_ref().map_or(0, Vec::len).max(sample.iter().map(Vec::len).max().unwrap_or(0));
        let mut metas: Vec<ColumnMeta> = (0..width)
            .map(|i| {
                let name = header.as_ref().and_then(|h| h.get(i)).map(|f| f.text.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("column{}", i + 1));
                ColumnMeta::new(name, "", DataKind::Text)
            })
            .collect();
        let unique = unique_names(&metas);
        for (m, n) in metas.iter_mut().zip(unique) {
            m.name = n;
        }
        let mut src = CsvSource {
            parser,
            columns: metas,
            pending: VecDeque::new(),
            null_string: options.null_string.clone(),
            records: 0,
            ragged: 0,
            first_data_line: if header.is_some() { header_line } else { 1 },
            done: false,
            warnings: Vec::new(),
        };
        let rows: Vec<Row> = sample.into_iter().map(|r| src.make_row(r)).collect();
        src.pending.extend(rows);
        Ok(src)
    }

    fn make_row(&mut self, fields: Vec<Field>) -> Row {
        let width = self.columns.len();
        if fields.len() != width {
            self.ragged += 1;
        }
        let mut row: Row =
            fields.into_iter().take(width).map(|f| if !f.quoted && f.text == self.null_string { Value::Null } else { Value::Text(f.text.into_boxed_str()) }).collect();
        row.resize(width, Value::Null);
        row
    }

    /// Line where the first data record starts (for error messages).
    pub fn first_data_line(&self) -> u64 {
        self.first_data_line
    }

    /// Encoding used to read the file (after BOM sniffing / detection), e.g. `windows-1252`.
    pub fn encoding(&self) -> &'static str {
        self.parser.reader.encoding().name()
    }
}

/// Header heuristic for [`CsvHeader::Auto`].
fn looks_like_header(first: &[Field], rest: &[Vec<Field>]) -> bool {
    let opts = InferOptions::default();
    if first.is_empty() || first.iter().any(|f| f.text.trim().is_empty()) {
        return false;
    }
    if first.iter().any(|f| classify_text(&f.text, &opts) != Guess::Text) {
        return false;
    }
    let mut names: Vec<&str> = first.iter().map(|f| f.text.trim()).collect();
    names.sort_unstable();
    names.dedup();
    if names.len() != first.len() {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    // A header value repeated in its own column suggests data.
    !first.iter().enumerate().any(|(i, f)| rest.iter().any(|r| r.get(i).is_some_and(|g| g.text == f.text)))
}

#[async_trait]
impl RowSource for CsvSource {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    async fn next_batch(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let max = max.max(1);
        let mut out = Vec::with_capacity(max.min(4096));
        while out.len() < max {
            if let Some(r) = self.pending.pop_front() {
                out.push(r);
                continue;
            }
            if self.done {
                break;
            }
            let line = self.parser.line;
            match self.parser.read_record() {
                Ok(Some(fields)) => {
                    let row = self.make_row(fields);
                    out.push(row);
                }
                Ok(None) => self.done = true,
                Err(e) => return Err(parse_error("CSV", format!("line {line}"), e)),
            }
        }
        self.records += out.len() as u64;
        Ok(out)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        let mut w = std::mem::take(&mut self.warnings);
        if self.ragged > 0 {
            w.push(format!("{} record(s) had a different number of fields than the {} columns (padded with NULL / truncated)", self.ragged, self.columns.len()));
            self.ragged = 0;
        }
        if self.parser.unterminated_quotes > 0 {
            w.push(format!("{} quoted field(s) were not terminated", self.parser.unterminated_quotes));
            self.parser.unterminated_quotes = 0;
        }
        if self.parser.reader_had_errors() {
            w.push("The input contained byte sequences invalid in its encoding (replaced with U+FFFD)".into());
        }
        w
    }
}

impl<R: Read> CsvParser<DecodingReader<R>> {
    fn reader_had_errors(&self) -> bool {
        self.reader.had_errors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str, opts: &CsvReadOptions) -> Vec<Vec<(String, bool)>> {
        let mut p = CsvParser::new(input.as_bytes(), opts).unwrap();
        let mut out = Vec::new();
        while let Some(r) = p.read_record().unwrap() {
            out.push(r.into_iter().map(|f| (f.text, f.quoted)).collect());
        }
        out
    }

    fn texts(input: &str, opts: &CsvReadOptions) -> Vec<Vec<String>> {
        parse(input, opts).into_iter().map(|r| r.into_iter().map(|f| f.0).collect()).collect()
    }

    #[test]
    fn quoting_and_line_endings() {
        let o = CsvReadOptions::default();
        assert_eq!(texts("a,b\r\n\"x,1\",\"he said \"\"hi\"\"\"\n\"multi\nline\",\n", &o), vec![vec!["a", "b"], vec!["x,1", "he said \"hi\""], vec!["multi\nline", ""]]);
        let r = parse("1,\"\",\n", &o);
        assert_eq!(r[0], vec![("1".into(), false), ("".into(), true), ("".into(), false)]);
        assert_eq!(texts("a\rb\r", &o), vec![vec!["a"], vec!["b"]]);
        assert_eq!(texts("\n\na\n\n", &o), vec![vec!["a"]]);
    }

    #[test]
    fn multi_char_delimiter_backslash_comment_trim() {
        let o = CsvReadOptions { delimiter: "||".into(), escape: EscapeStyle::Backslash, comment_char: Some('#'), trim: true, ..Default::default() };
        assert_eq!(texts("# comment\n a || \"b\\\"c\\\\\" ||d|e\n", &o), vec![vec!["a", "b\"c\\", "d|e"]]);
    }

    #[test]
    fn unterminated_quote_is_lenient() {
        let o = CsvReadOptions::default();
        let mut p = CsvParser::new("a,\"bc\nd".as_bytes(), &o).unwrap();
        let r = p.read_record().unwrap().unwrap();
        assert_eq!(r[1].text, "bc\nd");
        assert_eq!(p.unterminated_quotes, 1);
    }

    #[test]
    fn small_buffers_and_long_fields() {
        let long = "x".repeat(200_000);
        let input = format!("\"{long}\",{long}\n");
        let o = CsvReadOptions::default();
        let r = texts(&input, &o);
        assert_eq!(r[0][0].len(), 200_000);
        assert_eq!(r[0][1].len(), 200_000);
    }

    #[tokio::test]
    async fn source_header_null_ragged() {
        let csv = "id,name,note\n1,\"\",\n2,x\n3,y,z,extra\n";
        let mut s = CsvSource::from_reader(std::io::Cursor::new(csv.as_bytes().to_vec()), CsvReadOptions::default()).unwrap();
        assert_eq!(s.columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name", "note", "column4"]);
        let rows = s.next_batch(10).await.unwrap();
        assert_eq!(rows[0], vec![Value::text("1"), Value::text(""), Value::Null, Value::Null]);
        assert_eq!(rows[2][3], Value::text("extra"));
        assert!(s.next_batch(10).await.unwrap().is_empty());
        assert!(!s.take_warnings().is_empty());
    }

    #[tokio::test]
    async fn one_column_blank_lines_are_nulls() {
        // what the CSV export writes for a single column with NULLs
        let csv = "note\na\n\nb\n";
        let mut s = CsvSource::from_reader(std::io::Cursor::new(csv.as_bytes().to_vec()), CsvReadOptions::default()).unwrap();
        let rows = s.next_batch(10).await.unwrap();
        assert_eq!(rows, vec![vec![Value::text("a")], vec![Value::Null], vec![Value::text("b")]]);
        // blank lines at the end are not rows
        let csv = "note\na\n\n\nb\n\n\n";
        let mut s = CsvSource::from_reader(std::io::Cursor::new(csv.as_bytes().to_vec()), CsvReadOptions::default()).unwrap();
        assert_eq!(s.next_batch(10).await.unwrap(), vec![vec![Value::text("a")], vec![Value::Null], vec![Value::Null], vec![Value::text("b")]]);
        // several columns: blank lines are still skipped
        let csv = "a,b\n1,2\n\n3,4\n";
        let mut s = CsvSource::from_reader(std::io::Cursor::new(csv.as_bytes().to_vec()), CsvReadOptions::default()).unwrap();
        assert_eq!(s.next_batch(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn header_detection() {
        let with = "name,age\nann,31\nbob,42\n";
        let s = CsvSource::from_reader(std::io::Cursor::new(with.as_bytes().to_vec()), CsvReadOptions { header: CsvHeader::Auto, ..Default::default() }).unwrap();
        assert_eq!(s.columns()[1].name, "age");
        let without = "1,2\n3,4\n";
        let mut s = CsvSource::from_reader(std::io::Cursor::new(without.as_bytes().to_vec()), CsvReadOptions { header: CsvHeader::Auto, ..Default::default() }).unwrap();
        assert_eq!(s.columns()[0].name, "column1");
        assert_eq!(s.next_batch(10).await.unwrap().len(), 2);
    }
}
