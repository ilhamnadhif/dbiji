//! Test helpers: a `Session` backed by rusqlite, a session factory, a
//! SQLite-flavoured test dialect and the "nasty" data set of the round trip gate.
#![allow(dead_code)]
// `DbError` is defined by dbiji-core (144 bytes).
#![allow(clippy::result_large_err)]

use async_trait::async_trait;
use chrono::{FixedOffset, NaiveDate, NaiveTime};
use dbiji_core::admin::TabularData;
use dbiji_core::ddl::{ColumnDef, DdlGenerator, FkAction, ForeignKeyDef, IndexColumn, IndexDef, KeyDef, SchemaSnapshot, SnapshotOptions, TableDef};
use dbiji_core::dialect::{GenericDialect, SqlSyntax};
use dbiji_core::driver::{CancelHandle, ExecOptions, MaterializedResult, MemoryCursor, QueryCursor, ResultHeader, ServerInfo, Session, TransactionMode};
use dbiji_core::meta::{DatabaseInfo, MetadataSearch, ObjectInfo, ObjectKind, SchemaInfo, SchemaRef};
use dbiji_core::plan::QueryPlan;
use dbiji_core::{ColumnMeta, DataKind, DbError, DbKind, DbResult, Dialect, ObjectRef, Row, Value};
use dbiji_transfer::SessionFactory;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::types::{Value as SqlValue, ValueRef};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn err(e: rusqlite::Error) -> DbError {
    DbError::query(e.to_string())
}

pub fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => SqlValue::Integer(i),
            Err(_) => SqlValue::Text(u.to_string()),
        },
        Value::Float(f) => SqlValue::Real(*f),
        Value::Bytes(b) => SqlValue::Blob(b.to_vec()),
        other => SqlValue::Text(other.to_text().into_owned()),
    }
}

pub fn from_sql(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::bytes(b.to_vec()),
    }
}

/// Run one statement and materialise its result.
pub fn run(conn: &Connection, sql: &str, params: &[Value]) -> DbResult<MaterializedResult> {
    let mut stmt = conn.prepare(sql).map_err(err)?;
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, to_sql(p)).map_err(err)?;
    }
    let ncols = stmt.column_count();
    if ncols > 0 {
        let cols: Vec<ColumnMeta> = stmt
            .columns()
            .iter()
            .map(|c| {
                let decl = c.decl_type().unwrap_or("").to_string();
                let kind = if decl.is_empty() { DataKind::Other } else { DataKind::guess_from_type_name(&decl) };
                ColumnMeta::new(c.name(), decl, kind)
            })
            .collect();
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(err)? {
            let mut row = Vec::with_capacity(ncols);
            for i in 0..ncols {
                row.push(from_sql(r.get_ref(i).map_err(err)?));
            }
            out.push(row);
        }
        Ok(MaterializedResult { header: ResultHeader::Rows(cols), rows: out })
    } else {
        let n = stmt.raw_execute().map_err(err)?;
        Ok(MaterializedResult {
            header: ResultHeader::Update { rows_affected: Some(n as u64), last_insert_id: None, tag: None },
            rows: Vec::new(),
        })
    }
}

