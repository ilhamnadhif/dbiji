//! Incremental parser for `JSONCompactEachRowWithNamesAndTypes` response
//! bodies, including exceptions raised after streaming has started.
//!
//! ClickHouse sends the HTTP status and headers before the query finishes, so
//! an error during execution is written into the body. Two shapes exist:
//!
//! * ClickHouse ≥ 24.11 (with the `X-ClickHouse-Exception-Tag` header):
//!   ```text
//!   \r\n__exception__\r\n<tag>\r\n<message>\n<length> <tag>\r\n__exception__\r\n
//!   ```
//! * older servers append the plain text `Code: N. DB::Exception: ...`,
//!   possibly in the middle of a partially written row.

use crate::error::{exception_to_error, find_exception_start};
use crate::json::{self, Json};
use dbiji_core::DbError;
use std::io::{BufRead, BufReader, Read};

/// Marker line surrounding exception blocks.
pub const EXCEPTION_MARKER: &str = "__exception__";

/// Maximum number of bytes collected for an exception message.
const MAX_EXCEPTION_TEXT: usize = 256 * 1024;

/// What one body line turned out to be.
#[derive(Debug, PartialEq)]
pub enum Feed {
    /// A data (or header) row.
    Row(Vec<Json>),
    /// Nothing to emit (blank line, part of an exception block).
    Nothing,
    /// A complete exception; the stream is over.
    Error(DbError),
}

#[derive(Debug)]
enum State {
    Normal,
    /// Inside a tagged exception block.
    Tagged { saw_tag: bool, text: String },
    /// Legacy exception: collect the rest of the body.
    Legacy { text: String },
    /// The exception has been reported; ignore the rest.
    Done,
}

/// Line-by-line body parser.
#[derive(Debug)]
pub struct BodyParser {
    tag: Option<String>,
    header_code: Option<String>,
    state: State,
}

impl BodyParser {
    /// `tag` is the `X-ClickHouse-Exception-Tag` response header and
    /// `header_code` the `X-ClickHouse-Exception-Code` header, if any.
    pub fn new(tag: Option<String>, header_code: Option<String>) -> Self {
        BodyParser { tag: tag.filter(|t| !t.is_empty()), header_code, state: State::Normal }
    }

    fn error(&self, text: &str) -> DbError {
        exception_to_error(text, self.header_code.as_deref())
    }

    /// Feed one line without its trailing `\n` (a trailing `\r` is tolerated).
    pub fn feed(&mut self, raw: &[u8]) -> Feed {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        match &mut self.state {
            State::Done => Feed::Nothing,
            State::Legacy { text } => {
                if text.len() < MAX_EXCEPTION_TEXT {
                    text.push('\n');
                    text.push_str(&String::from_utf8_lossy(line));
                }
                Feed::Nothing
            }
            State::Tagged { saw_tag, text } => {
                let s = String::from_utf8_lossy(line);
                if !*saw_tag {
                    *saw_tag = true;
                    match &self.tag {
                        Some(t) if s.trim() == t => return Feed::Nothing,
                        // no tag known: treat the line as the tag if it looks like one
                        None if !s.contains(' ') && !s.starts_with("Code:") => return Feed::Nothing,
                        _ => {}
                    }
                }
                if is_tag_trailer(&s, self.tag.as_deref()) || s.trim() == EXCEPTION_MARKER {
                    let msg = std::mem::take(text);
                    self.state = State::Done;
                    return Feed::Error(self.error(&msg));
                }
                if text.len() < MAX_EXCEPTION_TEXT {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&s);
                }
                Feed::Nothing
            }
            State::Normal => {
                if line.is_empty() || line.iter().all(u8::is_ascii_whitespace) {
                    return Feed::Nothing;
                }
                if line == EXCEPTION_MARKER.as_bytes() {
                    self.state = State::Tagged { saw_tag: false, text: String::new() };
                    return Feed::Nothing;
                }
                match json::parse_row(line) {
                    Ok(row) => Feed::Row(row),
                    Err(e) => {
                        let s = String::from_utf8_lossy(line);
                        if let Some(i) = find_exception_start(&s) {
                            self.state = State::Legacy { text: s[i..].to_string() };
                            Feed::Nothing
                        } else {
                            self.state = State::Done;
                            Feed::Error(DbError::connection(format!(
                                "Malformed row in ClickHouse response ({e})"
                            )))
                        }
                    }
                }
            }
        }
    }

    /// Call at end of body: returns a pending exception, if any.
    pub fn finish(&mut self) -> Option<DbError> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Legacy { text } => Some(self.error(&text)),
            State::Tagged { text, .. } => Some(if text.is_empty() {
                DbError::connection("The ClickHouse response ended inside an exception block")
            } else {
                self.error(&text)
            }),
            _ => None,
        }
    }
}

/// `"<length> <tag>"` closing an exception message.
fn is_tag_trailer(line: &str, tag: Option<&str>) -> bool {
    let line = line.trim();
    let Some((len, rest)) = line.split_once(' ') else { return false };
    if len.is_empty() || !len.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match tag {
        Some(t) => rest == t,
        None => !rest.is_empty() && !rest.contains(' '),
    }
}

