//! Export formats and their options. Every type is serialisable and has a
//! sensible `Default` so export configurations can be stored as tasks.

use crate::format::{BinaryEncoding, ValueFormat};
use dbiji_core::DbKind;
use serde::{Deserialize, Serialize};

/// Output format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// Comma separated values.
    #[default]
    Csv,
    /// Tab separated values (CSV options with a tab delimiter).
    Tsv,
    /// JSON document (array of objects or columns + rows).
    Json,
    /// One JSON object per line.
    JsonLines,
    /// SQL `INSERT` / upsert statements.
    Sql,
    /// Excel workbook.
    Xlsx,
    /// Markdown (GitHub flavoured) table.
    Markdown,
    /// HTML page with a table.
    Html,
    /// DBeaver style XML (`<export><DATA_RECORD>...`).
    Xml,
    /// Fixed width plain text table.
    Txt,
    /// DbUnit flat XML data set.
    DbUnit,
    /// PHP source code (array of associative arrays).
    Php,
    /// Apache Parquet (columnar, snappy compressed).
    Parquet,
}

impl ExportFormat {
    /// Every format, in the order shown in the export wizard.
    pub const ALL: [ExportFormat; 13] = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::JsonLines,
        ExportFormat::Sql,
        ExportFormat::Xlsx,
        ExportFormat::Markdown,
        ExportFormat::Html,
        ExportFormat::Xml,
        ExportFormat::Txt,
        ExportFormat::DbUnit,
        ExportFormat::Php,
        ExportFormat::Parquet,
    ];

    /// File name extension without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::JsonLines => "jsonl",
            ExportFormat::Sql => "sql",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Xml => "xml",
            ExportFormat::Txt => "txt",
            ExportFormat::DbUnit => "xml",
            ExportFormat::Php => "php",
            ExportFormat::Parquet => "parquet",
        }
    }

    /// English label (route through `dbiji_core::tr` in the UI).
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::JsonLines => "JSON Lines",
            ExportFormat::Sql => "SQL INSERT statements",
            ExportFormat::Xlsx => "Excel (XLSX)",
            ExportFormat::Markdown => "Markdown table",
            ExportFormat::Html => "HTML",
            ExportFormat::Xml => "XML",
            ExportFormat::Txt => "Plain text (TXT)",
            ExportFormat::DbUnit => "DbUnit data set",
            ExportFormat::Php => "PHP source code",
            ExportFormat::Parquet => "Parquet",
        }
    }

    /// Binary formats ignore the text encoding options.
    pub fn is_binary(self) -> bool {
        matches!(self, ExportFormat::Xlsx | ExportFormat::Parquet)
    }

    /// Format for a file name extension (`csv`, `.xlsx`, `jsonl` ...).
    pub fn from_extension(ext: &str) -> Option<ExportFormat> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" => ExportFormat::Csv,
            "tsv" | "tab" => ExportFormat::Tsv,
            "json" => ExportFormat::Json,
            "jsonl" | "ndjson" => ExportFormat::JsonLines,
            "sql" => ExportFormat::Sql,
            "xlsx" => ExportFormat::Xlsx,
            "md" | "markdown" => ExportFormat::Markdown,
            "html" | "htm" => ExportFormat::Html,
            "xml" => ExportFormat::Xml,
            "txt" => ExportFormat::Txt,
            "php" => ExportFormat::Php,
            "parquet" => ExportFormat::Parquet,
            _ => return None,
        })
    }
}