/// Rows of `sql` (test convenience).
pub fn select(conn: &Connection, sql: &str) -> Vec<Row> {
    run(conn, sql, &[]).unwrap().rows
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `TableDef` from SQLite's pragmas.
pub fn table_def(conn: &Connection, schema: Option<String>, name: &str) -> DbResult<TableDef> {
    let info = run(conn, &format!("PRAGMA table_info({})", quote(name)), &[])?.rows;
    if info.is_empty() {
        return Err(DbError::query(format!("no such table: {name}")));
    }
    let mut def = TableDef::new(schema, name);
    let mut pk: Vec<(i64, String)> = Vec::new();
    for r in &info {
        let col_name = r[1].to_text().into_owned();
        let ty = r[2].to_text().into_owned();
        let mut c = ColumnDef::new(&col_name, &ty);
        c.nullable = r[3].as_i64() != Some(1);
        c.default = if r[4].is_null() { None } else { Some(r[4].to_text().into_owned()) };
        c.ordinal = r[0].as_i64().unwrap_or(0) as u32 + 1;
        if let Some(p) = r[5].as_i64().filter(|p| *p > 0) {
            pk.push((p, col_name.clone()));
        }
        def.columns.push(c);
    }
    pk.sort();
    if !pk.is_empty() {
        def.primary_key = Some(KeyDef { name: None, columns: pk.into_iter().map(|p| p.1).collect() });
    }
    let fks = run(conn, &format!("PRAGMA foreign_key_list({})", quote(name)), &[])?.rows;
    let mut by_id: Vec<(i64, ForeignKeyDef)> = Vec::new();
    for r in fks {
        let id = r[0].as_i64().unwrap_or(0);
        let entry = match by_id.iter_mut().find(|(i, _)| *i == id) {
            Some(e) => e,
            None => {
                by_id.push((
                    id,
                    ForeignKeyDef {
                        ref_table: r[2].to_text().into_owned(),
                        on_update: FkAction::parse(&r[5].to_text()),
                        on_delete: FkAction::parse(&r[6].to_text()),
                        ..Default::default()
                    },
                ));
                by_id.last_mut().unwrap()
            }
        };
        entry.1.columns.push(r[3].to_text().into_owned());
        entry.1.ref_columns.push(r[4].to_text().into_owned());
    }
    def.foreign_keys = by_id.into_iter().map(|(_, f)| f).collect();
    let idx = run(conn, &format!("PRAGMA index_list({})", quote(name)), &[])?.rows;
    for r in idx {
        let idx_name = r[1].to_text().into_owned();
        let unique = r[2].as_i64() == Some(1);
        let origin = r[3].to_text().into_owned();
        let cols: Vec<String> = run(conn, &format!("PRAGMA index_info({})", quote(&idx_name)), &[])?
            .rows
            .into_iter()
            .map(|c| c[2].to_text().into_owned())
            .collect();
        match origin.as_str() {
            "u" => def.unique_keys.push(KeyDef { name: None, columns: cols }),
            "pk" => {}
            _ => def.indexes.push(IndexDef {
                name: idx_name,
                columns: cols.into_iter().map(IndexColumn::column).collect(),
                unique,
                ..Default::default()
            }),
        }
    }
    Ok(def)
}

/// GenericDialect behaviour plus what SQLite needs: decimals and large
/// unsigned values as text literals (SQLite parses unquoted numbers as f64),
/// native upsert (`ON CONFLICT DO UPDATE`), `DELETE` instead of `TRUNCATE`.
pub struct SqliteTestDialect {
    inner: GenericDialect,
}

impl SqliteTestDialect {
    pub fn new() -> Self {
        SqliteTestDialect { inner: GenericDialect::new() }
    }
}

impl Dialect for SqliteTestDialect {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }
    fn syntax(&self) -> &SqlSyntax {
        self.inner.syntax()
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        self.inner.ddl()
    }
    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        match value {
            Value::Decimal(s) => self.string_literal(s),
            Value::UInt(u) if *u > i64::MAX as u64 => self.string_literal(&u.to_string()),
            other => self.inner.literal(other, type_name),
        }
    }
    fn upsert(&self, table: &str, columns: &[String], key_columns: &[String], values: &[String]) -> Option<String> {
        let updates: Vec<String> =
            columns.iter().filter(|c| !key_columns.contains(c)).map(|c| format!("{c} = excluded.{c}")).collect();
        let action = if updates.is_empty() { "NOTHING".to_string() } else { format!("UPDATE SET {}", updates.join(", ")) };
        Some(format!(
            "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) DO {action}",
            columns.join(", "),
            values.join(", "),
            key_columns.join(", ")
        ))
    }
    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!("DELETE FROM {}", self.qualified_ref(table))
    }
}

/// A session over a rusqlite connection.
pub struct SqliteSession {
    pub conn: Connection,
    dialect: Arc<dyn Dialect>,
    info: ServerInfo,
    mode: TransactionMode,
    closed: bool,
    /// Every executed statement.
    pub log: Arc<Mutex<Vec<String>>>,
    /// Statements containing this text fail.
    pub fail_on: Option<String>,
}

impl SqliteSession {
    pub fn open(path: &Path, dialect: Arc<dyn Dialect>) -> Self {
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        Self::with_conn(conn, dialect)
    }

    pub fn memory(dialect: Arc<dyn Dialect>) -> Self {
        Self::with_conn(Connection::open_in_memory().unwrap(), dialect)
    }

    pub fn with_conn(conn: Connection, dialect: Arc<dyn Dialect>) -> Self {
        SqliteSession {
            conn,
            dialect,
            info: ServerInfo { product: "SQLite".into(), version: "3".into(), version_number: (3, 0, 0), session_id: None },
            mode: TransactionMode::Auto,
            closed: false,
            log: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        }
    }