/// Column names and types from the two header rows.
pub fn header_columns(names: Vec<Json>, types: Vec<Json>) -> Result<Vec<(String, String)>, DbError> {
    if names.len() != types.len() {
        return Err(DbError::connection(format!(
            "ClickHouse sent {} column names but {} types",
            names.len(),
            types.len()
        )));
    }
    names
        .into_iter()
        .zip(types)
        .map(|(n, t)| match (n, t) {
            (Json::String(n), Json::String(t)) => Ok((n, t)),
            _ => Err(DbError::connection("Malformed header row in ClickHouse response")),
        })
        .collect()
}

/// Reads `\n` terminated lines from a body reader into a reusable buffer.
pub struct LineReader<R: Read> {
    inner: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: Read> LineReader<R> {
    /// Wrap a reader with a read buffer of `capacity` bytes.
    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        LineReader { inner: BufReader::with_capacity(capacity, inner), buf: Vec::with_capacity(4096) }
    }

    /// Wrap a reader with a default sized buffer.
    pub fn new(inner: R) -> Self {
        Self::with_capacity(64 * 1024, inner)
    }

    /// Next line without the line terminator; `Ok(None)` at end of body.
    pub fn next_line(&mut self) -> std::io::Result<Option<&[u8]>> {
        self.buf.clear();
        let n = self.inner.read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
        }
        Ok(Some(&self.buf))
    }

    /// True when the next read would have to wait for the network.
    pub fn buffer_is_empty(&self) -> bool {
        self.inner.buffer().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ErrorKind;

    fn feed_all(p: &mut BodyParser, body: &[u8]) -> (Vec<Vec<Json>>, Option<DbError>) {
        let mut rows = Vec::new();
        let mut reader = LineReader::new(body);
        while let Some(line) = reader.next_line().unwrap() {
            match p.feed(line) {
                Feed::Row(r) => rows.push(r),
                Feed::Nothing => {}
                Feed::Error(e) => return (rows, Some(e)),
            }
        }
        (rows, p.finish())
    }

    #[test]
    fn plain_rows() {
        let mut p = BodyParser::new(Some("abc".into()), None);
        let (rows, err) = feed_all(&mut p, b"[\"a\"]\n[\"String\"]\n[\"x\"]\n[\"y\"]\n");
        assert!(err.is_none());
        assert_eq!(rows.len(), 4);
        let cols = header_columns(rows[0].clone(), rows[1].clone()).unwrap();
        assert_eq!(cols, vec![("a".to_string(), "String".to_string())]);
    }

    #[test]
    fn tagged_mid_stream_exception() {
        let body = b"[\"0\", 0]\n[\"1\", 0]\n\r\n__exception__\r\nelsmzfvtzzmhjaxb\r\nCode: 395. DB::Exception: boom mid stream: while executing 'FUNCTION throwIf()'. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 26.9.19.9 (official build))\n317 elsmzfvtzzmhjaxb\r\n__exception__\r\n";
        let mut p = BodyParser::new(Some("elsmzfvtzzmhjaxb".into()), None);
        let (rows, err) = feed_all(&mut p, body);
        assert_eq!(rows.len(), 2);
        let err = err.expect("exception");
        assert_eq!(err.code.as_deref(), Some("395 FUNCTION_THROW_IF_VALUE_IS_NON_ZERO"));
        assert!(err.message.starts_with("boom mid stream"), "{}", err.message);
        assert_eq!(err.kind, ErrorKind::Query);
    }

    #[test]
    fn tagged_exception_without_known_tag() {
        let body = b"[1]\n__exception__\nqwerty\nCode: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)\n80 qwerty\n__exception__\n";
        let mut p = BodyParser::new(None, None);
        let (rows, err) = feed_all(&mut p, body);
        assert_eq!(rows.len(), 1);
        assert_eq!(err.unwrap().kind, ErrorKind::Cancelled);
    }

    #[test]
    fn legacy_exception_inside_row() {
        let body = b"[1, \"a\"]\n[2, \"bCode: 241. DB::Exception: Memory limit exceeded. (MEMORY_LIMIT_EXCEEDED)\nmore text\n";
        let mut p = BodyParser::new(None, None);
        let (rows, err) = feed_all(&mut p, body);
        assert_eq!(rows.len(), 1);
        let err = err.unwrap();
        assert_eq!(err.code.as_deref(), Some("241 MEMORY_LIMIT_EXCEEDED"));
        assert!(err.message.contains("Memory limit exceeded"));
    }

    #[test]
    fn legacy_exception_line() {
        let body = b"[1]\nCode: 159. DB::Exception: Timeout exceeded: elapsed 1.0 seconds. (TIMEOUT_EXCEEDED)\n";
        let mut p = BodyParser::new(None, None);
        let (rows, err) = feed_all(&mut p, body);
        assert_eq!(rows.len(), 1);
        assert_eq!(err.unwrap().kind, ErrorKind::Timeout);
    }

    #[test]
    fn garbage_is_a_connection_error() {
        let mut p = BodyParser::new(None, None);
        let (_, err) = feed_all(&mut p, b"[1]\nnot json at all\n");
        assert_eq!(err.unwrap().kind, ErrorKind::Connection);
    }

    #[test]
    fn truncated_exception_block() {
        let mut p = BodyParser::new(Some("t".into()), None);
        let (_, err) = feed_all(&mut p, b"[1]\n__exception__\nt\nCode: 1. DB::Exception: partial");
        let err = err.unwrap();
        assert!(err.message.contains("partial"));
    }

    #[test]
    fn header_mismatch() {
        assert!(header_columns(vec![Json::String("a".into())], vec![]).is_err());
        assert!(header_columns(vec![Json::Null], vec![Json::String("x".into())]).is_err());
    }
}
