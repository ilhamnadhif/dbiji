//! Copy (plain and advanced) and paste formats (FR-D6).

use crate::format::{ColumnFormat, format_value};
use dbiji_core::dialect::GenericDialect;
use dbiji_core::value::FormatOptions;
use dbiji_core::{DataKind, Dialect, Value, tr};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Output format of an (advanced) copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CopyFormat {
    /// Tab separated values (plain copy).
    #[default]
    Tsv,
    /// Comma (or custom delimiter) separated values.
    Csv,
    /// Markdown table.
    Markdown,
    /// JSON array of objects.
    Json,
    /// `INSERT` statements.
    SqlInsert,
    /// HTML table.
    Html,
    /// XML document.
    Xml,
}

impl CopyFormat {
    /// All formats in menu order.
    pub const ALL: [CopyFormat; 7] = [CopyFormat::Tsv, CopyFormat::Csv, CopyFormat::Markdown, CopyFormat::Json, CopyFormat::SqlInsert, CopyFormat::Html, CopyFormat::Xml];

    /// Translated label.
    pub fn label(self) -> &'static str {
        match self {
            CopyFormat::Tsv => tr("Tab separated (TSV)"),
            CopyFormat::Csv => tr("CSV"),
            CopyFormat::Markdown => tr("Markdown table"),
            CopyFormat::Json => tr("JSON"),
            CopyFormat::SqlInsert => tr("SQL INSERT"),
            CopyFormat::Html => tr("HTML table"),
            CopyFormat::Xml => tr("XML"),
        }
    }

    /// True for delimiter separated formats (delimiter and quoting apply).
    pub fn is_delimited(self) -> bool {
        matches!(self, CopyFormat::Tsv | CopyFormat::Csv)
    }
}

/// When delimited values are quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum QuoteMode {
    /// Only values containing the delimiter, quotes or line breaks.
    #[default]
    Auto,
    /// Every value.
    Always,
    /// Never (values are written verbatim).
    Never,
}

/// Options of the advanced copy dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyOptions {
    /// Output format.
    pub format: CopyFormat,
    /// Include a header line with column names.
    pub include_header: bool,
    /// Include the row number column.
    pub include_row_numbers: bool,
    /// Delimiter for delimited formats; empty uses the format default (tab / comma).
    pub delimiter: String,
    /// Quoting of delimited values.
    pub quote: QuoteMode,
    /// Text written for NULL values.
    pub null_text: String,
    /// Use display formatting (dates, number grouping) instead of raw values.
    pub formatted: bool,
    /// Table name used by [`CopyFormat::SqlInsert`].
    pub sql_table: String,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            format: CopyFormat::Tsv,
            include_header: true,
            include_row_numbers: false,
            delimiter: String::new(),
            quote: QuoteMode::Auto,
            null_text: String::new(),
            formatted: false,
            sql_table: String::new(),
        }
    }
}

impl CopyOptions {
    /// Options of the plain copy (Cmd+C): TSV, no header, raw values.
    pub fn plain() -> Self {
        CopyOptions { include_header: false, ..Default::default() }
    }

    fn effective_delimiter(&self) -> &str {
        if !self.delimiter.is_empty() {
            return &self.delimiter;
        }
        match self.format {
            CopyFormat::Csv => ",",
            _ => "\t",
        }
    }
}

/// Column description for copying.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CopyColumn {
    /// Header label.
    pub name: String,
    /// Logical kind (alignment, quoting in SQL).
    pub kind: DataKind,
    /// Database type name (typed SQL literals).
    pub type_name: String,
    /// Per-column display format.
    pub format: Option<ColumnFormat>,
}

/// One copied row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CopyRow {
    /// 1-based row number shown in the grid.
    pub number: usize,
    /// One entry per [`CopyData::columns`]; `None` for cells outside the selection.
    pub cells: Vec<Option<Value>>,
}

/// A block of cells to copy.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CopyData {
    /// Columns, in display order.
    pub columns: Vec<CopyColumn>,
    /// Rows, in display order.
    pub rows: Vec<CopyRow>,
}

