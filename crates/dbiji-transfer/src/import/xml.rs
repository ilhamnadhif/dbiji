//! XML reader for DBeaver style exports (`<export><DATA_RECORD><col>v</col>…`)
//! and generic documents made of repeated record elements, whose child
//! elements and attributes become columns.

use crate::encoding::DecodingReader;
use crate::error::{file_error, parse_error};
use crate::source::RowSource;
use async_trait::async_trait;
use base64::Engine;
use dbiji_core::{ColumnMeta, DataKind, DbError, DbResult, Row, Value};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Options of the XML reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XmlReadOptions {
    /// Input encoding label; `auto` uses the BOM, then the XML declaration, then UTF-8.
    pub encoding: String,
    /// Name of the record elements; `None` treats every child of the root element as a record.
    pub record_element: Option<String>,
    /// Attributes of record elements become columns.
    pub attributes_as_columns: bool,
    /// Empty elements (`<col/>`) are NULL instead of empty text.
    pub empty_as_null: bool,
    /// Records read ahead to determine the columns.
    pub sample_rows: usize,
}

impl Default for XmlReadOptions {
    fn default() -> Self {
        XmlReadOptions { encoding: "auto".into(), record_element: None, attributes_as_columns: true, empty_as_null: false, sample_rows: 100 }
    }
}

type Record = Vec<(String, Option<String>)>;

struct FieldState {
    name: String,
    text: String,
    null: bool,
    base64: bool,
}

type XmlInput = BufReader<DecodingReader<Box<dyn Read + Send>>>;

struct XmlParser {
    reader: quick_xml::Reader<XmlInput>,
    buf: Vec<u8>,
    depth: usize,
    record_element: Option<String>,
    attributes_as_columns: bool,
    empty_as_null: bool,
    record_depth: Option<usize>,
    record: Record,
    /// Name and direct text content of the current record element.
    record_name: String,
    record_text: String,
    field: Option<FieldState>,
    done: bool,
}

fn local_name(e: &BytesStart<'_>) -> String {
    let local = e.local_name();
    let name: &str = local.as_ref();
    name.to_string()
}

fn is_record(record_element: &Option<String>, depth: usize, name: &str) -> bool {
    match record_element {
        Some(r) => r == name,
        None => depth == 2,
    }
}

/// `null="true"` / `xsi:nil="true"` and `encoding="base64"` markers of a field element.
fn field_flags(e: &BytesStart<'_>) -> (bool, bool) {
    let mut null = false;
    let mut base64 = false;
    for a in e.attributes().flatten() {
        let key = AsRef::<str>::as_ref(&a.key.local_name()).to_ascii_lowercase();
        let value = a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.to_ascii_lowercase()).unwrap_or_default();
        match key.as_str() {
            "null" | "nil" if value == "true" || value == "1" => null = true,
            "encoding" if value == "base64" => base64 = true,
            _ => {}
        }
    }
    (null, base64)
}

fn resolve_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => return None,
    })
}

/// Append character data to the open field, or to the record's own text.
fn push_text(field: &mut Option<FieldState>, record_text: &mut String, directly_in_record: bool, piece: &str) {
    if let Some(f) = field.as_mut() {
        f.text.push_str(piece);
    } else if directly_in_record {
        record_text.push_str(piece);
    }
}

