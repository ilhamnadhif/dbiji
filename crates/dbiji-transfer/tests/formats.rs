//! Output of every export format: golden snippets for the text formats,
//! XLSX re-read with calamine and Parquet re-read with parquet's reader.

mod common;

use chrono::{FixedOffset, NaiveDate, NaiveTime, Timelike};
use common::*;
use dbiji_core::{ColumnMeta, DataKind, ObjectRef, Row, Value};
use dbiji_transfer::*;

fn cols() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::new("id", "int4", DataKind::Int),
        ColumnMeta::new("name", "text", DataKind::Text),
        ColumnMeta::new("price", "numeric(10,2)", DataKind::Decimal),
        ColumnMeta::new("born", "date", DataKind::Date),
        ColumnMeta::new("ok", "bool", DataKind::Bool),
    ]
}

fn rows() -> Vec<Row> {
    vec![
        vec![Value::Int(1), Value::text("Ann | \"A\""), Value::decimal("12.50"), Value::Date(NaiveDate::from_ymd_opt(2024, 1, 31).unwrap()), Value::Bool(true)],
        vec![Value::Int(2), Value::text("Bob\nSmith"), Value::Null, Value::Null, Value::Bool(false)],
    ]
}

fn table() -> Option<ObjectRef> {
    Some(ObjectRef::table(Some("app".into()), "t"))
}

async fn out(format: ExportFormat, opts: &ExportOptions) -> String {
    export_string(format, opts, cols(), rows(), table(), None).await
}

#[tokio::test]
async fn markdown_golden() {
    let md = out(ExportFormat::Markdown, &ExportOptions::default()).await;
    let expected = "| id | name | price | born | ok |\n\
                    |----:|------|------:|------|-----|\n\
                    | 1 | Ann \\| \"A\" | 12.50 | 2024-01-31 | true |\n\
                    | 2 | Bob<br>Smith |  |  | false |\n";
    assert_eq!(md, expected);
    let mut o = ExportOptions::default();
    o.markdown.escape_pipes = false;
    o.markdown.right_align_numbers = false;
    o.markdown.line_break = " ".into();
    o.values.null_text = "NULL".into();
    let md = out(ExportFormat::Markdown, &o).await;
    assert!(md.contains("|------|"), "{md}");
    assert!(!md.contains(":|"), "{md}");
    assert!(md.contains("| Ann | \"A\" |"), "{md}");
    assert!(md.contains("| Bob Smith | NULL | NULL |"), "{md}");
}

#[tokio::test]
async fn html_escapes_everything() {
    let mut r = rows();
    r.push(vec![Value::Int(3), Value::text("<script>alert('x')</script> & co"), Value::Null, Value::Null, Value::Null]);
    let mut o = ExportOptions::default();
    o.html.title = Some("Report <1>".into());
    let html = export_string(ExportFormat::Html, &o, cols(), r, table(), None).await;
    assert!(html.starts_with("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"UTF-8\">"), "{html}");
    assert!(html.contains("<title>Report &lt;1&gt;</title>"));
    assert!(html.contains("<style>"));
    assert!(html.contains("<thead>\n<tr><th>id</th><th>name</th><th>price</th><th>born</th><th>ok</th></tr>"));
    assert!(html.contains("<td class=\"num\">1</td><td>Ann | &quot;A&quot;</td><td class=\"num\">12.50</td>"));
    assert!(html.contains("<td class=\"null\">NULL</td>"));
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; co"));
    assert!(!html.contains("<script>"));
    assert!(html.trim_end().ends_with("</tbody>\n</table>\n</body>\n</html>"));
    o.html.embed_css = false;
    o.html.header = false;
    let html = export_string(ExportFormat::Html, &o, cols(), rows(), table(), None).await;
    assert!(!html.contains("<style>") && !html.contains("<thead>"));
}