impl CopyData {
    /// Number of cells.
    pub fn cell_count(&self) -> usize {
        self.rows.len() * self.columns.len()
    }
}

/// Render `data`. `dialect` is used for [`CopyFormat::SqlInsert`] (ANSI when `None`).
pub fn render(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions, dialect: Option<&dyn Dialect>) -> String {
    // Plain copy of exactly one cell: the raw text without quoting.
    if opts.format == CopyFormat::Tsv && !opts.include_header && !opts.include_row_numbers && data.rows.len() == 1 && data.columns.len() == 1 {
        return match (data.rows[0].cells.first(), data.columns.first()) {
            (Some(Some(v)), Some(col)) => cell_text(v, col, opts, fmt),
            _ => String::new(),
        };
    }
    match opts.format {
        CopyFormat::Tsv | CopyFormat::Csv => render_delimited(data, opts, fmt),
        CopyFormat::Markdown => render_markdown(data, opts, fmt),
        CopyFormat::Json => render_json(data, opts, fmt),
        CopyFormat::SqlInsert => {
            let generic = GenericDialect::new();
            render_sql(data, opts, fmt, dialect.unwrap_or(&generic))
        }
        CopyFormat::Html => render_html(data, opts, fmt),
        CopyFormat::Xml => render_xml(data, opts, fmt),
    }
}

fn cell_text(v: &Value, col: &CopyColumn, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    if v.is_null() {
        return opts.null_text.clone();
    }
    if opts.formatted {
        // Formatted copy never truncates.
        let mut full = fmt.clone();
        full.max_text_len = usize::MAX;
        full.binary_max_len = usize::MAX;

        match v {
            // `display` shows only the first line of multi-line text;
            // booleans are copied as true / false, not as check boxes.
            Value::Text(_) | Value::Json(_) | Value::Bool(_) => v.to_text().into_owned(),
            _ => format_value(v, &full, col.format.as_ref()).into_owned(),
        }
    } else {
        v.to_text().into_owned()
    }
}

fn needs_quote(s: &str, delimiter: &str) -> bool {
    s.contains(delimiter) || s.contains(['"', '\n', '\r']) || (!s.is_empty() && (s.starts_with(' ') || s.ends_with(' ')))
}