impl XmlParser {
    fn next_record(&mut self) -> DbResult<Option<Record>> {
        if self.done {
            return Ok(None);
        }
        loop {
            self.buf.clear();
            let event = match self.reader.read_event_into(&mut self.buf) {
                Ok(ev) => ev,
                Err(e) => {
                    let loc = format!("byte {}", self.reader.error_position());
                    self.done = true;
                    return Err(parse_error("XML", loc, e));
                }
            };
            match event {
                Event::Start(e) => {
                    self.depth += 1;
                    let name = local_name(&e);
                    match self.record_depth {
                        Some(rd) if self.depth == rd + 1 => {
                            let (null, base64) = field_flags(&e);
                            self.field = Some(FieldState { name, text: String::new(), null, base64 });
                        }
                        Some(_) => {}
                        None if is_record(&self.record_element, self.depth, &name) => {
                            self.record_depth = Some(self.depth);
                            self.record.clear();
                            self.record_text.clear();
                            self.record_name.clone_from(&name);
                            if self.attributes_as_columns {
                                for a in e.attributes().flatten() {
                                    let qname = AsRef::<str>::as_ref(&a.key).to_string();
                                    if qname.starts_with("xmlns") || qname.starts_with("xsi:") {
                                        continue;
                                    }
                                    let key = AsRef::<str>::as_ref(&a.key.local_name()).to_string();
                                    let value =
                                        a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).map_err(|err| parse_error("XML", format!("attribute {qname}"), err))?;
                                    self.record.push((key, Some(value)));
                                }
                            }
                        }
                        None => {}
                    }
                }
                Event::End(_) => {
                    if let Some(rd) = self.record_depth {
                        if self.depth == rd + 1 {
                            if let Some(f) = self.field.take() {
                                let value = if f.null {
                                    None
                                } else if f.base64 {
                                    let bytes = base64::engine::general_purpose::STANDARD
                                        .decode(f.text.trim().as_bytes())
                                        .map_err(|e| parse_error("XML", format!("element {}", f.name), e))?;
                                    Some(String::from_utf8_lossy(&bytes).into_owned())
                                } else if f.text.is_empty() && self.empty_as_null {
                                    None
                                } else {
                                    Some(f.text)
                                };
                                self.record.push((f.name, value));
                            }
                        } else if self.depth == rd {
                            self.record_depth = None;
                            self.depth = self.depth.saturating_sub(1);
                            // A record that carries text (`<tag>a</tag>`) gets a column named after it.
                            let text = std::mem::take(&mut self.record_text);
                            if !text.trim().is_empty() {
                                self.record.push((self.record_name.clone(), Some(text)));
                            }
                            return Ok(Some(std::mem::take(&mut self.record)));
                        }
                    }
                    self.depth = self.depth.saturating_sub(1);
                }
                Event::Text(t) => {
                    let piece = t.xml10_content();
                    push_text(&mut self.field, &mut self.record_text, self.record_depth == Some(self.depth), &piece);
                }
                Event::CData(c) => {
                    let piece = c.xml10_content();
                    push_text(&mut self.field, &mut self.record_text, self.record_depth == Some(self.depth), &piece);
                }
                Event::GeneralRef(r) => {
                    let piece = if r.is_char_ref() {
                        match r.resolve_char_ref() {
                            Ok(Some(c)) => c.to_string(),
                            _ => '\u{FFFD}'.to_string(),
                        }
                    } else {
                        match resolve_entity(&r) {
                            Some(s) => s.to_string(),
                            None => format!("&{};", &*r),
                        }
                    };
                    push_text(&mut self.field, &mut self.record_text, self.record_depth == Some(self.depth), &piece);
                }
                Event::Eof => {
                    self.done = true;
                    if self.record_depth.is_some() {
                        let loc = format!("byte {}", self.reader.buffer_position());
                        return Err(parse_error("XML", loc, "unexpected end of document inside a record"));
                    }
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

/// Encoding named in the XML declaration, if any.
fn declared_encoding(head: &[u8]) -> Option<String> {
    let head = &head[..head.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{FEFF}').trim_start();
    if !text.starts_with("<?xml") {
        return None;
    }
    let decl = &text[..text.find("?>")?];
    let pos = decl.find("encoding")?;
    let rest = decl[pos + "encoding".len()..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    Some(rest[..rest.find(quote)?].to_string())
}

/// Streams the records of an XML document as rows.
pub struct XmlSource {
    parser: XmlParser,
    columns: Vec<ColumnMeta>,
    index: HashMap<String, usize>,
    pending: VecDeque<Row>,
    unknown: Vec<String>,
}

impl XmlSource {
    /// Open an XML file.
    pub fn open(path: impl AsRef<Path>, options: XmlReadOptions) -> DbResult<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|e| file_error("Cannot open", path, e))?;
        Self::from_reader(file, options)
    }

    /// Read XML from any reader.
    pub fn from_reader(reader: impl Read + Send + 'static, options: XmlReadOptions) -> DbResult<Self> {
        let mut buffered = BufReader::new(Box::new(reader) as Box<dyn Read + Send>);
        let label = if options.encoding.trim().is_empty() || options.encoding.eq_ignore_ascii_case("auto") {
            let head = buffered.fill_buf().map_err(|e| DbError::io(format!("Cannot read XML input: {e}")))?;
            declared_encoding(head).unwrap_or_else(|| "auto".into())
        } else {
            options.encoding.clone()
        };
        let decoded = DecodingReader::new(Box::new(buffered) as Box<dyn Read + Send>, &label)?;
        let mut reader = quick_xml::Reader::from_reader(BufReader::with_capacity(64 * 1024, decoded));
        reader.config_mut().expand_empty_elements = true;
        let mut parser = XmlParser {
            reader,
            buf: Vec::with_capacity(4096),
            depth: 0,
            record_element: options.record_element.clone().filter(|r| !r.trim().is_empty()),
            attributes_as_columns: options.attributes_as_columns,
            empty_as_null: options.empty_as_null,
            record_depth: None,
            record: Vec::new(),
            record_name: String::new(),
            record_text: String::new(),
            field: None,
            done: false,
        };
        let mut sample = Vec::new();
        while sample.len() < options.sample_rows.max(1) {
            match parser.next_record()? {
                Some(r) => sample.push(r),
                None => break,
            }
        }
        let mut names: Vec<String> = Vec::new();
        let mut index = HashMap::new();
        for rec in &sample {
            for (n, _) in rec {
                if !index.contains_key(n) {
                    index.insert(n.clone(), names.len());
                    names.push(n.clone());
                }
            }
        }
        let columns = names.into_iter().map(|n| ColumnMeta::new(n, "", DataKind::Text)).collect();
        let mut src = XmlSource { parser, columns, index, pending: VecDeque::new(), unknown: Vec::new() };
        let rows: Vec<Row> = sample.into_iter().map(|r| src.make_row(r)).collect();
        src.pending.extend(rows);
        Ok(src)
    }

    /// Encoding used to read the document (after BOM / declaration detection).
    pub fn encoding(&self) -> &'static str {
        self.parser.reader.get_ref().get_ref().encoding().name()
    }

    fn make_row(&mut self, rec: Record) -> Row {
        let mut row = vec![Value::Null; self.columns.len()];
        for (name, value) in rec {
            match self.index.get(&name) {
                Some(&i) => row[i] = value.map(|v| Value::Text(v.into_boxed_str())).unwrap_or(Value::Null),
                None => {
                    if !self.unknown.contains(&name) && self.unknown.len() < 100 {
                        self.unknown.push(name);
                    }
                }
            }
        }
        row
    }
}

#[async_trait]
impl RowSource for XmlSource {
    fn binary_is_base64(&self) -> bool {
        true
    }

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
            match self.parser.next_record()? {
                Some(rec) => {
                    let row = self.make_row(rec);
                    out.push(row);
                }
                None => break,
            }
        }
        Ok(out)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        let mut w = Vec::new();
        if !self.unknown.is_empty() {
            w.push(format!("Elements not present in the first records were ignored: {}", std::mem::take(&mut self.unknown).join(", ")));
        }
        if self.parser.reader.get_ref().get_ref().had_errors() {
            w.push("The input contained byte sequences invalid in its encoding (replaced with U+FFFD)".into());
        }
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn rows_of(xml: &str, opts: XmlReadOptions) -> (Vec<String>, Vec<Row>) {
        let mut s = XmlSource::from_reader(std::io::Cursor::new(xml.as_bytes().to_vec()), opts).unwrap();
        let names = s.columns().iter().map(|c| c.name.clone()).collect();
        let mut rows = Vec::new();
        loop {
            let b = s.next_batch(2).await.unwrap();
            if b.is_empty() {
                break;
            }
            rows.extend(b);
        }
        (names, rows)
    }

    #[tokio::test]
    async fn dbeaver_style() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE export [ <!ELEMENT export (DATA_RECORD*)> ]>
<export>
  <DATA_RECORD>
    <id>1</id>
    <name>a &amp; b&#13;
c</name>
    <note null="true"/>
  </DATA_RECORD>
  <DATA_RECORD>
    <id>2</id>
    <name><![CDATA[<x>]]></name>
    <note></note>
    <bin encoding="base64">aMOp</bin>
  </DATA_RECORD>
</export>"#;
        let (names, rows) = rows_of(xml, XmlReadOptions::default()).await;
        assert_eq!(names, vec!["id", "name", "note", "bin"]);
        assert_eq!(rows[0], vec![Value::text("1"), Value::text("a & b\r\nc"), Value::Null, Value::Null]);
        assert_eq!(rows[1], vec![Value::text("2"), Value::text("<x>"), Value::text(""), Value::text("hé")]);
    }

    #[tokio::test]
    async fn generic_records_with_attributes() {
        let xml = r#"<orders><meta/><order id="7" status="new"><customer>Ann</customer><total>9.5</total></order>
<order id="8"><customer>Bob</customer><extra><deep>x</deep></extra></order></orders>"#;
        let opts = XmlReadOptions { record_element: Some("order".into()), ..Default::default() };
        let (names, rows) = rows_of(xml, opts).await;
        assert_eq!(names, vec!["id", "status", "customer", "total", "extra"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], Value::text("8"));
        assert_eq!(rows[1][1], Value::Null);
        assert_eq!(rows[1][4], Value::text("x"));
    }

    #[tokio::test]
    async fn text_only_records() {
        let xml = "<tags>\n  <tag>a &amp; b</tag>\n  <tag lang=\"en\">c</tag>\n</tags>";
        let (names, rows) = rows_of(xml, XmlReadOptions::default()).await;
        assert_eq!(names, vec!["tag", "lang"]);
        assert_eq!(rows, vec![vec![Value::text("a & b"), Value::Null], vec![Value::text("c"), Value::text("en")]]);
    }

    #[tokio::test]
    async fn declared_legacy_encoding() {
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><r><rec><v>caf".to_vec();
        bytes.push(0xE9);
        bytes.extend_from_slice(b"</v></rec></r>");
        let mut s = XmlSource::from_reader(std::io::Cursor::new(bytes), XmlReadOptions::default()).unwrap();
        assert_eq!(s.next_batch(10).await.unwrap(), vec![vec![Value::text("café")]]);
        assert_eq!(declared_encoding(b"<?xml version='1.0' encoding='ISO-8859-2' ?>"), Some("ISO-8859-2".into()));
    }

    #[tokio::test]
    async fn malformed_reports_error() {
        let r = XmlSource::from_reader(std::io::Cursor::new(b"<a><b><c>1</b></a>".to_vec()), XmlReadOptions::default());
        assert!(r.is_err());
    }
}
