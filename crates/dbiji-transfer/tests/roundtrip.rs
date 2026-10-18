//! PRD §13 gate: "export then import CSV, JSON and SQL produce identical data".
//!
//! Rows covering every `Value` variant and hostile text are exported, read
//! back with the crate's readers (or executed as SQL in SQLite), converted to
//! the known column kinds and compared value by value.

mod common;

use common::*;
use dbiji_core::{ColumnMeta, Row, Value};
use dbiji_transfer::*;

async fn csv_round_trip(opts: ExportOptions, read: CsvReadOptions, what: &str) {
    let cols = nasty_columns();
    let rows = nasty_rows();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let summary = export_to(ExportFormat::Csv, &opts, &path, cols.clone(), rows.clone(), None).await;
    assert_eq!(summary.rows, rows.len() as u64);
    let mut src = CsvSource::open(&path, read).unwrap();
    let names: Vec<&str> = src.columns().iter().map(|c| c.name.as_str()).collect();
    let expected: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, expected, "{what}: header");
    let raw = drain(&mut src).await;
    assert!(src.take_warnings().is_empty(), "{what}: unexpected warnings");
    let back = convert_rows(raw, &cols, &ConvertOptions::default());
    assert_same_rows(&rows, &back, &cols, what);
}

#[tokio::test]
async fn csv_default_options() {
    csv_round_trip(ExportOptions::default(), CsvReadOptions::default(), "csv default").await;
}

#[tokio::test]
async fn csv_custom_dialects() {
    // Semicolon, backslash escapes, CRLF, quote everything.
    let mut o = ExportOptions::default();
    o.csv.delimiter = ";".into();
    o.csv.escape = EscapeStyle::Backslash;
    o.csv.line_ending = LineEnding::CrLf;
    o.csv.quote_mode = QuoteMode::Always;
    let r = CsvReadOptions { delimiter: ";".into(), escape: EscapeStyle::Backslash, ..Default::default() };
    csv_round_trip(o, r, "csv semicolon/backslash/always").await;

    // Multi character delimiter, explicit NULL marker, strings-only quoting.
    let mut o = ExportOptions::default();
    o.csv.delimiter = "||".into();
    o.csv.null_string = "\\N".into();
    o.csv.quote_mode = QuoteMode::Strings;
    let r = CsvReadOptions { delimiter: "||".into(), null_string: "\\N".into(), ..Default::default() };
    csv_round_trip(o, r, "csv multi-char delimiter / \\N").await;

    // Single quote as quote character, CR line endings.
    let mut o = ExportOptions::default();
    o.csv.quote_char = '\'';
    o.csv.line_ending = LineEnding::Cr;
    let r = CsvReadOptions { quote_char: Some('\''), ..Default::default() };
    csv_round_trip(o, r, "csv single quotes / CR").await;
}

#[tokio::test]
async fn csv_encodings_and_bom() {
    for (enc, bom) in [("UTF-8", true), ("UTF-16LE", true), ("UTF-16BE", true), ("UTF-16LE", false)] {
        let mut o = ExportOptions { encoding: enc.into(), insert_bom: bom, ..Default::default() };
        o.csv.header = true;
        let r = CsvReadOptions { encoding: if bom { "auto".into() } else { enc.into() }, ..Default::default() };
        csv_round_trip(o, r, &format!("csv {enc} bom={bom}")).await;
    }
}

#[tokio::test]
async fn tsv_round_trip() {
    let cols = nasty_columns();
    let rows = nasty_rows();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.tsv");
    export_to(ExportFormat::Tsv, &ExportOptions::default(), &path, cols.clone(), rows.clone(), None).await;
    let mut src = CsvSource::open(&path, CsvReadOptions::tsv()).unwrap();
    let back = convert_rows(drain(&mut src).await, &cols, &ConvertOptions::default());
    assert_same_rows(&rows, &back, &cols, "tsv");
}

async fn json_round_trip(format: ExportFormat, opts: ExportOptions, what: &str) {
    let cols = nasty_columns();
    let rows = nasty_rows();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.json");
    export_to(format, &opts, &path, cols.clone(), rows.clone(), None).await;
    let bytes = std::fs::read(&path).unwrap();
    let text = if opts.encoding.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).unwrap()
    } else {
        let (text, _, had_errors) = encoding_rs::Encoding::for_label(opts.encoding.as_bytes()).unwrap().decode(&bytes);
        assert!(!had_errors);
        text.into_owned()
    };
    // The output is valid JSON (per line for JSON Lines).
    if format == ExportFormat::JsonLines {
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| panic!("{what}: {e}: {line}"));
        }
    } else {
        serde_json::from_str::<serde_json::Value>(&text).unwrap_or_else(|e| panic!("{what}: {e}"));
    }
    let mut src = JsonSource::open(&path, JsonReadOptions::default()).unwrap();
    let names: Vec<String> = src.columns().iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), "{what}: columns");
    let raw = drain(&mut src).await;
    let convert = ConvertOptions { binary: Some(BinaryEncoding::Base64), ..Default::default() };
    let back = convert_rows(raw, &cols, &convert);
    assert_same_rows(&rows, &back, &cols, what);
}