fn quote_field(s: &str, mode: QuoteMode, delimiter: &str) -> String {
    let quote = match mode {
        QuoteMode::Always => true,
        QuoteMode::Never => false,
        QuoteMode::Auto => needs_quote(s, delimiter),
    };
    if quote { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn render_delimited(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    let d = opts.effective_delimiter();
    let mut out = String::new();
    if opts.include_header {
        let mut fields: Vec<String> = Vec::with_capacity(data.columns.len() + 1);
        if opts.include_row_numbers {
            fields.push(quote_field("#", opts.quote, d));
        }
        fields.extend(data.columns.iter().map(|c| quote_field(&c.name, opts.quote, d)));
        out.push_str(&fields.join(d));
        out.push('\n');
    }
    for row in &data.rows {
        let mut fields: Vec<String> = Vec::with_capacity(data.columns.len() + 1);
        if opts.include_row_numbers {
            fields.push(row.number.to_string());
        }
        for (cell, col) in row.cells.iter().zip(&data.columns) {
            fields.push(match cell {
                Some(v) => {
                    let t = cell_text(v, col, opts, fmt);
                    if v.is_null() && opts.quote != QuoteMode::Always { t } else { quote_field(&t, opts.quote, d) }
                }
                None => String::new(),
            });
        }
        out.push_str(&fields.join(d));
        out.push('\n');
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn md_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace('\n', "<br>")
}

fn render_markdown(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    let mut out = String::new();
    let mut header: Vec<String> = Vec::new();
    let mut align: Vec<&str> = Vec::new();
    if opts.include_row_numbers {
        header.push("#".into());
        align.push("---:");
    }
    for c in &data.columns {
        header.push(md_escape(&c.name));
        align.push(if c.kind.is_numeric() { "---:" } else { "---" });
    }
    let _ = writeln!(out, "| {} |", header.join(" | "));
    let _ = writeln!(out, "|{}|", align.iter().map(|a| format!(" {a} ")).collect::<Vec<_>>().join("|"));
    for row in &data.rows {
        let mut fields: Vec<String> = Vec::new();
        if opts.include_row_numbers {
            fields.push(row.number.to_string());
        }
        for (cell, col) in row.cells.iter().zip(&data.columns) {
            fields.push(match cell {
                Some(v) => md_escape(&cell_text(v, col, opts, fmt)),
                None => String::new(),
            });
        }
        let _ = writeln!(out, "| {} |", fields.join(" | "));
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// JSON value of a cell honouring the options.
fn json_value(v: &Value, col: &CopyColumn, opts: &CopyOptions, fmt: &FormatOptions) -> serde_json::Value {
    if v.is_null() {
        return serde_json::Value::Null;
    }
    if opts.formatted && !matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Bool(_) | Value::Json(_)) {
        return serde_json::Value::String(cell_text(v, col, opts, fmt));
    }
    v.to_json()
}

fn render_json(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    let mut arr = Vec::with_capacity(data.rows.len());
    for row in &data.rows {
        let mut obj = serde_json::Map::new();
        if opts.include_row_numbers {
            obj.insert("#".into(), serde_json::Value::from(row.number));
        }
        for (cell, col) in row.cells.iter().zip(&data.columns) {
            if let Some(v) = cell {
                let mut key = col.name.clone();
                // Keep duplicate column names distinct.
                let mut n = 2;
                while obj.contains_key(&key) {
                    key = format!("{}_{n}", col.name);
                    n += 1;
                }
                obj.insert(key, json_value(v, col, opts, fmt));
            }
        }
        arr.push(serde_json::Value::Object(obj));
    }
    serde_json::to_string_pretty(&serde_json::Value::Array(arr)).unwrap_or_default()
}

fn render_sql(data: &CopyData, opts: &CopyOptions, _fmt: &FormatOptions, dialect: &dyn Dialect) -> String {
    let table = if opts.sql_table.trim().is_empty() {
        dialect.quote_ident("table_name")
    } else if opts.sql_table.contains(['.', '"', '`', '[']) {
        // Already qualified / quoted by the caller.
        opts.sql_table.clone()
    } else {
        dialect.quote_ident(opts.sql_table.trim())
    };
    let cols: Vec<String> = data.columns.iter().map(|c| dialect.quote_ident(&c.name)).collect();
    let mut out = String::new();
    for row in &data.rows {
        let vals: Vec<String> = row
            .cells
            .iter()
            .zip(&data.columns)
            .map(|(cell, col)| match cell {
                Some(v) => dialect.literal(v, (!col.type_name.is_empty()).then_some(col.type_name.as_str())),
                None => "NULL".to_string(),
            })
            .collect();
        let _ = writeln!(out, "INSERT INTO {table} ({}) VALUES ({});", cols.join(", "), vals.join(", "));
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Escape text for HTML / XML content and attributes.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters not allowed in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {
                let _ = write!(out, "&#xFFFD;");
            }
            c => out.push(c),
        }
    }
    out
}

fn render_html(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    let mut out = String::from("<table>\n");
    if opts.include_header {
        out.push_str("  <thead>\n    <tr>");
        if opts.include_row_numbers {
            out.push_str("<th>#</th>");
        }
        for c in &data.columns {
            let _ = write!(out, "<th>{}</th>", xml_escape(&c.name));
        }
        out.push_str("</tr>\n  </thead>\n");
    }
    out.push_str("  <tbody>\n");
    for row in &data.rows {
        out.push_str("    <tr>");
        if opts.include_row_numbers {
            let _ = write!(out, "<td>{}</td>", row.number);
        }
        for (cell, col) in row.cells.iter().zip(&data.columns) {
            let align = if col.kind.is_numeric() { " style=\"text-align:right\"" } else { "" };
            match cell {
                Some(v) => {
                    let _ = write!(out, "<td{align}>{}</td>", xml_escape(&cell_text(v, col, opts, fmt)).replace('\n', "<br>"));
                }
                None => {
                    let _ = write!(out, "<td{align}></td>");
                }
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("  </tbody>\n</table>");
    out
}

/// Turn a column name into a valid XML element name.
pub fn xml_name(name: &str) -> String {
    let mut out: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
    if out.is_empty() || !out.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        out.insert(0, '_');
    }
    if out.to_ascii_lowercase().starts_with("xml") {
        out.insert(0, '_');
    }
    out
}

fn render_xml(data: &CopyData, opts: &CopyOptions, fmt: &FormatOptions) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rows>\n");
    let names: Vec<String> = data.columns.iter().map(|c| xml_name(&c.name)).collect();
    for row in &data.rows {
        if opts.include_row_numbers {
            let _ = writeln!(out, "  <row number=\"{}\">", row.number);
        } else {
            out.push_str("  <row>\n");
        }
        for ((cell, col), name) in row.cells.iter().zip(&data.columns).zip(&names) {
            let Some(v) = cell else { continue };
            if v.is_null() {
                let _ = writeln!(out, "    <{name} null=\"true\"/>");
            } else {
                let _ = writeln!(out, "    <{name}>{}</{name}>", xml_escape(&cell_text(v, col, opts, fmt)));
            }
        }
        out.push_str("  </row>\n");
    }
    out.push_str("</rows>");
    out
}

/// Parse clipboard text (TSV as produced by spreadsheets and DBiji) into
/// rows of fields. Quoted fields may contain tabs, quotes (`""`) and line
/// breaks. A trailing line break is ignored.
pub fn parse_clipboard_table(text: &str) -> Vec<Vec<String>> {
    parse_table(text, true)
}

/// Parse DBiji's own copied text: a last line break is a last empty field
/// (a copied NULL or empty text).
pub(crate) fn parse_own_copy(text: &str) -> Vec<Vec<String>> {
    parse_table(text, false)
}

fn parse_table(text: &str, trim_last_break: bool) -> Vec<Vec<String>> {
    let text = text.replace("\r\n", "\n");
    let text = if trim_last_break { text.strip_suffix('\n').unwrap_or(&text) } else { &text };
    if text.is_empty() {
        return Vec::new();
    }
    let delimiter = if text.contains('\t') { '\t' } else { '\u{0}' };
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut at_field_start = true;
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_field_start => {
                in_quotes = true;
                at_field_start = false;
            }
            c if c == delimiter => {
                row.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_field_start = true;
            }
            c => {
                field.push(c);
                at_field_start = false;
            }
        }
    }
    row.push(field);
    rows.push(row);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> CopyData {
        CopyData {
            columns: vec![
                CopyColumn { name: "id".into(), kind: DataKind::Int, type_name: "int4".into(), format: None },
                CopyColumn { name: "name".into(), kind: DataKind::Text, type_name: "text".into(), format: None },
                CopyColumn { name: "when".into(), kind: DataKind::Date, type_name: "date".into(), format: None },
            ],
            rows: vec![
                CopyRow { number: 1, cells: vec![Some(Value::Int(1)), Some(Value::text("a\tb")), Some(Value::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()))] },
                CopyRow { number: 2, cells: vec![Some(Value::Int(1234)), Some(Value::Null), None] },
            ],
        }
    }

    fn r(opts: &CopyOptions) -> String {
        render(&data(), opts, &FormatOptions::default(), None)
    }

    #[test]
    fn plain_tsv_and_single_cell() {
        assert_eq!(r(&CopyOptions::plain()), "1\t\"a\tb\"\t2024-01-02\n1234\t\t");
        let single = CopyData {
            columns: vec![CopyColumn { name: "x".into(), ..Default::default() }],
            rows: vec![CopyRow { number: 1, cells: vec![Some(Value::text("line1\nline2 \"q\""))] }],
        };
        assert_eq!(render(&single, &CopyOptions::plain(), &FormatOptions::default(), None), "line1\nline2 \"q\"");
    }

    #[test]
    fn csv_options() {
        let o = CopyOptions { format: CopyFormat::Csv, include_row_numbers: true, null_text: "NULL".into(), ..Default::default() };
        assert_eq!(r(&o), "#,id,name,when\n1,1,a\tb,2024-01-02\n2,1234,NULL,");
        let o = CopyOptions { format: CopyFormat::Csv, quote: QuoteMode::Always, include_header: false, ..Default::default() };
        assert_eq!(r(&o), "\"1\",\"a\tb\",\"2024-01-02\"\n\"1234\",\"\",");
        let o = CopyOptions { format: CopyFormat::Csv, delimiter: ";".into(), include_header: false, ..Default::default() };
        assert_eq!(r(&o), "1;a\tb;2024-01-02\n1234;;");
        let fmt = FormatOptions { thousands_separator: true, date_format: "%d/%m/%Y".into(), ..Default::default() };
        let o = CopyOptions { format: CopyFormat::Csv, formatted: true, include_header: false, ..Default::default() };
        assert_eq!(render(&data(), &o, &fmt, None), "1,a\tb,02/01/2024\n\"1,234\",,");
    }

    #[test]
    fn markdown_json_sql_html_xml() {
        let md = r(&CopyOptions { format: CopyFormat::Markdown, ..Default::default() });
        assert_eq!(md, "| id | name | when |\n| ---: | --- | --- |\n| 1 | a\tb | 2024-01-02 |\n| 1234 |  |  |");
        let json = r(&CopyOptions { format: CopyFormat::Json, ..Default::default() });
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["name"], "a\tb");
        assert_eq!(parsed[0]["id"], 1);
        assert_eq!(parsed[1]["name"], serde_json::Value::Null);
        assert!(parsed[1].get("when").is_none());
        let sql = r(&CopyOptions { format: CopyFormat::SqlInsert, sql_table: "people".into(), ..Default::default() });
        assert_eq!(
            sql,
            "INSERT INTO people (id, name, \"when\") VALUES (1, 'a\tb', '2024-01-02');\n\
             INSERT INTO people (id, name, \"when\") VALUES (1234, NULL, NULL);"
        );
        let html = r(&CopyOptions { format: CopyFormat::Html, ..Default::default() });
        assert!(html.contains("<th>name</th>"));
        assert!(html.contains("<td style=\"text-align:right\">1234</td>"));
        let xml = r(&CopyOptions { format: CopyFormat::Xml, include_row_numbers: true, ..Default::default() });
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<row number=\"2\">"));
        assert!(xml.contains("<name null=\"true\"/>"));
        assert!(xml.contains("<when>2024-01-02</when>"));
        assert_eq!(xml_name("1st col"), "_1st_col");
        assert_eq!(xml_name("xmlish"), "_xmlish");
        assert_eq!(xml_escape("<a & 'b'>"), "&lt;a &amp; &apos;b&apos;&gt;");
    }

    #[test]
    fn clipboard_parsing() {
        assert_eq!(parse_clipboard_table("a\tb\nc\td\n"), vec![vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(parse_clipboard_table("a\r\nb"), vec![vec!["a"], vec!["b"]]);
        assert_eq!(parse_clipboard_table("\"x\ty\"\t\"multi\nline\"\t\"q\"\"q\"\nz\t\t"), vec![vec!["x\ty", "multi\nline", "q\"q"], vec!["z", "", ""]]);
        assert_eq!(parse_clipboard_table("single value"), vec![vec!["single value"]]);
        assert!(parse_clipboard_table("").is_empty());
        // Round trip with the plain copy format.
        let copied = r(&CopyOptions::plain());
        let parsed = parse_clipboard_table(&copied);
        assert_eq!(parsed[0], vec!["1", "a\tb", "2024-01-02"]);
        assert_eq!(parsed[1], vec!["1234", "", ""]);
    }
}