#[tokio::test]
async fn xml_golden_and_null_modes() {
    let xml = out(ExportFormat::Xml, &ExportOptions::default()).await;
    let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<export>\n  <DATA_RECORD>\n    <id>1</id>\n    <name>Ann | \"A\"</name>\n    \
                    <price>12.50</price>\n    <born>2024-01-31</born>\n    <ok>true</ok>\n  </DATA_RECORD>\n  <DATA_RECORD>\n    <id>2</id>\n    \
                    <name>Bob\nSmith</name>\n    <price null=\"true\"/>\n    <born null=\"true\"/>\n    <ok>false</ok>\n  </DATA_RECORD>\n</export>\n";
    assert_eq!(xml, expected);
    let mut o = ExportOptions::default();
    o.xml.null_mode = XmlNullMode::Omit;
    o.xml.indent = false;
    o.xml.root_element = "${table}".into();
    o.xml.include_dtd = true;
    let xml = out(ExportFormat::Xml, &o).await;
    assert!(xml.contains("<!DOCTYPE t ["), "{xml}");
    assert!(xml.contains("<!ELEMENT DATA_RECORD (id?,name?,price?,born?,ok?)*>"), "{xml}");
    assert!(xml.contains("<t><DATA_RECORD><id>1</id>"), "{xml}");
    assert!(xml.contains("<DATA_RECORD><id>2</id><name>Bob\nSmith</name><ok>false</ok></DATA_RECORD></t>"), "{xml}");
    o.xml.null_mode = XmlNullMode::Text;
    o.values.null_text = "[NULL]".into();
    let xml = out(ExportFormat::Xml, &o).await;
    assert!(xml.contains("<price>[NULL]</price>"), "{xml}");
    // Characters XML cannot carry are transported as base64; CR is a character reference.
    let special =
        vec![vec![Value::Int(1), Value::text("a\u{1}b"), Value::Null, Value::Null, Value::Null], vec![Value::Int(2), Value::text("x\r\ny"), Value::Null, Value::Null, Value::Null]];
    let xml = export_string(ExportFormat::Xml, &ExportOptions::default(), cols(), special, table(), None).await;
    assert!(xml.contains("<name encoding=\"base64\">YQFi</name>"), "{xml}");
    assert!(xml.contains("<name>x&#13;\ny</name>"), "{xml}");
}

#[tokio::test]
async fn txt_golden() {
    let txt = out(ExportFormat::Txt, &ExportOptions::default()).await;
    let expected = "+----+-----------+--------+------------+-------+\n\
                    | id | name      | price  | born       | ok    |\n\
                    +----+-----------+--------+------------+-------+\n\
                    |  1 | Ann | \"A\" |  12.50 | 2024-01-31 | true  |\n\
                    |  2 | Bob Smith | [NULL] | [NULL]     | false |\n\
                    +----+-----------+--------+------------+-------+\n";
    assert_eq!(txt, expected);
    let mut o = ExportOptions::default();
    o.txt.borders = false;
    o.txt.show_nulls = false;
    o.txt.max_column_width = 6;
    o.txt.width_sample_rows = 1;
    let txt = out(ExportFormat::Txt, &o).await;
    let lines: Vec<&str> = txt.lines().collect();
    assert_eq!(lines[0], "id  name    price  born    ok");
    assert_eq!(lines[1], "--  ------  -----  ------  ----");
    assert_eq!(lines[2], " 1  Ann |…  12.50  2024-…  true");
    // Widths come from the first row only; later rows are cut to them.
    assert_eq!(lines[3], " 2  Bob S…                 fal…");
}

#[tokio::test]
async fn dbunit_golden() {
    let x = out(ExportFormat::DbUnit, &ExportOptions::default()).await;
    let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<dataset>\n    \
                    <t id=\"1\" name=\"Ann | &quot;A&quot;\" price=\"12.50\" born=\"2024-01-31\" ok=\"true\"/>\n    \
                    <t id=\"2\" name=\"Bob&#10;Smith\" ok=\"false\"/>\n</dataset>\n";
    assert_eq!(x, expected);
    let mut o = ExportOptions::default();
    o.dbunit.upper_case_table_name = true;
    o.dbunit.upper_case_column_names = true;
    o.dbunit.null_value = Some("[null]".into());
    let x = out(ExportFormat::DbUnit, &o).await;
    assert!(x.contains("<T ID=\"2\" NAME=\"Bob&#10;Smith\" PRICE=\"[null]\" BORN=\"[null]\" OK=\"false\"/>"), "{x}");
    let bin = vec![vec![Value::bytes(vec![1u8, 2, 3])]];
    let x = export_string(ExportFormat::DbUnit, &ExportOptions::default(), vec![ColumnMeta::new("b", "bytea", DataKind::Binary)], bin, None, None).await;
    assert!(x.contains("<query_result b=\"AQID\"/>"), "{x}");
}