    pub fn exec_batch(&self, sql: &str) {
        self.conn.execute_batch(sql).unwrap();
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }
    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        None
    }
    async fn ping(&mut self) -> DbResult<()> {
        Ok(())
    }
    async fn close(&mut self) -> DbResult<()> {
        self.closed = true;
        Ok(())
    }
    fn is_closed(&self) -> bool {
        self.closed
    }
    async fn execute(&mut self, sql: &str, params: &[Value], _opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        self.log.lock().push(sql.to_string());
        if let Some(f) = &self.fail_on
            && sql.contains(f.as_str()) {
                return Err(DbError::query(format!("injected failure for: {f}")));
            }
        if sql.contains("/*endless*/") {
            return Ok(Box::new(EndlessCursor::new(std::time::Duration::from_millis(1))));
        }
        if self.mode == TransactionMode::Manual && self.conn.is_autocommit() {
            self.conn.execute_batch("BEGIN").map_err(err)?;
        }
        let result = run(&self.conn, sql, params)?;
        Ok(Box::new(MemoryCursor::new(vec![result])))
    }
    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }
    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        if mode == TransactionMode::Auto && !self.conn.is_autocommit() {
            self.conn.execute_batch("COMMIT").map_err(err)?;
        }
        self.mode = mode;
        Ok(())
    }
    fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }
    async fn commit(&mut self) -> DbResult<()> {
        self.log.lock().push("COMMIT".into());
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("COMMIT").map_err(err)?;
        }
        Ok(())
    }
    async fn rollback(&mut self) -> DbResult<()> {
        self.log.lock().push("ROLLBACK".into());
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK").map_err(err)?;
        }
        Ok(())
    }
    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(None)
    }
    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some("main".into()))
    }
    async fn set_current_schema(&mut self, _schema: &str) -> DbResult<()> {
        Ok(())
    }
    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(Vec::new())
    }
    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        Ok(Vec::new())
    }
    async fn list_objects(&mut self, _schema: &SchemaRef, _kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }
    async fn list_children(&mut self, _parent: &ObjectRef, _kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }
    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        table_def(&self.conn, table.schema.clone(), &table.name)
    }
    async fn object_ddl(&mut self, _obj: &ObjectRef) -> DbResult<String> {
        Ok(String::new())
    }
    async fn search_metadata(&mut self, _search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }
    async fn schema_snapshot(&mut self, _schema: &SchemaRef, _opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        Ok(SchemaSnapshot::default())
    }
    async fn explain(&mut self, _sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        Ok(QueryPlan::default())
    }
}

/// A cursor producing rows forever (slowly), for cancellation tests.
pub struct EndlessCursor {
    delay: std::time::Duration,
    header_sent: bool,
    pub served: u64,
}

impl EndlessCursor {
    pub fn new(delay: std::time::Duration) -> Self {
        EndlessCursor { delay, header_sent: false, served: 0 }
    }
}

#[async_trait]
impl QueryCursor for EndlessCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        if self.header_sent {
            return Ok(None);
        }
        self.header_sent = true;
        Ok(Some(ResultHeader::Rows(vec![
            ColumnMeta::new("n", "int8", DataKind::Int),
            ColumnMeta::new("label", "text", DataKind::Text),
        ])))
    }
    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        tokio::time::sleep(self.delay).await;
        let rows = (0..max_rows)
            .map(|i| {
                let n = self.served + i as u64;
                vec![Value::Int(n as i64), Value::text(format!("row {n}"))]
            })
            .collect();
        self.served += max_rows as u64;
        Ok(rows)
    }
    async fn close(&mut self) -> DbResult<()> {
        Ok(())
    }
    fn take_messages(&mut self) -> Vec<dbiji_core::ServerMessage> {
        Vec::new()
    }
}

/// Factory opening sessions on one SQLite file.
pub struct SqliteFactory {
    pub path: PathBuf,
    pub dialect: Arc<dyn Dialect>,
    pub opened: Arc<AtomicUsize>,
    pub log: Arc<Mutex<Vec<String>>>,
    pub fail_on: Option<String>,
}

impl SqliteFactory {
    pub fn new(path: &Path, dialect: Arc<dyn Dialect>) -> Arc<Self> {
        Arc::new(SqliteFactory {
            path: path.to_path_buf(),
            dialect,
            opened: Arc::new(AtomicUsize::new(0)),
            log: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        })
    }

    pub fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionFactory for SqliteFactory {
    async fn open(&self) -> DbResult<Box<dyn Session>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let mut s = SqliteSession::open(&self.path, self.dialect.clone());
        s.log = self.log.clone();
        s.fail_on = self.fail_on.clone();
        Ok(Box::new(s))
    }
    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }
}