/// Options of an export; only the section matching the chosen format is used,
/// plus the common output settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportOptions {
    /// Character encoding of text formats (`UTF-8`, `UTF-16LE`, `windows-1252` ...).
    pub encoding: String,
    /// Write a byte order mark (UTF-8 and UTF-16 only).
    pub insert_bom: bool,
    /// Rows fetched from the source per batch.
    pub fetch_size: usize,
    /// Keep the partially written file when an export fails or is cancelled
    /// (by default it is deleted).
    pub keep_partial_file: bool,
    /// Value formatting for text formats.
    pub values: ValueFormat,
    /// CSV / TSV settings.
    pub csv: CsvOptions,
    /// JSON / JSON Lines settings.
    pub json: JsonOptions,
    /// SQL settings.
    pub sql: SqlOptions,
    /// XLSX settings.
    pub xlsx: XlsxOptions,
    /// Markdown settings.
    pub markdown: MarkdownOptions,
    /// HTML settings.
    pub html: HtmlOptions,
    /// XML settings.
    pub xml: XmlOptions,
    /// Plain text table settings.
    pub txt: TxtOptions,
    /// DbUnit settings.
    pub dbunit: DbUnitOptions,
    /// PHP settings.
    pub php: PhpOptions,
    /// Parquet settings.
    pub parquet: ParquetOptions,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            encoding: "UTF-8".into(),
            insert_bom: false,
            fetch_size: 1000,
            keep_partial_file: false,
            values: ValueFormat::default(),
            csv: CsvOptions::default(),
            json: JsonOptions::default(),
            sql: SqlOptions::default(),
            xlsx: XlsxOptions::default(),
            markdown: MarkdownOptions::default(),
            html: HtmlOptions::default(),
            xml: XmlOptions::default(),
            txt: TxtOptions::default(),
            dbunit: DbUnitOptions::default(),
            php: PhpOptions::default(),
            parquet: ParquetOptions::default(),
        }
    }
}

/// When CSV fields are enclosed in quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuoteMode {
    /// Only fields that need it (delimiter, quote, line break, leading or
    /// trailing space, empty text vs NULL).
    #[default]
    Needed,
    /// Every non-NULL field.
    Always,
    /// Never (output may be ambiguous).
    Never,
    /// Every value that is not a number or boolean, plus fields that need it.
    Strings,
}

/// How a quote character inside a quoted field is escaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EscapeStyle {
    /// `"` becomes `""` (RFC 4180).
    #[default]
    Doubled,
    /// `"` becomes `\"` and `\` becomes `\\`.
    Backslash,
}

/// Content of the CSV header row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeaderStyle {
    /// Column label.
    #[default]
    Label,
    /// Database type name.
    Type,
    /// `label (type)`.
    Both,
}

/// Line terminator of text output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    /// `\n`
    #[default]
    Lf,
    /// `\r\n`
    CrLf,
    /// `\r`
    Cr,
}

impl LineEnding {
    /// The terminator text.
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }
}

/// CSV and TSV export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvOptions {
    /// Field delimiter; may be longer than one character. TSV always uses a tab.
    pub delimiter: String,
    /// Quote character.
    pub quote_char: char,
    /// When fields are quoted.
    pub quote_mode: QuoteMode,
    /// How quotes inside quoted fields are escaped.
    pub escape: EscapeStyle,
    /// Write a header row.
    pub header: bool,
    /// Content of the header row.
    pub header_style: HeaderStyle,
    /// Text written (unquoted) for NULL. With the default empty string an
    /// empty text value is written as `""` so both stay distinguishable.
    pub null_string: String,
    /// Line terminator.
    pub line_ending: LineEnding,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: ",".into(),
            quote_char: '"',
            quote_mode: QuoteMode::Needed,
            escape: EscapeStyle::Doubled,
            header: true,
            header_style: HeaderStyle::Label,
            null_string: String::new(),
            line_ending: LineEnding::Lf,
        }
    }
}

/// Structure of a JSON export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JsonLayout {
    /// `[{"col": value, ...}, ...]`
    #[default]
    ArrayOfObjects,
    /// `{"columns": [{"name": .., "type": ..}], "rows": [[...], ...]}`
    ColumnsAndRows,
}

/// JSON / JSON Lines options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JsonOptions {
    /// Indent the output (ignored by JSON Lines).
    pub pretty: bool,
    /// Document structure (JSON only).
    pub layout: JsonLayout,
    /// Wrap the rows in an object keyed by the table name (DBeaver style) or add a `table` member.
    pub include_table_name: bool,
    /// Rendering of binary values.
    pub binary: BinaryEncoding,
    /// Render dates and times with [`ExportOptions::values`] patterns instead of ISO text.
    pub format_dates: bool,
}

impl Default for JsonOptions {
    fn default() -> Self {
        JsonOptions {
            pretty: true,
            layout: JsonLayout::ArrayOfObjects,
            include_table_name: false,
            binary: BinaryEncoding::Base64,
            format_dates: false,
        }
    }
}

/// Kind of statements produced by the SQL export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SqlInsertMode {
    /// Plain `INSERT` (multi-row `VALUES`).
    #[default]
    Insert,
    /// Insert-or-update via [`Dialect::upsert`](dbiji_core::Dialect::upsert);
    /// dialects without upsert get `DELETE` + `INSERT` pairs.
    Upsert,
}