#[tokio::test]
async fn php_golden() {
    let php = out(ExportFormat::Php, &ExportOptions::default()).await;
    let expected = "<?php\n$t = array(\n    array(\n        \"id\" => 1,\n        \"name\" => \"Ann | \\\"A\\\"\",\n        \
                    \"price\" => \"12.50\",\n        \"born\" => \"2024-01-31\",\n        \"ok\" => true,\n    ),\n    array(\n        \
                    \"id\" => 2,\n        \"name\" => \"Bob\\nSmith\",\n        \"price\" => null,\n        \"born\" => null,\n        \
                    \"ok\" => false,\n    ),\n);\n?>\n";
    assert_eq!(php, expected);
    let mut o = ExportOptions::default();
    o.php.short_array_syntax = true;
    o.php.variable_name = Some("$data".into());
    o.php.open_tag = false;
    let php = out(ExportFormat::Php, &o).await;
    assert!(php.starts_with("$data = [\n    [\n"), "{php}");
    assert!(php.ends_with("    ],\n];\n"), "{php}");
}

#[tokio::test]
async fn csv_options_golden() {
    let csv = out(ExportFormat::Csv, &ExportOptions::default()).await;
    assert_eq!(csv, "id,name,price,born,ok\n1,\"Ann | \"\"A\"\"\",12.50,2024-01-31,true\n2,\"Bob\nSmith\",,,false\n");
    let mut o = ExportOptions::default();
    o.csv.header_style = HeaderStyle::Both;
    o.csv.quote_mode = QuoteMode::Strings;
    o.csv.null_string = "NULL".into();
    o.csv.line_ending = LineEnding::CrLf;
    let csv = out(ExportFormat::Csv, &o).await;
    assert_eq!(
        csv,
        "\"id (int4)\",\"name (text)\",\"price (numeric(10,2))\",\"born (date)\",\"ok (bool)\"\r\n\
         1,\"Ann | \"\"A\"\"\",12.50,\"2024-01-31\",true\r\n2,\"Bob\nSmith\",NULL,NULL,false\r\n"
    );
    let mut o = ExportOptions::default();
    o.csv.header = false;
    o.csv.quote_mode = QuoteMode::Always;
    o.csv.escape = EscapeStyle::Backslash;
    let csv = out(ExportFormat::Csv, &o).await;
    assert_eq!(csv, "\"1\",\"Ann | \\\"A\\\"\",\"12.50\",\"2024-01-31\",\"true\"\n\"2\",\"Bob\nSmith\",,,\"false\"\n");
    let tsv = out(ExportFormat::Tsv, &ExportOptions::default()).await;
    assert_eq!(tsv, "id\tname\tprice\tborn\tok\n1\t\"Ann | \"\"A\"\"\"\t12.50\t2024-01-31\ttrue\n2\t\"Bob\nSmith\"\t\t\tfalse\n");
    // Value formatting.
    let mut o = ExportOptions::default();
    o.values.date_format = Some("%d.%m.%Y".into());
    o.values.decimal_separator = Some(',');
    o.values.true_text = "yes".into();
    o.values.false_text = "no".into();
    o.csv.delimiter = ";".into();
    let csv = out(ExportFormat::Csv, &o).await;
    assert!(csv.contains("1;\"Ann | \"\"A\"\"\";12,50;31.01.2024;yes\n"), "{csv}");
}

#[tokio::test]
async fn text_encodings_and_bom() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.csv");
    let r = vec![vec![Value::text("café €")]];
    let c = vec![ColumnMeta::new("v", "text", DataKind::Text)];
    let o = ExportOptions { encoding: "windows-1252".into(), ..Default::default() };
    export_to(ExportFormat::Csv, &o, &path, c.clone(), r.clone(), None).await;
    assert_eq!(std::fs::read(&path).unwrap(), b"v\ncaf\xe9 \x80\n");
    let o = ExportOptions { encoding: "UTF-8".into(), insert_bom: true, ..Default::default() };
    export_to(ExportFormat::Csv, &o, &path, c.clone(), r.clone(), None).await;
    assert_eq!(std::fs::read(&path).unwrap()[..3], [0xEF, 0xBB, 0xBF]);
    let o = ExportOptions { encoding: "UTF-16LE".into(), insert_bom: true, ..Default::default() };
    export_to(ExportFormat::Csv, &o, &path, c.clone(), r.clone(), None).await;
    let b = std::fs::read(&path).unwrap();
    assert_eq!(&b[..4], &[0xFF, 0xFE, b'v', 0]);
    // Unrepresentable characters become '?' and are reported.
    let o = ExportOptions { encoding: "ISO-8859-2".into(), ..Default::default() };
    let s = export_to(ExportFormat::Csv, &o, &path, c, vec![vec![Value::text("😀")]], None).await;
    assert_eq!(std::fs::read(&path).unwrap(), b"v\n?\n");
    assert!(s.warnings.iter().any(|w| w.contains("replaced")), "{:?}", s.warnings);
    // XML declares the encoding it is written in.
    let o = ExportOptions { encoding: "windows-1252".into(), ..Default::default() };
    let xml_path = dir.path().join("x.xml");
    export_to(ExportFormat::Xml, &o, &xml_path, vec![ColumnMeta::new("v", "text", DataKind::Text)], vec![vec![Value::text("é")]], None).await;
    let bytes = std::fs::read(&xml_path).unwrap();
    assert!(bytes.starts_with(b"<?xml version=\"1.0\" encoding=\"windows-1252\"?>"));
    let mut src = XmlSource::open(&xml_path, XmlReadOptions::default()).unwrap();
    assert_eq!(src.encoding(), "windows-1252");
    assert_eq!(drain(&mut src).await, vec![vec![Value::text("é")]]);
}