#[tokio::test]
async fn json_layouts() {
    json_round_trip(ExportFormat::Json, ExportOptions::default(), "json pretty array").await;
    let mut compact = ExportOptions::default();
    compact.json.pretty = false;
    json_round_trip(ExportFormat::Json, compact, "json compact array").await;
    let mut wrapped = ExportOptions::default();
    wrapped.json.include_table_name = true;
    json_round_trip(ExportFormat::Json, wrapped, "json table wrapper").await;
    let mut layout = ExportOptions::default();
    layout.json.layout = JsonLayout::ColumnsAndRows;
    layout.json.include_table_name = true;
    json_round_trip(ExportFormat::Json, layout.clone(), "json columns+rows pretty").await;
    layout.json.pretty = false;
    json_round_trip(ExportFormat::Json, layout, "json columns+rows compact").await;
    json_round_trip(ExportFormat::JsonLines, ExportOptions::default(), "json lines").await;
    let utf16 = ExportOptions { encoding: "UTF-16LE".into(), insert_bom: true, ..Default::default() };
    json_round_trip(ExportFormat::JsonLines, utf16, "json lines utf-16").await;
}

/// SQLite table able to hold the data set without type affinity conversions.
const SQLITE_TABLE: &str = "CREATE TABLE t (id INTEGER, flag INTEGER, big TEXT, ratio REAL, amount TEXT, note TEXT, code TEXT, \
     data BLOB, day TEXT, at TEXT, ts TEXT, tstz TEXT, uid TEXT, doc TEXT, arr TEXT)";

async fn sql_round_trip(rows: Vec<Row>, dialect: std::sync::Arc<dyn dbiji_core::Dialect>, rows_per_statement: usize, what: &str) {
    let cols = nasty_columns();
    let mut opts = ExportOptions::default();
    opts.sql.table_name = Some("t".into());
    opts.sql.rows_per_statement = rows_per_statement;
    let script = export_string(ExportFormat::Sql, &opts, cols.clone(), rows.clone(), None, Some(dialect)).await;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(SQLITE_TABLE).unwrap();
    conn.execute_batch(&script).unwrap_or_else(|e| panic!("{what}: {e}\n{script}"));
    let back = select(&conn, "SELECT * FROM t ORDER BY id");
    let back = convert_rows(back, &cols, &ConvertOptions::default());
    assert_same_rows(&rows, &back, &cols, what);
}

/// SQLite parses unquoted numeric literals as 64-bit floats, so with plain
/// ANSI literals the decimals and huge numbers of the set are limited to
/// values SQLite can hold exactly.
fn sqlite_safe(rows: Vec<Row>) -> Vec<Row> {
    rows.into_iter()
        .map(|mut r| {
            if matches!(r[2], Value::UInt(_)) {
                r[2] = Value::Int(4_242);
            }
            if let Value::Float(f) = r[3]
                && f.abs() > 1e20 {
                    r[3] = Value::Float(1.5e10);
                }
            if let Value::Decimal(d) = &r[4] {
                let safe = match d.as_ref() {
                    "12345678901234567890.123456789" => "12.5",
                    "1.50" => "-0.25",
                    "-98765432109876543210" => "-987654321",
                    "-0.000001" => "0.5",
                    other if other.ends_with('0') && other.contains('.') => "7.25",
                    other => other,
                };
                r[4] = Value::decimal(safe.to_string());
            }
            r
        })
        .collect()
}

#[tokio::test]
async fn sql_generic_dialect_literals() {
    for per in [1, 4, 100] {
        sql_round_trip(sqlite_safe(nasty_rows()), generic(), per, &format!("sql generic, {per} rows/statement")).await;
    }
}

#[tokio::test]
async fn sql_full_data_set_with_text_decimals() {
    // Same ANSI literals, but decimals / u64 values as text literals like a
    // SQLite dialect must render them; floats SQLite cannot parse exactly are replaced.
    let rows: Vec<Row> = nasty_rows()
        .into_iter()
        .map(|mut r| {
            if let Value::Float(f) = r[3]
                && f.abs() > 1e20 {
                    r[3] = Value::Float(-1e15);
                }
            r
        })
        .collect();
    sql_round_trip(rows, sqlite_dialect(), 10, "sql full set").await;
}