/// When identifiers are quoted in generated SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierQuoting {
    /// Only reserved words, special characters and case sensitive names.
    #[default]
    AsNeeded,
    /// Every identifier.
    Always,
}

/// SQL export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlOptions {
    /// Target table name; defaults to the source table (or `query_result`).
    pub table_name: Option<String>,
    /// Target schema; defaults to the source schema.
    pub schema_name: Option<String>,
    /// Qualify the table name with its schema.
    pub qualify_with_schema: bool,
    /// Rows per `INSERT` statement (multi-row `VALUES`).
    pub rows_per_statement: usize,
    /// Insert or upsert.
    pub mode: SqlInsertMode,
    /// Key columns for upsert; defaults to the primary key of the source table.
    pub key_columns: Vec<String>,
    /// Emit `CREATE TABLE` before the data.
    pub include_create_table: bool,
    /// Identifier quoting.
    pub quoting: IdentifierQuoting,
    /// Statement terminator.
    pub terminator: String,
    /// Product of the source database. When it matches the target dialect,
    /// source type names are passed to [`Dialect::literal`](dbiji_core::Dialect::literal)
    /// as cast hints and kept in `CREATE TABLE`.
    pub source_kind: Option<DbKind>,
}

impl Default for SqlOptions {
    fn default() -> Self {
        SqlOptions {
            table_name: None,
            schema_name: None,
            qualify_with_schema: true,
            rows_per_statement: 10,
            mode: SqlInsertMode::Insert,
            key_columns: Vec::new(),
            include_create_table: false,
            quoting: IdentifierQuoting::AsNeeded,
            terminator: ";".into(),
            source_kind: None,
        }
    }
}

/// XLSX export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XlsxOptions {
    /// Sheet name; defaults to the table name or `Sheet1`.
    pub sheet_name: Option<String>,
    /// Write a header row.
    pub header: bool,
    /// Bold header.
    pub bold_header: bool,
    /// Freeze the header row.
    pub freeze_header: bool,
    /// Add an autofilter over the data.
    pub autofilter: bool,
    /// Size columns to their content (up to `max_column_width`).
    pub auto_column_width: bool,
    /// Maximum automatic column width in characters.
    pub max_column_width: u16,
    /// Excel number format of dates.
    pub date_format: String,
    /// Excel number format of times.
    pub time_format: String,
    /// Excel number format of timestamps.
    pub datetime_format: String,
    /// Excel number format of fractional numbers (`None` = General).
    pub number_format: Option<String>,
    /// Excel number format of integers (`None` = General).
    pub integer_format: Option<String>,
    /// Data rows per sheet before a new sheet is started (Excel's limit is
    /// 1,048,576 rows including the header).
    pub max_rows_per_sheet: u32,
}

impl Default for XlsxOptions {
    fn default() -> Self {
        XlsxOptions {
            sheet_name: None,
            header: true,
            bold_header: true,
            freeze_header: true,
            autofilter: true,
            auto_column_width: true,
            max_column_width: 60,
            date_format: "yyyy-mm-dd".into(),
            time_format: "hh:mm:ss".into(),
            datetime_format: "yyyy-mm-dd hh:mm:ss".into(),
            number_format: None,
            integer_format: None,
            max_rows_per_sheet: 1_048_575,
        }
    }
}

/// Markdown table options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownOptions {
    /// Right align numeric columns (`---:`).
    pub right_align_numbers: bool,
    /// Escape `|` inside cells as `\|`.
    pub escape_pipes: bool,
    /// Replacement for line breaks inside cells.
    pub line_break: String,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        MarkdownOptions { right_align_numbers: true, escape_pipes: true, line_break: "<br>".into() }
    }
}

/// HTML export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HtmlOptions {
    /// Page title and table caption; defaults to the table name.
    pub title: Option<String>,
    /// Embed a small style sheet.
    pub embed_css: bool,
    /// Write the header row.
    pub header: bool,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        HtmlOptions { title: None, embed_css: true, header: true }
    }
}

/// How NULL is written in the XML export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum XmlNullMode {
    /// `<col null="true"/>` (read back as NULL).
    #[default]
    NullAttribute,
    /// The element is omitted.
    Omit,
    /// `<col/>` (indistinguishable from an empty string).
    Empty,
    /// The [`ValueFormat::null_text`] as element text.
    Text,
}