#[tokio::test]
async fn json_golden() {
    let json = out(ExportFormat::Json, &ExportOptions::default()).await;
    let expected = "[\n  {\n    \"id\": 1,\n    \"name\": \"Ann | \\\"A\\\"\",\n    \"price\": 12.50,\n    \"born\": \"2024-01-31\",\n    \
                    \"ok\": true\n  },\n  {\n    \"id\": 2,\n    \"name\": \"Bob\\nSmith\",\n    \"price\": null,\n    \"born\": null,\n    \
                    \"ok\": false\n  }\n]\n";
    assert_eq!(json, expected);
    let mut o = ExportOptions::default();
    o.json.layout = JsonLayout::ColumnsAndRows;
    o.json.pretty = false;
    o.json.include_table_name = true;
    let json = out(ExportFormat::Json, &o).await;
    assert_eq!(
        json,
        "{\"table\":\"t\",\"columns\":[{\"name\":\"id\",\"type\":\"int4\",\"kind\":\"int\"},{\"name\":\"name\",\"type\":\"text\",\"kind\":\"text\"},\
         {\"name\":\"price\",\"type\":\"numeric(10,2)\",\"kind\":\"decimal\"},{\"name\":\"born\",\"type\":\"date\",\"kind\":\"date\"},\
         {\"name\":\"ok\",\"type\":\"bool\",\"kind\":\"bool\"}],\"rows\":[[1,\"Ann | \\\"A\\\"\",12.50,\"2024-01-31\",true],\
         [2,\"Bob\\nSmith\",null,null,false]]}\n"
    );
    let lines = out(ExportFormat::JsonLines, &ExportOptions::default()).await;
    assert_eq!(
        lines,
        "{\"id\":1,\"name\":\"Ann | \\\"A\\\"\",\"price\":12.50,\"born\":\"2024-01-31\",\"ok\":true}\n\
         {\"id\":2,\"name\":\"Bob\\nSmith\",\"price\":null,\"born\":null,\"ok\":false}\n"
    );
    let mut wrapped = ExportOptions::default();
    wrapped.json.include_table_name = true;
    let json = out(ExportFormat::Json, &wrapped).await;
    assert!(json.starts_with("{\n  \"t\": [\n    {\n      \"id\": 1,"), "{json}");
    let empty = export_string(ExportFormat::Json, &ExportOptions::default(), cols(), vec![], None, None).await;
    assert_eq!(empty, "[]\n");
}