pub fn generic() -> Arc<dyn Dialect> {
    Arc::new(GenericDialect::new())
}

pub fn sqlite_dialect() -> Arc<dyn Dialect> {
    Arc::new(SqliteTestDialect::new())
}

// ---------------------------------------------------------------------------
// the round trip data set

pub fn nasty_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::new("id", "int8", DataKind::Int),
        ColumnMeta::new("flag", "bool", DataKind::Bool),
        ColumnMeta::new("big", "uint64", DataKind::Int),
        ColumnMeta::new("ratio", "float8", DataKind::Float),
        ColumnMeta::new("amount", "numeric", DataKind::Decimal),
        ColumnMeta::new("note", "text", DataKind::Text),
        ColumnMeta::new("code", "varchar(10)", DataKind::Text),
        ColumnMeta::new("data", "bytea", DataKind::Binary),
        ColumnMeta::new("day", "date", DataKind::Date),
        ColumnMeta::new("at", "time", DataKind::Time),
        ColumnMeta::new("ts", "timestamp", DataKind::DateTime),
        ColumnMeta::new("tstz", "timestamptz", DataKind::DateTimeTz),
        ColumnMeta::new("uid", "uuid", DataKind::Uuid),
        ColumnMeta::new("doc", "jsonb", DataKind::Json),
        ColumnMeta::new("arr", "text[]", DataKind::Array),
    ]
}

fn tz(h: i32, m: i32) -> FixedOffset {
    FixedOffset::east_opt(h * 3600 + h.signum() * m * 60).unwrap()
}

/// Rows covering every value variant and hostile text.
pub fn nasty_rows() -> Vec<Row> {
    let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
    let t = |h, mi, s, us| NaiveTime::from_hms_micro_opt(h, mi, s, us).unwrap();
    let arr = |v: Vec<Value>| Value::Array(v.into_boxed_slice());
    let texts = [
        "plain",
        "",
        "comma, semicolon; pipe | tab\there",
        "quote \" double \"\" and 'single'",
        "line1\nline2\r\nline3\rline4",
        "  padded  ",
        "NULL",
        "back\\slash \\n not a newline",
        "unicode: ünïcödé 日本語 العربية",
        "emoji 😀🎉👍🏽",
        "<tag attr=\"x\">&amp; &lt;</tag>",
        "$dollar {$x} ${y}",
        "ctrl\u{1}char\u{7f}",
        "=1+1",
    ];
    let mut rows = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        let i = i as i64;
        rows.push(vec![
            Value::Int(i + 1),
            Value::Bool(i % 2 == 0),
            match i {
                0 => Value::UInt(u64::MAX),
                1 => Value::Int(i64::MIN),
                2 => Value::Int(i64::MAX),
                _ => Value::Int(-i * 1_000_003),
            },
            match i {
                0 => Value::Float(0.1),
                1 => Value::Float(-2.5e-8),
                2 => Value::Float(1e300),
                3 => Value::Float(f64::NAN),
                4 => Value::Float(f64::INFINITY),
                5 => Value::Float(f64::NEG_INFINITY),
                6 => Value::Float(3.0),
                _ => Value::Float(i as f64 * 1.25),
            },
            match i {
                0 => Value::decimal("12345678901234567890.123456789"),
                1 => Value::decimal("-0.000001"),
                2 => Value::decimal("1.50"),
                3 => Value::decimal("0"),
                4 => Value::decimal("NaN"),
                5 => Value::decimal("-98765432109876543210"),
                _ => Value::decimal(format!("{}.{:02}", i * 7, i)),
            },
            Value::text(*text),
            Value::text(format!("{:03}", i)),
            match i {
                0 => Value::bytes(vec![0u8, 1, 2, 255]),
                1 => Value::bytes(Vec::<u8>::new()),
                _ => Value::bytes((0..(i as u8 * 3)).collect::<Vec<u8>>()),
            },
            Value::Date(d(1900 + i as i32 * 9, 1 + (i as u32 % 12), 1 + (i as u32 * 2) % 28)),
            Value::Time(t(i as u32, 59 - i as u32, 30, if i % 2 == 0 { 123_456 } else { 0 })),
            Value::DateTime(d(2024, 2, 29).and_time(t(23, 59, 59, ((i as u32) * 111_111) % 1_000_000))),
            Value::DateTimeTz(
                d(2020, 6, 15)
                    .and_time(t(12, 0, 0, 500_000))
                    .and_local_timezone(match i % 3 {
                        0 => tz(7, 0),
                        1 => tz(-3, 30),
                        _ => tz(0, 0),
                    })
                    .unwrap(),
            ),
            Value::Uuid(uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef ^ (i as u128) << 64)),
            match i {
                0 => Value::json(r#"{"a":[1,2,{"b":null}],"s":"x\"y","n":12345678901234567890.5}"#),
                1 => Value::json("[]"),
                2 => Value::json(r#"{"unicode":"é😀","nested":{"deep":[true,false]}}"#),
                _ => Value::json(format!("{{\"row\":{i}}}")),
            },
            match i {
                0 => arr(vec![Value::Int(1), Value::Null, Value::text("x y")]),
                1 => arr(vec![]),
                2 => arr(vec![arr(vec![Value::Int(1), Value::Int(2)]), arr(vec![Value::Int(3)])]),
                _ => arr(vec![Value::text("a\"b"), Value::text("c,d")]),
            },
        ]);
    }
    // A row of NULLs.
    rows.push((0..nasty_columns().len()).map(|i| if i == 0 { Value::Int(99) } else { Value::Null }).collect());
    rows
}