/// XML export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XmlOptions {
    /// Root element; `${table}` is replaced by the table name.
    pub root_element: String,
    /// Element of every row.
    pub record_element: String,
    /// NULL notation.
    pub null_mode: XmlNullMode,
    /// Include an inline DTD describing the document.
    pub include_dtd: bool,
    /// Indent elements.
    pub indent: bool,
}

impl Default for XmlOptions {
    fn default() -> Self {
        XmlOptions {
            root_element: "export".into(),
            record_element: "DATA_RECORD".into(),
            null_mode: XmlNullMode::NullAttribute,
            include_dtd: false,
            indent: true,
        }
    }
}

/// Fixed width text table options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TxtOptions {
    /// Draw `+---+` borders and `|` separators.
    pub borders: bool,
    /// Write the header row.
    pub header: bool,
    /// Maximum column width; longer values are truncated with `…`.
    pub max_column_width: usize,
    /// Minimum column width.
    pub min_column_width: usize,
    /// Show NULL as `null_text`; otherwise leave the cell blank.
    pub show_nulls: bool,
    /// Text for NULL when `show_nulls` is set.
    pub null_text: String,
    /// Right align numbers.
    pub right_align_numbers: bool,
    /// Rows buffered to compute column widths before streaming.
    pub width_sample_rows: usize,
}

impl Default for TxtOptions {
    fn default() -> Self {
        TxtOptions {
            borders: true,
            header: true,
            max_column_width: 64,
            min_column_width: 1,
            show_nulls: true,
            null_text: "[NULL]".into(),
            right_align_numbers: true,
            width_sample_rows: 1000,
        }
    }
}

/// DbUnit flat XML options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DbUnitOptions {
    /// Table element name; defaults to the table name.
    pub table_name: Option<String>,
    /// Upper case the table name.
    pub upper_case_table_name: bool,
    /// Upper case column names.
    pub upper_case_column_names: bool,
    /// Text written for NULL; `None` omits the attribute (DbUnit's NULL notation).
    pub null_value: Option<String>,
}

/// PHP source export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PhpOptions {
    /// Variable name without `$`; defaults to the table name or `rows`.
    pub variable_name: Option<String>,
    /// Use `[...]` instead of `array(...)`.
    pub short_array_syntax: bool,
    /// Start with `<?php`.
    pub open_tag: bool,
}

impl Default for PhpOptions {
    fn default() -> Self {
        PhpOptions { variable_name: None, short_array_syntax: false, open_tag: true }
    }
}

/// Parquet compression codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ParquetCompression {
    /// Snappy (fast, widely supported).
    #[default]
    Snappy,
    /// No compression.
    None,
}

/// Parquet export options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParquetOptions {
    /// Compression codec.
    pub compression: ParquetCompression,
    /// Maximum rows per row group.
    pub row_group_rows: usize,
    /// Approximate maximum buffered bytes per row group.
    pub row_group_bytes: usize,
}

impl Default for ParquetOptions {
    fn default() -> Self {
        ParquetOptions {
            compression: ParquetCompression::Snappy,
            row_group_rows: 65_536,
            row_group_bytes: 64 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_and_defaults() {
        let mut o = ExportOptions::default();
        o.csv.delimiter = ";".into();
        o.sql.mode = SqlInsertMode::Upsert;
        o.sql.source_kind = Some(DbKind::Postgres);
        o.values.date_format = Some("%d/%m/%Y".into());
        let s = serde_json::to_string(&o).unwrap();
        let back: ExportOptions = serde_json::from_str(&s).unwrap();
        assert_eq!(back, o);
        let partial: ExportOptions = serde_json::from_str(r#"{"csv":{"delimiter":"|"}}"#).unwrap();
        assert_eq!(partial.csv.delimiter, "|");
        assert_eq!(partial.csv.quote_char, '"');
        assert_eq!(partial.fetch_size, 1000);
        assert_eq!(serde_json::to_string(&ExportFormat::JsonLines).unwrap(), "\"json_lines\"");
    }

    #[test]
    fn extensions() {
        for f in ExportFormat::ALL {
            assert!(!f.extension().is_empty());
            assert!(!f.label().is_empty());
        }
        assert_eq!(ExportFormat::from_extension(".XLSX"), Some(ExportFormat::Xlsx));
        assert_eq!(ExportFormat::from_extension("nope"), None);
    }
}