#[tokio::test]
async fn sql_golden() {
    let dialect = Some(generic());
    let sql = export_string(ExportFormat::Sql, &ExportOptions::default(), cols(), rows(), table(), dialect.clone()).await;
    assert_eq!(
        sql,
        "INSERT INTO app.t (id, name, price, born, ok) VALUES\n(1, 'Ann | \"A\"', 12.50, '2024-01-31', TRUE),\n\
         (2, 'Bob\nSmith', NULL, NULL, FALSE);\n"
    );
    let mut o = ExportOptions::default();
    o.sql.rows_per_statement = 1;
    o.sql.quoting = IdentifierQuoting::Always;
    o.sql.terminator = ";;".into();
    o.sql.qualify_with_schema = false;
    let sql = export_string(ExportFormat::Sql, &o, cols(), rows(), table(), dialect.clone()).await;
    assert_eq!(
        sql,
        "INSERT INTO \"t\" (\"id\", \"name\", \"price\", \"born\", \"ok\") VALUES (1, 'Ann | \"A\"', 12.50, '2024-01-31', TRUE);;\n\
         INSERT INTO \"t\" (\"id\", \"name\", \"price\", \"born\", \"ok\") VALUES (2, 'Bob\nSmith', NULL, NULL, FALSE);;\n"
    );
    let mut o = ExportOptions::default();
    o.sql.include_create_table = true;
    o.sql.table_name = Some("copy".into());
    o.sql.schema_name = Some("dst".into());
    o.sql.key_columns = vec!["id".into()];
    let sql = export_string(ExportFormat::Sql, &o, cols(), rows(), table(), dialect.clone()).await;
    assert!(
        sql.starts_with("CREATE TABLE dst.copy (\n    id BIGINT,\n    name text,\n    price numeric(10,2),\n    born date,\n    ok BOOLEAN,\n    PRIMARY KEY (id)\n);\n\n"),
        "{sql}"
    );
    assert!(sql.contains("INSERT INTO dst.copy (id, name, price, born, ok) VALUES"));
    // The generated script runs in SQLite.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("ATTACH ':memory:' AS dst").unwrap();
    conn.execute_batch(&sql).unwrap();
    assert_eq!(select(&conn, "SELECT count(*) FROM dst.copy"), vec![vec![Value::Int(2)]]);
    // Upsert: native when the dialect has one, DELETE + INSERT otherwise.
    let mut o = ExportOptions::default();
    o.sql.mode = SqlInsertMode::Upsert;
    o.sql.key_columns = vec!["id".into()];
    o.sql.qualify_with_schema = false;
    let native = export_string(ExportFormat::Sql, &o, cols(), rows(), table(), Some(sqlite_dialect())).await;
    assert!(native.starts_with("INSERT INTO t (id, name, price, born, ok) VALUES (1, 'Ann | \"A\"', '12.50', '2024-01-31', TRUE) ON CONFLICT (id) DO UPDATE SET"), "{native}");
    let fallback = export_string(ExportFormat::Sql, &o, cols(), rows(), table(), dialect.clone()).await;
    assert!(fallback.starts_with("DELETE FROM t WHERE id = 1;\nINSERT INTO t (id, name, price, born, ok) VALUES (1, "), "{fallback}");
    // Upsert without keys falls back to INSERT with a warning.
    let mut o = ExportOptions::default();
    o.sql.mode = SqlInsertMode::Upsert;
    let dir = tempfile::tempdir().unwrap();
    let s = export_to(ExportFormat::Sql, &o, &dir.path().join("x.sql"), cols(), rows(), dialect).await;
    assert!(s.warnings.iter().any(|w| w.contains("key columns")), "{:?}", s.warnings);
}

#[tokio::test]
async fn sql_uses_table_definition_and_type_hints() {
    use dbiji_core::ddl::{ColumnDef, KeyDef, TableDef};
    let mut def = TableDef::new(Some("app".into()), "t");
    def.columns.push(ColumnDef::new("id", "int4").not_null());
    def.columns.push(ColumnDef::new("name", "text"));
    def.columns.push(ColumnDef::new("price", "numeric(10,2)"));
    def.columns.push(ColumnDef::new("born", "date"));
    def.columns.push(ColumnDef::new("ok", "bool"));
    def.primary_key = Some(KeyDef { name: Some("t_pkey".into()), columns: vec!["id".into()] });
    let mut o = ExportOptions::default();
    o.sql.include_create_table = true;
    o.sql.mode = SqlInsertMode::Upsert;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.sql");
    let mut src = VecSource::new(cols(), rows()).with_table(ObjectRef::table(Some("app".into()), "t"));
    let mut sink = create_sink(ExportFormat::Sql, &o, ExportTarget::path(&path), Some(sqlite_dialect())).unwrap();
    sink.set_table_definition(&def);
    run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.unwrap();
    let sql = std::fs::read_to_string(&path).unwrap();
    // The table definition supplies the primary key used for the upsert.
    assert!(sql.contains("CONSTRAINT t_pkey PRIMARY KEY (id)") || sql.contains("PRIMARY KEY (id)"), "{sql}");
    assert!(sql.contains("ON CONFLICT (id) DO UPDATE SET"), "{sql}");
}

