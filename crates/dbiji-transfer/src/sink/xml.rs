//! XML (DBeaver style) and DbUnit flat XML export.

use super::{ExportSummary, RowSink, TextOutput, cell, source_table_name, unique_names};
use crate::format::{BinaryEncoding, ValueFormat, format_value};
use crate::options::{DbUnitOptions, ExportOptions, XmlNullMode, XmlOptions};
use base64::Engine;
use dbiji_core::{ColumnMeta, DbResult, ObjectRef, Row, Value};

/// Characters allowed in XML 1.0 documents.
pub(crate) fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// Turn any text into a valid XML element / attribute name.
pub(crate) fn xml_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        let ok = c.is_alphanumeric() || c == '_' || (i > 0 && (c == '-' || c == '.'));
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        return "column".into();
    }
    let first = out.chars().next().unwrap_or('_');
    if first.is_ascii_digit() || out.to_ascii_lowercase().starts_with("xml") {
        out.insert(0, '_');
    }
    out
}

/// Escape element content. Carriage returns become `&#13;` so XML parsers
/// (which normalise line ends) give back the exact text.
pub(crate) fn push_xml_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c if is_xml_char(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
}

/// Escape an attribute value (whitespace is written as character references
/// because attribute value normalisation would turn it into spaces).
pub(crate) fn push_xml_attr(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if is_xml_char(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
}

pub(crate) struct XmlSink {
    out: TextOutput,
    opts: XmlOptions,
    values: ValueFormat,
    names: Vec<String>,
    root: String,
    line: String,
    rows: u64,
}

impl XmlSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        XmlSink {
            out,
            opts: options.xml.clone(),
            values: options.values.clone(),
            names: Vec::new(),
            root: String::new(),
            line: String::new(),
            rows: 0,
        }
    }
}

impl RowSink for XmlSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        let mut names: Vec<String> = Vec::new();
        for n in unique_names(columns) {
            let mut x = xml_name(&n);
            let base = x.clone();
            let mut i = 2;
            while names.contains(&x) {
                x = format!("{base}_{i}");
                i += 1;
            }
            names.push(x);
        }
        self.names = names;
        let table_name = source_table_name(columns, table).map(|(_, n)| n).unwrap_or_else(|| "export".into());
        let root = self.opts.root_element.replace("${table}", &table_name);
        self.root = xml_name(if root.trim().is_empty() { "export" } else { root.trim() });
        let record = xml_name(&self.opts.record_element);
        self.opts.record_element = record.clone();

        let mut head = format!("<?xml version=\"1.0\" encoding=\"{}\"?>\n", self.out.encoding_name());
        if self.opts.include_dtd {
            head.push_str(&format!("<!DOCTYPE {} [\n  <!ELEMENT {} ({record}*)>\n", self.root, self.root));
            if self.names.is_empty() {
                head.push_str(&format!("  <!ELEMENT {record} EMPTY>\n"));
            } else {
                head.push_str(&format!("  <!ELEMENT {record} ({}?)*>\n", self.names.join("?,")));
            }
            for n in &self.names {
                head.push_str(&format!(
                    "  <!ELEMENT {n} (#PCDATA)>\n  <!ATTLIST {n} null (true|false) #IMPLIED encoding CDATA #IMPLIED>\n"
                ));
            }
            head.push_str("]>\n");
        }
        head.push_str(&format!("<{}>", self.root));
        if self.opts.indent {
            head.push('\n');
        }
        self.out.write_str(&head)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        let (ind1, ind2, nl) = if self.opts.indent { ("  ", "    ", "\n") } else { ("", "", "") };
        let record = &self.opts.record_element;
        self.line.clear();
        self.line.push_str(ind1);
        self.line.push('<');
        self.line.push_str(record);
        self.line.push('>');
        self.line.push_str(nl);
        for (i, name) in self.names.iter().enumerate() {
            let v = cell(row, i);
            if v.is_null() {
                match self.opts.null_mode {
                    XmlNullMode::Omit => continue,
                    XmlNullMode::NullAttribute => {
                        self.line.push_str(&format!("{ind2}<{name} null=\"true\"/>{nl}"));
                    }
                    XmlNullMode::Empty => self.line.push_str(&format!("{ind2}<{name}/>{nl}")),
                    XmlNullMode::Text => {
                        self.line.push_str(&format!("{ind2}<{name}>"));
                        push_xml_text(&mut self.line, &self.values.null_text);
                        self.line.push_str(&format!("</{name}>{nl}"));
                    }
                }
                continue;
            }
            let text = format_value(v, &self.values);
            if text.chars().all(is_xml_char) {
                self.line.push_str(&format!("{ind2}<{name}>"));
                push_xml_text(&mut self.line, &text);
            } else {
                // Not representable in XML 1.0: transport the UTF-8 bytes as base64.
                self.line.push_str(&format!("{ind2}<{name} encoding=\"base64\">"));
                self.line.push_str(&base64::engine::general_purpose::STANDARD.encode(text.as_bytes()));
            }
            self.line.push_str(&format!("</{name}>{nl}"));
        }
        self.line.push_str(&format!("{ind1}</{record}>{nl}"));
        self.out.write_str(&self.line)?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        self.out.write_str(&format!("</{}>\n", self.root))?;
        self.out.finish(self.rows)
    }

    fn abort(&mut self) {
        self.out.abort();
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes()
    }
}