#[tokio::test]
async fn sql_upsert_round_trip() {
    let cols = nasty_columns();
    let rows = nasty_rows();
    let mut opts = ExportOptions::default();
    opts.sql.table_name = Some("t".into());
    opts.sql.mode = SqlInsertMode::Upsert;
    opts.sql.key_columns = vec!["id".into()];
    // Native upsert (SQLite ON CONFLICT) and the DELETE + INSERT fallback (ANSI).
    for (dialect, what) in [(sqlite_dialect(), "native upsert"), (generic(), "delete+insert")] {
        let script = export_string(ExportFormat::Sql, &opts, cols.clone(), rows.clone(), None, Some(dialect.clone())).await;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&SQLITE_TABLE.replace("id INTEGER", "id INTEGER PRIMARY KEY")).unwrap();
        // Running the script twice must not duplicate rows.
        conn.execute_batch(&script).unwrap_or_else(|e| panic!("{what}: {e}"));
        conn.execute_batch(&script).unwrap_or_else(|e| panic!("{what} (second run): {e}"));
        let back = select(&conn, "SELECT * FROM t ORDER BY id");
        if what == "native upsert" {
            let back = convert_rows(back, &cols, &ConvertOptions::default());
            assert_same_rows(&rows, &back, &cols, what);
        } else {
            // ANSI literals: SQLite mangles the big numbers (see `sqlite_safe`); the keys must be unique.
            assert_eq!(back.len(), rows.len(), "{what}");
            assert!(script.contains("DELETE FROM t WHERE id = 1;"), "{what}: {script}");
        }
    }
}

#[tokio::test]
async fn csv_through_import_into_sqlite() {
    // Full pipeline: CSV export → CsvSource → run_import (mapping with known
    // kinds) → SQLite → read back. SQLite stores NaN as NULL, so NaN is
    // replaced for this test.
    let cols = nasty_columns();
    let rows: Vec<Row> = nasty_rows()
        .into_iter()
        .map(|mut r| {
            if matches!(r[3], Value::Float(f) if f.is_nan()) {
                r[3] = Value::Float(42.0);
            }
            r
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.csv");
    export_to(ExportFormat::Csv, &ExportOptions::default(), &path, cols.clone(), rows.clone(), None).await;
    let mut session = SqliteSession::memory(sqlite_dialect());
    session.exec_batch(SQLITE_TABLE);
    let mut src = CsvSource::open(&path, CsvReadOptions::default()).unwrap();
    let mut mapping = ImportMapping::new(dbiji_core::ObjectRef::table(None, "t"));
    mapping.columns = cols
        .iter()
        .enumerate()
        .map(|(i, c)| ColumnMapping { target_kind: Some(c.kind), ..ColumnMapping::new(i, c.name.clone()) })
        .collect();
    mapping.batch_size = 4;
    mapping.rows_per_statement = 3;
    let summary = run_import(&mut src, &mut session, &mapping, &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(summary.rows_written, rows.len() as u64);
    assert_eq!(summary.rows_failed, 0);
    let back = convert_rows(select(&session.conn, "SELECT * FROM t ORDER BY id"), &cols, &ConvertOptions::default());
    assert_same_rows(&rows, &back, &cols, "csv → import → sqlite");
}

#[tokio::test]
async fn xml_round_trip_text() {
    // XML is text based: every value comes back as its canonical text.
    let cols = nasty_columns();
    let rows = nasty_rows();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.xml");
    let mut opts = ExportOptions::default();
    opts.xml.include_dtd = true;
    export_to(ExportFormat::Xml, &opts, &path, cols.clone(), rows.clone(), None).await;
    let mut src = XmlSource::open(&path, XmlReadOptions::default()).unwrap();
    let names: Vec<String> = src.columns().iter().map(|c| c.name.clone()).collect();
    // The NULL-only last row does not change the set of columns.
    assert_eq!(names, cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    let back = convert_rows(drain(&mut src).await, &cols, &ConvertOptions::default());
    assert_same_rows(&rows, &back, &cols, "xml");
}

#[test]
fn data_set_covers_every_variant() {
    let rows = nasty_rows();
    let mut seen = std::collections::BTreeSet::new();
    fn walk(v: &Value, seen: &mut std::collections::BTreeSet<&'static str>) {
        seen.insert(v.variant_name());
        if let Value::Array(items) = v {
            for i in items.iter() {
                walk(i, seen);
            }
        }
    }
    for r in &rows {
        for v in r {
            walk(v, &mut seen);
        }
    }
    for name in [
        "null", "bool", "int", "uint", "float", "decimal", "text", "bytes", "date", "time", "datetime", "datetimetz", "uuid",
        "json", "array",
    ] {
        assert!(seen.contains(name), "variant {name} missing from the data set");
    }
    let _: Vec<ColumnMeta> = nasty_columns();
}