#[tokio::test]
async fn xlsx_reread_with_calamine() {
    use calamine::{Data, Reader, Xlsx, open_workbook};
    let c = vec![
        ColumnMeta::new("int", "int8", DataKind::Int),
        ColumnMeta::new("float", "float8", DataKind::Float),
        ColumnMeta::new("dec", "numeric", DataKind::Decimal),
        ColumnMeta::new("text", "text", DataKind::Text),
        ColumnMeta::new("bool", "bool", DataKind::Bool),
        ColumnMeta::new("date", "date", DataKind::Date),
        ColumnMeta::new("ts", "timestamp", DataKind::DateTime),
        ColumnMeta::new("tstz", "timestamptz", DataKind::DateTimeTz),
        ColumnMeta::new("time", "time", DataKind::Time),
        ColumnMeta::new("bin", "bytea", DataKind::Binary),
        ColumnMeta::new("big", "int8", DataKind::Int),
    ];
    let d = NaiveDate::from_ymd_opt(2025, 10, 13).unwrap();
    let r = vec![
        vec![
            Value::Int(42),
            Value::Float(2.5),
            Value::decimal("12.50"),
            Value::text("héllo\nworld"),
            Value::Bool(true),
            Value::Date(d),
            Value::DateTime(d.and_hms_opt(12, 30, 0).unwrap()),
            Value::DateTimeTz(d.and_hms_opt(8, 0, 0).unwrap().and_local_timezone(FixedOffset::east_opt(7 * 3600).unwrap()).unwrap()),
            Value::Time(NaiveTime::from_hms_opt(6, 0, 0).unwrap()),
            Value::bytes(vec![0xABu8, 0xCD]),
            Value::Int(1_234_567_890_123_456_789),
        ],
        vec![
            Value::Null,
            Value::Float(f64::NAN),
            Value::decimal("12345678901234567890.5"),
            Value::text(""),
            Value::Bool(false),
            Value::Date(NaiveDate::from_ymd_opt(1899, 1, 1).unwrap()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.xlsx");
    let s = export_to(ExportFormat::Xlsx, &ExportOptions::default(), &path, c.clone(), r.clone(), None).await;
    assert_eq!(s.rows, 2);
    assert_eq!(s.files, vec![path.clone()]);
    let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
    assert_eq!(wb.sheet_names(), vec!["Sheet1".to_string()]);
    let range = wb.worksheet_range("Sheet1").unwrap();
    let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();
    let header: Vec<String> = rows[0].iter().map(|d| d.to_string()).collect();
    assert_eq!(header, c.iter().map(|m| m.name.clone()).collect::<Vec<_>>());
    let num = |d: &Data| match d {
        Data::Float(f) => *f,
        Data::Int(i) => *i as f64,
        other => panic!("not a number: {other:?}"),
    };
    assert_eq!(num(&rows[1][0]), 42.0);
    assert_eq!(num(&rows[1][1]), 2.5);
    assert_eq!(num(&rows[1][2]), 12.5);
    assert_eq!(rows[1][3], Data::String("héllo\nworld".into()));
    assert_eq!(rows[1][4], Data::Bool(true));
    match &rows[1][5] {
        Data::DateTime(dt) => assert_eq!(dt.as_f64(), 45943.0),
        other => panic!("{other:?}"),
    }
    match &rows[1][6] {
        Data::DateTime(dt) => assert!((dt.as_f64() - 45943.520833333).abs() < 1e-6),
        other => panic!("{other:?}"),
    }
    assert!(matches!(&rows[1][7], Data::DateTime(_)));
    match &rows[1][8] {
        Data::DateTime(dt) => assert!((dt.as_f64() - 0.25).abs() < 1e-9),
        other => panic!("{other:?}"),
    }
    assert_eq!(rows[1][9], Data::String("abcd".into()));
    // Beyond 15 digits Excel numbers lose precision: written as text.
    assert_eq!(rows[1][10], Data::String("1234567890123456789".into()));
    assert_eq!(rows[2][0], Data::Empty);
    assert_eq!(rows[2][1], Data::String("NaN".into()));
    assert_eq!(rows[2][2], Data::String("12345678901234567890.5".into()));
    // Dates before 1900-03-01 cannot be Excel dates.
    assert_eq!(rows[2][5], Data::String("1899-01-01".into()));

    // The importer reads it back with typed values.
    let mut src = XlsxSource::open(&path, XlsxReadOptions::default()).await.unwrap();
    assert_eq!(src.columns().len(), c.len());
    let back = drain(&mut src).await;
    assert_eq!(back[0][0], Value::Float(42.0));
    assert_eq!(back[0][4], Value::Bool(true));
    assert_eq!(back[0][5], Value::Date(d));
    assert_eq!(back[0][6], Value::DateTime(d.and_hms_opt(12, 30, 0).unwrap()));
    assert_eq!(back[0][8], Value::Time(NaiveTime::from_hms_opt(6, 0, 0).unwrap()));
    let converted = convert_value(&back[0][0], DataKind::Int, &ConvertOptions::default()).unwrap();
    assert_eq!(converted, Value::Int(42));
}

#[tokio::test]
async fn xlsx_options_and_sheet_split() {
    use calamine::{Data, Reader, Xlsx, open_workbook};
    let c = vec![ColumnMeta::new("n", "int", DataKind::Int), ColumnMeta::new("label", "text", DataKind::Text)];
    let r: Vec<Row> = (1..=5).map(|i| vec![Value::Int(i), Value::text(format!("row {i}"))]).collect();
    let mut o = ExportOptions::default();
    o.xlsx.max_rows_per_sheet = 2;
    o.xlsx.sheet_name = Some("Data/2024: [x]".into());
    o.xlsx.number_format = Some("#,##0.00".into());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split.xlsx");
    let s = export_to(ExportFormat::Xlsx, &o, &path, c, r, None).await;
    assert!(s.warnings.iter().any(|w| w.contains("3 sheets")), "{:?}", s.warnings);
    let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
    let names = wb.sheet_names();
    assert_eq!(names, vec!["Data_2024_ _x_".to_string(), "Data_2024_ _x_ (2)".into(), "Data_2024_ _x_ (3)".into()]);
    let mut total = 0;
    for n in &names {
        let range = wb.worksheet_range(n).unwrap();
        let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();
        assert_eq!(rows[0], vec![Data::String("n".into()), Data::String("label".into())], "{n}");
        total += rows.len() - 1;
    }
    assert_eq!(total, 5);
    // Writer targets get the file through a temporary file.
    let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    struct Shared(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut src = VecSource::new(cols(), rows());
    let mut sink = create_sink(ExportFormat::Xlsx, &ExportOptions::default(), ExportTarget::writer(Shared(buf.clone())), None).unwrap();
    run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.unwrap();
    assert_eq!(&buf.lock()[..2], b"PK");
}

#[tokio::test]
async fn parquet_reread_with_parquet_reader() {
    use parquet::basic::{Compression, LogicalType, Type as Physical};
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;
    let c = vec![
        ColumnMeta::new("id", "int8", DataKind::Int),
        ColumnMeta::new("ok", "bool", DataKind::Bool),
        ColumnMeta::new("score", "float8", DataKind::Float),
        ColumnMeta::new("amount", "numeric", DataKind::Decimal),
        ColumnMeta::new("name", "text", DataKind::Text),
        ColumnMeta::new("day", "date", DataKind::Date),
        ColumnMeta::new("ts", "timestamp", DataKind::DateTime),
        ColumnMeta::new("tstz", "timestamptz", DataKind::DateTimeTz),
        ColumnMeta::new("raw", "bytea", DataKind::Binary),
        ColumnMeta::new("doc", "jsonb", DataKind::Json),
        ColumnMeta::new("at", "time", DataKind::Time),
        ColumnMeta::new("u", "bigint unsigned", DataKind::Int),
    ];
    let d = NaiveDate::from_ymd_opt(1970, 1, 11).unwrap();
    let ts = d.and_hms_micro_opt(0, 0, 1, 500).unwrap();
    let tz = ts.and_local_timezone(FixedOffset::east_opt(3600).unwrap()).unwrap();
    let mut r: Vec<Row> = Vec::new();
    for i in 0..5i64 {
        r.push(vec![
            Value::Int(i),
            Value::Bool(i % 2 == 0),
            Value::Float(i as f64 / 2.0),
            Value::decimal(format!("{i}.50")),
            if i == 3 { Value::Null } else { Value::text(format!("n{i} ünï")) },
            Value::Date(d),
            Value::DateTime(ts),
            Value::DateTimeTz(tz),
            Value::bytes(vec![i as u8; 3]),
            Value::json(format!("{{\"i\":{i}}}")),
            Value::Time(NaiveTime::from_hms_micro_opt(1, 2, 3, 4).unwrap()),
            Value::UInt(u64::MAX - i as u64),
        ]);
    }
    let mut o = ExportOptions::default();
    o.parquet.row_group_rows = 2;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.parquet");
    let s = export_to(ExportFormat::Parquet, &o, &path, c.clone(), r, None).await;
    assert_eq!(s.rows, 5);
    assert!(s.bytes > 0);
    let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.file_metadata().num_rows(), 5);
    assert_eq!(meta.num_row_groups(), 3);
    assert_eq!(meta.row_group(0).column(0).compression(), Compression::SNAPPY);
    let schema = meta.file_metadata().schema_descr();
    let expect = [
        (Physical::INT64, None),
        (Physical::BOOLEAN, None),
        (Physical::DOUBLE, None),
        (Physical::BYTE_ARRAY, Some(LogicalType::String)),
        (Physical::BYTE_ARRAY, Some(LogicalType::String)),
        (Physical::INT32, Some(LogicalType::Date)),
        (Physical::INT64, Some(LogicalType::timestamp(false, parquet::basic::TimeUnit::MICROS))),
        (Physical::INT64, Some(LogicalType::timestamp(true, parquet::basic::TimeUnit::MICROS))),
        (Physical::BYTE_ARRAY, None),
        (Physical::BYTE_ARRAY, Some(LogicalType::Json)),
        (Physical::INT64, Some(LogicalType::time(false, parquet::basic::TimeUnit::MICROS))),
        (Physical::INT64, Some(LogicalType::integer(64, false))),
    ];
    for (i, (phys, logical)) in expect.iter().enumerate() {
        let col = schema.column(i);
        assert_eq!(col.physical_type(), *phys, "{}", col.name());
        assert_eq!(col.logical_type_ref(), logical.as_ref(), "{}", col.name());
    }
    let rows: Vec<parquet::record::Row> = reader.get_row_iter(None).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 5);
    let fields: Vec<&Field> = rows[3].get_column_iter().map(|(_, f)| f).collect();
    assert_eq!(fields[0], &Field::Long(3));
    assert_eq!(fields[1], &Field::Bool(false));
    assert_eq!(fields[2], &Field::Double(1.5));
    assert_eq!(fields[3], &Field::Str("3.50".into()));
    assert_eq!(fields[4], &Field::Null);
    assert_eq!(fields[5], &Field::Date(10));
    let local_micros = 10 * 86_400 * 1_000_000 + 1_000_500;
    assert_eq!(fields[6], &Field::TimestampMicros(local_micros));
    // UTC adjusted: 00:00:01 at +01:00 is 23:00:01 UTC the previous day.
    assert_eq!(fields[7], &Field::TimestampMicros(local_micros - 3_600_000_000));
    let time_micros: i64 = (3600 + 2 * 60 + 3) * 1_000_000 + 4;
    match fields[10] {
        Field::TimeMicros(us) | Field::Long(us) => assert_eq!(*us, time_micros),
        other => panic!("{other:?}"),
    }
    match fields[11] {
        Field::ULong(u) => assert_eq!(*u, u64::MAX - 3),
        other => panic!("{other:?}"),
    }
    let name0: Vec<&Field> = rows[0].get_column_iter().map(|(_, f)| f).collect();
    assert_eq!(name0[4], &Field::Str("n0 ünï".into()));
    assert!(!name0[6].to_string().is_empty());
    let _ = ts.nanosecond();
}

#[tokio::test]
async fn parquet_type_mismatch_is_an_error_with_row() {
    let c = vec![ColumnMeta::new("n", "int", DataKind::Int)];
    let r = vec![vec![Value::Int(1)], vec![Value::text("not a number")]];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.parquet");
    let mut src = VecSource::new(c, r);
    let mut sink = create_sink(ExportFormat::Parquet, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    let err = run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.unwrap_err();
    assert!(err.message.contains("Row 2, column n"), "{}", err.message);
    // The partial file is removed.
    assert!(!path.exists());
}

#[tokio::test]
async fn failed_exports_leave_no_files() {
    let dir = tempfile::tempdir().unwrap();
    // Parquet needs at least one column.
    let path = dir.path().join("none.parquet");
    let mut src = VecSource::new(vec![], vec![]);
    let mut sink = create_sink(ExportFormat::Parquet, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
    assert!(run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.is_err());
    assert!(!path.exists());
    // A failing source aborts a text export and removes its file.
    struct Failing(Vec<ColumnMeta>);
    #[async_trait::async_trait]
    impl RowSource for Failing {
        fn columns(&self) -> &[ColumnMeta] {
            &self.0
        }
        async fn next_batch(&mut self, _max: usize) -> dbiji_core::DbResult<Vec<Row>> {
            Err(dbiji_core::DbError::connection("connection reset"))
        }
    }
    for format in [ExportFormat::Csv, ExportFormat::Json, ExportFormat::Xlsx, ExportFormat::Sql] {
        let path = dir.path().join(format!("fail.{}", format.extension()));
        let mut src = Failing(cols());
        let mut sink = create_sink(format, &ExportOptions::default(), ExportTarget::path(&path), None).unwrap();
        let err = run_export(&mut src, sink.as_mut(), &Progress::new(), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Connection);
        assert!(!path.exists(), "{format:?}");
    }
    // Unknown encodings are rejected up front.
    let o = ExportOptions { encoding: "no-such".into(), ..Default::default() };
    assert!(create_sink(ExportFormat::Csv, &o, ExportTarget::path(dir.path().join("x.csv")), None).is_err());
}

#[test]
fn format_metadata() {
    assert_eq!(ExportFormat::DbUnit.extension(), "xml");
    assert!(ExportFormat::Parquet.is_binary());
    assert!(!ExportFormat::Markdown.is_binary());
}