pub(crate) struct DbUnitSink {
    out: TextOutput,
    opts: DbUnitOptions,
    element: String,
    names: Vec<String>,
    line: String,
    rows: u64,
    replaced: u64,
}

impl DbUnitSink {
    pub(crate) fn new(out: TextOutput, options: &ExportOptions) -> Self {
        DbUnitSink {
            out,
            opts: options.dbunit.clone(),
            element: String::new(),
            names: Vec::new(),
            line: String::new(),
            rows: 0,
            replaced: 0,
        }
    }
}

impl RowSink for DbUnitSink {
    fn begin(&mut self, columns: &[ColumnMeta], table: Option<&ObjectRef>) -> DbResult<()> {
        let table_name = match &self.opts.table_name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => source_table_name(columns, table).map(|(_, n)| n).unwrap_or_else(|| "query_result".into()),
        };
        let table_name = if self.opts.upper_case_table_name { table_name.to_uppercase() } else { table_name };
        self.element = xml_name(&table_name);
        let mut names: Vec<String> = Vec::new();
        for n in unique_names(columns) {
            let n = if self.opts.upper_case_column_names { n.to_uppercase() } else { n };
            let mut x = xml_name(&n);
            let base = x.clone();
            let mut i = 2;
            while names.contains(&x) {
                x = format!("{base}_{i}");
                i += 1;
            }
            names.push(x);
        }
        self.names = names;
        let head = format!("<?xml version=\"1.0\" encoding=\"{}\"?>\n<dataset>\n", self.out.encoding_name());
        self.out.write_str(&head)
    }

    fn write_row(&mut self, row: &Row) -> DbResult<()> {
        self.line.clear();
        self.line.push_str("    <");
        self.line.push_str(&self.element);
        for (i, name) in self.names.iter().enumerate() {
            let v = cell(row, i);
            let text = match v {
                Value::Null => match &self.opts.null_value {
                    Some(t) => t.clone(),
                    None => continue,
                },
                Value::Bytes(b) => BinaryEncoding::Base64.encode(b),
                other => other.to_text().into_owned(),
            };
            if !text.chars().all(is_xml_char) {
                self.replaced += 1;
            }
            self.line.push(' ');
            self.line.push_str(name);
            self.line.push_str("=\"");
            push_xml_attr(&mut self.line, &text);
            self.line.push('"');
        }
        self.line.push_str("/>\n");
        self.out.write_str(&self.line)?;
        self.rows += 1;
        Ok(())
    }

    fn finish(&mut self) -> DbResult<ExportSummary> {
        self.out.write_str("</dataset>\n")?;
        let mut summary = self.out.finish(self.rows)?;
        if self.replaced > 0 {
            summary.warnings.push(format!(
                "{} value(s) contained characters not allowed in XML; they were replaced with U+FFFD",
                self.replaced
            ));
        }
        Ok(summary)
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
    fn names() {
        assert_eq!(xml_name("user id"), "user_id");
        assert_eq!(xml_name("1st"), "_1st");
        assert_eq!(xml_name("xmlns"), "_xmlns");
        assert_eq!(xml_name("a-b.c"), "a-b.c");
        assert_eq!(xml_name("-x"), "_x");
        assert_eq!(xml_name(""), "column");
        assert_eq!(xml_name("ünï"), "ünï");
    }

    #[test]
    fn escaping() {
        let mut s = String::new();
        push_xml_text(&mut s, "a<b>&c\r\nd\u{1}");
        assert_eq!(s, "a&lt;b&gt;&amp;c&#13;\nd\u{FFFD}");
        let mut a = String::new();
        push_xml_attr(&mut a, "\"x\"\t\n");
        assert_eq!(a, "&quot;x&quot;&#9;&#10;");
    }
}