/// Value equality treating NaN as equal to NaN and comparing time zone offsets.
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (Value::DateTimeTz(x), Value::DateTimeTz(y)) => x == y && x.offset() == y.offset(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same_value(p, q)),
        _ => a == b,
    }
}

pub fn assert_same_rows(expected: &[Row], actual: &[Row], columns: &[ColumnMeta], what: &str) {
    assert_eq!(expected.len(), actual.len(), "{what}: row count");
    for (r, (e, a)) in expected.iter().zip(actual).enumerate() {
        assert_eq!(e.len(), a.len(), "{what}: width of row {r}");
        for (c, (x, y)) in e.iter().zip(a).enumerate() {
            assert!(
                same_value(x, y),
                "{what}: row {r}, column {} differs:\n expected {x:?}\n   actual {y:?}",
                columns.get(c).map(|m| m.name.as_str()).unwrap_or("?")
            );
        }
    }
}

/// Convert raw imported rows to the kinds of `columns` (what an import mapping does).
pub fn convert_rows(rows: Vec<Row>, columns: &[ColumnMeta], opts: &dbiji_transfer::ConvertOptions) -> Vec<Row> {
    rows.into_iter()
        .map(|r| {
            r.iter()
                .zip(columns)
                .map(|(v, c)| dbiji_transfer::convert_value(v, c.kind, opts).unwrap_or_else(|e| panic!("{}: {e}", c.name)))
                .collect()
        })
        .collect()
}

/// Drain a row source.
pub async fn drain(source: &mut dyn dbiji_transfer::RowSource) -> Vec<Row> {
    let mut out = Vec::new();
    loop {
        let b = source.next_batch(7).await.unwrap();
        if b.is_empty() {
            break;
        }
        out.extend(b);
    }
    out
}

pub fn tabular(rows: Vec<Row>) -> TabularData {
    TabularData { columns: Vec::new(), rows }
}

/// Export `rows` to `path` in `format`.
pub async fn export_to(
    format: dbiji_transfer::ExportFormat,
    opts: &dbiji_transfer::ExportOptions,
    path: &Path,
    columns: Vec<ColumnMeta>,
    rows: Vec<Row>,
    dialect: Option<Arc<dyn Dialect>>,
) -> dbiji_transfer::ExportSummary {
    let mut src = dbiji_transfer::VecSource::new(columns, rows);
    let mut sink = dbiji_transfer::create_sink(format, opts, dbiji_transfer::ExportTarget::path(path), dialect).unwrap();
    dbiji_transfer::run_export(&mut src, sink.as_mut(), &dbiji_transfer::Progress::new(), &dbiji_transfer::CancellationToken::new())
        .await
        .unwrap()
}

/// Export `rows` into a string (text formats).
pub async fn export_string(
    format: dbiji_transfer::ExportFormat,
    opts: &dbiji_transfer::ExportOptions,
    columns: Vec<ColumnMeta>,
    rows: Vec<Row>,
    table: Option<ObjectRef>,
    dialect: Option<Arc<dyn Dialect>>,
) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out");
    let mut src = dbiji_transfer::VecSource::new(columns, rows);
    if let Some(t) = table {
        src = src.with_table(t);
    }
    let mut sink = dbiji_transfer::create_sink(format, opts, dbiji_transfer::ExportTarget::path(&path), dialect).unwrap();
    dbiji_transfer::run_export(&mut src, sink.as_mut(), &dbiji_transfer::Progress::new(), &dbiji_transfer::CancellationToken::new())
        .await
        .unwrap();
    std::fs::read_to_string(&path).unwrap()
}
