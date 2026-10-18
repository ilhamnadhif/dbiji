//! Integration tests against a live SQL Server.
//!
//! Set `DBIJI_TEST_MSSQL=sqlserver://user:password@host:port[/database]`
//! to run them; without it every test returns immediately. Each test creates
//! its own uniquely named database and drops it at the end.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::ddl::*;
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ErrorKind, ExecOptions, ResultHeader, Secret, Session, TransactionMode, Value,
};
use dbiji_driver_mssql::SqlServerDriver;
use std::time::{Duration, Instant};

fn params() -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_MSSQL").ok()?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, hostpart) = rest.rsplit_once('@')?;
    let (user, password) = auth.split_once(':')?;
    let (hostport, db) = match hostpart.split_once('/') {
        Some((h, d)) => (h, Some(d.to_string()).filter(|d| !d.is_empty())),
        None => (hostpart, None),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()),
        None => (hostport.to_string(), None),
    };
    let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "test");
    cfg.host = host;
    cfg.port = port.or(Some(1433));
    cfg.user = Some(user.to_string());
    cfg.database = db.or(Some("master".into()));
    cfg.ssl.mode = SslMode::Require;
    cfg.init.connect_timeout_secs = 30;
    Some(ConnectParams::from_config(&cfg, Some(Secret::new(password))))
}

async fn connect_to(db: Option<&str>) -> Option<Box<dyn Session>> {
    let mut p = params()?;
    if let Some(d) = db {
        p.database = Some(d.to_string());
    }
    Some(SqlServerDriver::new().connect(&p).await.expect("connect"))
}

async fn exec(s: &mut dyn Session, sql: &str) {
    execute_update(s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
}

async fn scalar(s: &mut dyn Session, sql: &str) -> Value {
    query_scalar(s, sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()))
}

/// A uniquely named database, dropped by [`TestDb::drop`].
struct TestDb {
    name: String,
}

impl TestDb {
    async fn create(s: &mut dyn Session) -> TestDb {
        let name = format!("dbiji_it_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        exec(s, &format!("CREATE DATABASE [{name}]")).await;
        TestDb { name }
    }

    async fn drop(self, s: &mut dyn Session) {
        let _ = execute_update(s, "USE master", &[]).await;
        exec(s, &format!("ALTER DATABASE [{}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE", self.name)).await;
        exec(s, &format!("DROP DATABASE [{}]", self.name)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_queries_types_and_params() {
    let Some(mut s) = connect_to(None).await else { return };
    let info = s.server_info().clone();
    assert!(info.product.contains("SQL"), "{info:?}");
    assert!(info.version_number.0 >= 11, "{info:?}");
    assert!(info.session_id.is_some());
    s.ping().await.unwrap();
    let db = TestDb::create(s.as_mut()).await;
    s.set_current_database(&db.name).await.unwrap();
    assert_eq!(s.current_database().await.unwrap().as_deref(), Some(db.name.as_str()));
    assert_eq!(s.current_schema().await.unwrap().as_deref(), Some("dbo"));
    assert_eq!(s.set_current_schema("x").await.unwrap_err().kind, ErrorKind::Unsupported);

    let (cols, rows) = query_all(
        s.as_mut(),
        "SELECT CAST(1 AS tinyint) AS t, CAST(-2 AS smallint) AS sm, 3 AS i, CAST(9223372036854775807 AS bigint) AS bi, \
         CAST(1 AS bit) AS b, CAST(12.345 AS decimal(10,3)) AS d, CAST(922337203685477.5807 AS money) AS m, \
         CAST(1.5 AS real) AS r, CAST(2.25 AS float) AS f, N'héllo' AS nv, CAST('abc' AS varchar(10)) AS v, \
         CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS u, CAST('2024-02-29' AS date) AS dt, \
         CAST('13:14:15.1234567' AS time) AS tm, CAST('2024-01-02T03:04:05.003' AS datetime) AS dtm, \
         CAST('2024-01-02T03:04:05.1234567' AS datetime2) AS dt2, CAST('2024-01-02 10:00:00 +07:00' AS datetimeoffset) AS dto, \
         CAST(0x0102FF AS varbinary(10)) AS vb, CAST('<a>1</a>' AS xml) AS x, CAST(42 AS sql_variant) AS sv, NULL AS n, \
         CAST('2024-01-02 03:04:00' AS smalldatetime) AS sdt, geography::Point(1, 2, 4326) AS geo",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(cols.len(), 23);
    assert_eq!(cols[11].kind, DataKind::Uuid);
    assert_eq!(cols[18].kind, DataKind::Xml);
    let r = &rows[0];
    assert_eq!(r[0], Value::Int(1));
    assert_eq!(r[1], Value::Int(-2));
    assert_eq!(r[3], Value::Int(i64::MAX));
    assert_eq!(r[4], Value::Bool(true));
    assert_eq!(r[5], Value::decimal("12.345"));
    assert_eq!(r[6].as_str().map(|s| s.starts_with("922337203685477.")), Some(true), "{:?}", r[6]);
    assert_eq!(r[7], Value::Float(1.5));
    assert_eq!(r[9], Value::text("héllo"));
    assert_eq!(r[10], Value::text("abc"));
    assert_eq!(r[11].to_text().to_ascii_uppercase(), "6F9619FF-8B86-D011-B42D-00C04FC964FF");
    assert_eq!(r[12].to_text(), "2024-02-29");
    assert_eq!(r[13].to_text(), "13:14:15.1234567");
    assert_eq!(r[14].to_text(), "2024-01-02 03:04:05.003");
    assert_eq!(r[15].to_text(), "2024-01-02 03:04:05.1234567");
    assert_eq!(r[16].to_text(), "2024-01-02 10:00:00+07");
    assert_eq!(r[17], Value::bytes(vec![1u8, 2, 255]));
    assert_eq!(r[18], Value::text("<a>1</a>"));
    assert_eq!(r[19], Value::text("42"));
    assert_eq!(r[20], Value::Null);
    assert_eq!(r[21].to_text(), "2024-01-02 03:04:00");
    assert!(matches!(r[22], Value::Bytes(_)));

    // multiple results and DML counts
    exec(s.as_mut(), "CREATE TABLE dbo.items (id int IDENTITY PRIMARY KEY, name nvarchar(50) NOT NULL, qty int NULL)").await;
    let mut cur = s
        .execute("INSERT INTO dbo.items (name, qty) VALUES (N'a', 1), (N'b', 2), (N'c', 3)", &[], &ExecOptions::default())
        .await
        .unwrap();
    match cur.next_result().await.unwrap() {
        Some(ResultHeader::Update { rows_affected, .. }) => assert_eq!(rows_affected, Some(3)),
        other => panic!("{other:?}"),
    }
    assert!(cur.next_result().await.unwrap().is_none());
    cur.close().await.unwrap();
    drop(cur);
    let n = execute_update(s.as_mut(), "UPDATE dbo.items SET qty = qty + 1 WHERE qty >= @P1", &[Value::Int(2)]).await.unwrap();
    assert_eq!(n, Some(2));
    let mut cur = s.execute("SELECT 1 AS a; SELECT name FROM dbo.items ORDER BY id; UPDATE dbo.items SET qty = 0 WHERE 1 = 0", &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    assert_eq!(cur.fetch(10).await.unwrap(), vec![vec![Value::Int(1)]]);
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    assert_eq!(cur.fetch(10).await.unwrap().len(), 3);
    // batches give no per statement counts
    let last = cur.next_result().await.unwrap();
    assert!(matches!(last, Some(ResultHeader::Update { .. }) | None), "{last:?}");
    cur.close().await.unwrap();
    drop(cur);

    // parameters of every kind round-trip through sp_executesql
    let dt = chrono::NaiveDate::from_ymd_opt(2024, 5, 6).unwrap().and_hms_micro_opt(7, 8, 9, 123_456).unwrap();
    let tz = dt.and_local_timezone(chrono::FixedOffset::east_opt(-5 * 3600).unwrap()).unwrap();
    let u = uuid::Uuid::new_v4();
    let values = vec![
        Value::Int(-7),
        Value::UInt(u64::MAX),
        Value::Float(0.5),
        Value::decimal("-123.4500"),
        Value::Bool(false),
        Value::text("it's ✓"),
        Value::bytes(vec![0u8, 9]),
        Value::Date(dt.date()),
        Value::Time(dt.time()),
        Value::DateTime(dt),
        Value::DateTimeTz(tz),
        Value::Uuid(u),
        Value::Null,
        Value::text("x".repeat(6000)),
    ];
    let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("@P{i}")).collect();
    let (_, rows) = query_all(s.as_mut(), &format!("SELECT {}", placeholders.join(", ")), &values).await.unwrap();
    let r = &rows[0];
    assert_eq!(r[0], Value::Int(-7));
    assert_eq!(r[1], Value::decimal("18446744073709551615"));
    assert_eq!(r[2], Value::Float(0.5));
    assert_eq!(r[3], Value::decimal("-123.4500"));
    assert_eq!(r[4], Value::Bool(false));
    assert_eq!(r[5], Value::text("it's ✓"));
    assert_eq!(r[6], Value::bytes(vec![0u8, 9]));
    assert_eq!(r[7], Value::Date(dt.date()));
    assert_eq!(r[8], Value::Time(dt.time()));
    assert_eq!(r[9], Value::DateTime(dt));
    assert_eq!(r[10], Value::DateTimeTz(tz));
    assert_eq!(r[11], Value::Uuid(u));
    assert_eq!(r[12], Value::Null);
    assert_eq!(r[13].as_str().map(str::len), Some(6000));

    // literals of the dialect are accepted by the server
    let d = s.dialect();
    let lits: Vec<String> = values[..12].iter().map(|v| d.literal(v, None)).collect();
    let (_, rows) = query_all(s.as_mut(), &format!("SELECT {}", lits.join(", ")), &[]).await.unwrap();
    assert_eq!(rows[0][5], Value::text("it's ✓"));
    assert_eq!(rows[0][9], Value::DateTime(dt));

    // session state persists: temp tables, SET options, GO batches
    exec(s.as_mut(), "CREATE TABLE #tmp (a int); INSERT INTO #tmp VALUES (1), (2)").await;
    assert_eq!(scalar(s.as_mut(), "SELECT COUNT(*) FROM #tmp").await.as_i64(), Some(2));
    let n = execute_update(s.as_mut(), "INSERT INTO #tmp VALUES (3)", &[]).await.unwrap();
    assert_eq!(n, Some(1));
    let (_, rows) = query_all(s.as_mut(), "SELECT 1 AS a\nGO\nSELECT COUNT(*) FROM #tmp\nGO 2", &[]).await.unwrap();
    assert_eq!(rows, vec![vec![Value::Int(1)]]);

    // errors carry number, state, line and a position
    let err = query_all(s.as_mut(), "SELECT 1\nSELEC 2", &[]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert_eq!(err.code.as_deref(), Some("102"));
    assert_eq!(err.context.as_deref(), Some("Line 2"));
    assert_eq!(err.position, Some(10));
    let err = query_all(s.as_mut(), "SELECT * FROM dbo.nope", &[]).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("208"));
    assert!(err.detail.as_deref().unwrap_or("").starts_with("Msg 208"), "{err:?}");
    let err = execute_update(s.as_mut(), "INSERT INTO dbo.items (name) VALUES (NULL)", &[]).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some("515"));
    s.ping().await.unwrap();

    // resolve_sources: base tables and nullability
    let mut cur = s
        .execute("SELECT id, name, qty, 1 AS k FROM dbo.items", &[], &ExecOptions { resolve_sources: true, ..Default::default() })
        .await
        .unwrap();
    let Some(ResultHeader::Rows(cols)) = cur.next_result().await.unwrap() else { panic!("rows") };
    cur.close().await.unwrap();
    drop(cur);
    assert_eq!(cols[0].auto_increment, Some(true));
    assert_eq!(cols[1].type_name, "nvarchar(50)");
    assert_eq!(cols[1].nullable, Some(false));
    assert_eq!(cols[1].max_length, Some(50));
    let src = cols[1].source.as_ref().expect("source");
    assert_eq!((src.schema.as_deref(), src.table.as_str(), src.column.as_str()), (Some("dbo"), "items", "name"));
    assert!(cols[3].source.is_none());

    db.drop(s.as_mut()).await;
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn transactions() {
    let Some(mut s) = connect_to(None).await else { return };
    let db = TestDb::create(s.as_mut()).await;
    s.set_current_database(&db.name).await.unwrap();
    exec(s.as_mut(), "CREATE TABLE dbo.t (id int PRIMARY KEY, v nvarchar(10))").await;
    assert!(!s.in_transaction());
    s.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    assert_eq!(s.transaction_mode(), TransactionMode::Manual);
    // DML through sp_executesql inside an implicit transaction
    exec(s.as_mut(), "INSERT INTO dbo.t VALUES (1, N'a')").await;
    assert!(s.in_transaction());
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    assert_eq!(scalar(s.as_mut(), "SELECT COUNT(*) FROM dbo.t").await.as_i64(), Some(0));
    execute_update(s.as_mut(), "INSERT INTO dbo.t VALUES (@P1, @P2)", &[Value::Int(2), Value::text("b")]).await.unwrap();
    assert!(s.in_transaction());
    s.commit().await.unwrap();
    assert!(!s.in_transaction());
    // switching to auto commits an open transaction
    exec(s.as_mut(), "INSERT INTO dbo.t VALUES (3, N'c')").await;
    assert!(s.in_transaction());
    s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert!(!s.in_transaction());
    let mut other = connect_to(Some(&db.name)).await.unwrap();
    assert_eq!(scalar(other.as_mut(), "SELECT COUNT(*) FROM dbo.t").await.as_i64(), Some(2));
    // explicit transactions in auto mode are tracked too
    exec(s.as_mut(), "BEGIN TRANSACTION").await;
    assert!(s.in_transaction());
    exec(s.as_mut(), "DELETE FROM dbo.t").await;
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    assert_eq!(scalar(s.as_mut(), "SELECT COUNT(*) FROM dbo.t").await.as_i64(), Some(2));
    drop(other);
    db.drop(s.as_mut()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_timeout_and_streaming() {
    let Some(mut s) = connect_to(None).await else { return };
    // User cancel: the attention signal stops the statement on the server.
    // tiberius cannot read SQL Server's separate acknowledgement, so the
    // connection is replaced and the next statement carries a warning.
    let handle = s.cancel_handle().unwrap();
    let mut cur = s.execute("WAITFOR DELAY '00:00:30'; SELECT 1", &[], &ExecOptions::default()).await.unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        handle.cancel().await
    });
    let start = Instant::now();
    let err = cur.next_result().await.unwrap_err();
    canceller.await.unwrap().unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(20));
    cur.close().await.unwrap();
    drop(cur);
    let mut cur = s.execute("SELECT 1", &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    assert_eq!(cur.fetch(10).await.unwrap(), vec![vec![Value::Int(1)]]);
    let msgs = cur.take_messages();
    assert!(msgs.iter().any(|m| m.severity == "WARNING" && m.message.contains("re-established")), "{msgs:?}");
    cur.close().await.unwrap();
    drop(cur);

    // statement timeout behaves the same way
    let opts = ExecOptions { timeout: Some(Duration::from_millis(1500)), ..Default::default() };
    let mut cur = s.execute("WAITFOR DELAY '00:00:30'", &[], &opts).await.unwrap();
    let err = cur.next_result().await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    cur.close().await.unwrap();
    drop(cur);
    s.ping().await.unwrap();

    // streaming a large result in pages
    let sql = "WITH n AS (SELECT TOP (300000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i \
               FROM sys.all_columns a CROSS JOIN sys.all_columns b) SELECT i, REPLICATE(N'x', 20) AS pad FROM n";
    let mut cur = s.execute(sql, &[], &ExecOptions::default()).await.unwrap();
    assert!(matches!(cur.next_result().await.unwrap(), Some(ResultHeader::Rows(_))));
    let mut total = 0usize;
    let mut last = 0i64;
    loop {
        let b = cur.fetch(5000).await.unwrap();
        if b.is_empty() {
            break;
        }
        for r in &b {
            let i = r[0].as_i64().unwrap();
            assert_eq!(i, last + 1);
            last = i;
        }
        total += b.len();
    }
    assert_eq!(total, 300_000);
    cur.close().await.unwrap();
    drop(cur);

    // dropping a cursor in the middle of a result drains the rest: the
    // connection and its session state survive
    exec(s.as_mut(), "CREATE TABLE #keep (a int); INSERT INTO #keep VALUES (42)").await;
    let small = "SELECT TOP (20000) a.object_id FROM sys.all_columns a CROSS JOIN sys.all_columns b";
    let mut cur = s.execute(small, &[], &ExecOptions::default()).await.unwrap();
    cur.next_result().await.unwrap();
    assert_eq!(cur.fetch(10).await.unwrap().len(), 10);
    drop(cur);
    let start = Instant::now();
    assert_eq!(scalar(s.as_mut(), "SELECT a FROM #keep").await.as_i64(), Some(42));
    assert!(start.elapsed() < Duration::from_secs(20));
    // a huge abandoned result is cancelled instead (bounded wait)
    let mut cur = s.execute(sql, &[], &ExecOptions::default()).await.unwrap();
    cur.next_result().await.unwrap();
    assert_eq!(cur.fetch(10).await.unwrap().len(), 10);
    let start = Instant::now();
    cur.close().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(20));
    drop(cur);
    s.ping().await.unwrap();

    // a second cursor while one is open is refused instead of deadlocking
    let mut cur = s.execute(sql, &[], &ExecOptions::default()).await.unwrap();
    cur.next_result().await.unwrap();
    let busy = s.execute("SELECT 1", &[], &ExecOptions::default()).await;
    assert!(busy.is_err());
    cur.close().await.unwrap();
    drop(cur);
    s.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_and_ddl_round_trip() {
    let Some(mut s) = connect_to(None).await else { return };
    let db = TestDb::create(s.as_mut()).await;
    let dbn = db.name.clone();
    s.set_current_database(&dbn).await.unwrap();
    let script = r#"
CREATE SCHEMA sales
GO
CREATE TYPE sales.Phone FROM nvarchar(20) NOT NULL
GO
CREATE TABLE sales.customers (
    id int IDENTITY(10, 5) NOT NULL CONSTRAINT PK_customers PRIMARY KEY CLUSTERED,
    email nvarchar(200) NOT NULL CONSTRAINT UQ_customers_email UNIQUE,
    phone sales.Phone NULL,
    created datetime2(3) NOT NULL CONSTRAINT DF_customers_created DEFAULT (sysdatetime())
)
GO
CREATE TABLE sales.orders (
    id bigint NOT NULL,
    customer_id int NOT NULL,
    qty int NOT NULL CONSTRAINT DF_orders_qty DEFAULT ((1)),
    price decimal(10, 2) NOT NULL,
    total AS (qty * price) PERSISTED,
    note nvarchar(max) NULL,
    CONSTRAINT PK_orders PRIMARY KEY NONCLUSTERED (id),
    CONSTRAINT CK_orders_qty CHECK (qty > 0),
    CONSTRAINT FK_orders_customers FOREIGN KEY (customer_id) REFERENCES sales.customers (id) ON DELETE CASCADE
)
GO
CREATE CLUSTERED INDEX CIX_orders_customer ON sales.orders (customer_id, id DESC)
GO
CREATE NONCLUSTERED INDEX IX_orders_price ON sales.orders (price) INCLUDE (qty) WHERE price > 0
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Customer orders', @level0type = N'SCHEMA', @level0name = N'sales', @level1type = N'TABLE', @level1name = N'orders'
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Unit price', @level0type = N'SCHEMA', @level0name = N'sales', @level1type = N'TABLE', @level1name = N'orders', @level2type = N'COLUMN', @level2name = N'price'
GO
CREATE TRIGGER sales.trg_orders ON sales.orders AFTER INSERT, UPDATE AS BEGIN SET NOCOUNT ON; END
GO
CREATE VIEW sales.v_orders WITH SCHEMABINDING AS SELECT o.id, o.total FROM sales.orders o
GO
CREATE PROCEDURE sales.get_orders @customer int, @n int = 10 OUTPUT AS SELECT TOP (@n) * FROM sales.orders WHERE customer_id = @customer
GO
CREATE FUNCTION sales.order_total(@id bigint) RETURNS decimal(21,2) AS BEGIN RETURN (SELECT total FROM sales.orders WHERE id = @id) END
GO
CREATE SEQUENCE sales.order_seq AS bigint START WITH 100 INCREMENT BY 10 CACHE 20
GO
CREATE SYNONYM sales.o FOR sales.orders
"#;
    exec(s.as_mut(), script).await;
    // read metadata from master to exercise cross-database catalog access
    s.set_current_database("master").await.unwrap();

    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.iter().any(|d| d.name == dbn && !d.is_system));
    assert!(dbs.iter().any(|d| d.name == "master" && d.is_system && d.is_current));
    let sch = SchemaRef::new(Some(dbn.clone()), Some("sales".into()));
    let tables = s.list_objects(&sch, ObjectKind::Table).await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.obj.name.as_str()).collect();
    assert_eq!(names, vec!["customers", "orders"]);
    assert_eq!(tables[1].comment.as_deref(), Some("Customer orders"));
    assert_eq!(s.list_objects(&sch, ObjectKind::View).await.unwrap().len(), 1);
    let procs = s.list_objects(&sch, ObjectKind::Procedure).await.unwrap();
    assert!(procs[0].detail.as_deref().unwrap().contains("@customer int"), "{procs:?}");
    assert_eq!(s.list_objects(&sch, ObjectKind::Function).await.unwrap().len(), 1);
    assert_eq!(s.list_objects(&sch, ObjectKind::Sequence).await.unwrap().len(), 1);
    assert_eq!(s.list_objects(&sch, ObjectKind::Synonym).await.unwrap()[0].detail.as_deref(), Some("[sales].[orders]"));
    assert_eq!(s.list_objects(&sch, ObjectKind::Type).await.unwrap()[0].detail.as_deref(), Some("alias of nvarchar(20)"));

    let orders = ObjectRef::table(Some("sales".into()), "orders").with_database(Some(dbn.clone()));
    let cols = s.list_children(&orders, ObjectKind::Column).await.unwrap();
    assert_eq!(cols.len(), 6);
    assert_eq!(cols[3].detail.as_deref(), Some("decimal(10,2) NOT NULL"));
    assert_eq!(cols[3].comment.as_deref(), Some("Unit price"));
    let idx = s.list_children(&orders, ObjectKind::Index).await.unwrap();
    assert_eq!(idx.len(), 3, "{idx:?}");
    assert!(idx.iter().any(|i| i.detail.as_deref().unwrap_or("").contains("INCLUDE ([qty]) WHERE [price]>(0)")), "{idx:?}");
    assert_eq!(s.list_children(&orders, ObjectKind::PrimaryKey).await.unwrap().len(), 1);
    assert_eq!(s.list_children(&orders, ObjectKind::ForeignKey).await.unwrap().len(), 1);
    assert_eq!(s.list_children(&orders, ObjectKind::CheckConstraint).await.unwrap()[0].detail.as_deref(), Some("[qty]>(0)"));
    let trg = s.list_children(&orders, ObjectKind::Trigger).await.unwrap();
    assert!(trg[0].detail.as_deref().unwrap().starts_with("AFTER"), "{trg:?}");

    let t = s.table_definition(&orders).await.unwrap();
    assert_eq!(t.columns.len(), 6);
    assert_eq!(t.columns[2].default.as_deref(), Some("1"));
    assert_eq!(t.columns[4].generated.as_deref(), Some("[qty]*[price] PERSISTED"));
    assert_eq!(t.columns[5].data_type, "nvarchar(max)");
    assert_eq!(t.primary_key.as_ref().unwrap().name.as_deref(), Some("PK_orders"));
    assert_eq!(t.checks[0].expression, "[qty]>(0)");
    assert_eq!(t.foreign_keys[0].on_delete, FkAction::Cascade);
    assert_eq!(t.foreign_keys[0].ref_table, "customers");
    assert_eq!(t.triggers.len(), 1);
    assert!(t.triggers[0].definition.starts_with("CREATE TRIGGER"));
    assert_eq!(t.comment.as_deref(), Some("Customer orders"));
    assert_eq!(t.options.get("default_constraint:qty").map(String::as_str), Some("DF_orders_qty"));
    let customers = s.table_definition(&ObjectRef::table(Some("sales".into()), "customers").with_database(Some(dbn.clone()))).await.unwrap();
    assert!(customers.columns[0].auto_increment);
    assert_eq!(customers.options.get("identity:id").map(String::as_str), Some("10,5"));
    assert_eq!(customers.columns[2].data_type, "[sales].[Phone]");
    assert_eq!(customers.unique_keys.len(), 1);
    assert_eq!(customers.referenced_by.len(), 1);

    let ddl = s.object_ddl(&orders).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE sales.orders ("), "{ddl}");
    assert!(ddl.contains("PRIMARY KEY NONCLUSTERED"), "{ddl}");
    assert!(ddl.contains("CREATE CLUSTERED INDEX [CIX_orders_customer]"), "{ddl}");
    assert!(ddl.contains("\nGO\n"));
    let vref = ObjectRef::new(ObjectKind::View, Some("sales".into()), "v_orders").with_database(Some(dbn.clone()));
    let v = s.view_definition(&vref).await.unwrap();
    assert!(v.definition.starts_with("SELECT o.id"), "{v:?}");
    assert_eq!(v.columns.len(), 2);
    let r = s
        .routine_definition(&ObjectRef::new(ObjectKind::Procedure, Some("sales".into()), "get_orders").with_database(Some(dbn.clone())))
        .await
        .unwrap();
    assert_eq!(r.arguments, "@customer int, @n int OUTPUT");
    let f = s
        .routine_definition(&ObjectRef::new(ObjectKind::Function, Some("sales".into()), "order_total").with_database(Some(dbn.clone())))
        .await
        .unwrap();
    assert_eq!(f.returns.as_deref(), Some("decimal(21,2)"));
    let seq = s
        .sequence_definition(&ObjectRef::new(ObjectKind::Sequence, Some("sales".into()), "order_seq").with_database(Some(dbn.clone())))
        .await
        .unwrap();
    assert_eq!((seq.start, seq.increment, seq.cache), (Some(100), Some(10), Some(20)));
    let deps = s.object_dependencies(&orders).await.unwrap();
    assert!(deps.iter().any(|d| d.target.name == "v_orders" && d.direction == DependencyDirection::UsedBy), "{deps:?}");
    assert!(deps.iter().any(|d| d.target.name == "customers" && d.direction == DependencyDirection::DependsOn), "{deps:?}");
    assert!(!s.object_properties(&orders).await.unwrap().is_empty());
    assert!(s.object_statistics(&orders).await.unwrap().iter().any(|p| p.name == "Rows"));

    // snapshot (batched)
    let snap = s.schema_snapshot(&sch, &SnapshotOptions::full()).await.unwrap();
    assert_eq!(snap.tables.len(), 2);
    assert_eq!(snap.views.len(), 1);
    assert_eq!(snap.routines.len(), 2);
    assert_eq!(snap.sequences.len(), 1);
    assert_eq!(snap.types.len(), 1);

    // search (current database)
    s.set_current_database(&dbn).await.unwrap();
    let found = s
        .search_metadata(&MetadataSearch { pattern: "order".into(), kinds: vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Procedure, ObjectKind::Column], ..Default::default() })
        .await
        .unwrap();
    assert!(found.iter().any(|o| o.obj.kind == ObjectKind::Table && o.obj.name == "orders"));
    assert!(found.iter().any(|o| o.obj.kind == ObjectKind::View && o.obj.name == "v_orders"));
    let found = s
        .search_metadata(&MetadataSearch { pattern: "pri*".into(), kinds: vec![ObjectKind::Column], schemas: vec!["sales".into()], ..Default::default() })
        .await
        .unwrap();
    assert!(found.iter().any(|o| o.obj.name == "price"), "{found:?}");

    // DDL round trip: recreate orders in another schema from its definition
    exec(s.as_mut(), "CREATE SCHEMA copy").await;
    let dialect = s.dialect();
    let mut copy = t.clone();
    copy.schema = Some("copy".into());
    copy.triggers.clear();
    copy.foreign_keys.clear();
    copy.options.insert("default_constraint:qty".into(), "DF_copy_orders_qty".into());
    copy.primary_key.as_mut().unwrap().name = Some("PK_copy_orders".into());
    copy.checks[0].name = Some("CK_copy_orders_qty".into());
    for i in &mut copy.indexes {
        if i.primary {
            i.name = "PK_copy_orders".into();
        } else {
            i.name = format!("copy_{}", i.name);
        }
    }
    for stmt in dialect.ddl().create_table(&copy, false) {
        exec(s.as_mut(), &stmt).await;
    }
    let copy_ref = ObjectRef::table(Some("copy".into()), "orders");
    let back = s.table_definition(&copy_ref).await.unwrap();
    assert_eq!(back.columns, copy.columns);
    assert_eq!(back.checks, copy.checks);
    assert_eq!(back.indexes.len(), 3);
    assert_eq!(back.comment, copy.comment);

    // alter: change type of an indexed column, rename, add, drop
    let mut to = back.clone();
    to.columns[3].data_type = "decimal(12,2)".into();
    to.columns[5].name = "remark".into();
    to.columns.push(ColumnDef::new("flag", "bit").not_null().default_value("0"));
    to.comment = Some("Copied orders".into());
    let stmts = dialect.ddl().alter_table(&back, &to, &[("note".into(), "remark".into())]);
    for stmt in &stmts {
        exec(s.as_mut(), stmt).await;
    }
    let after = s.table_definition(&copy_ref).await.unwrap();
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    // the computed column depending on price is re-created at the end
    assert_eq!(names, vec!["id", "customer_id", "qty", "price", "remark", "total", "flag"], "{stmts:#?}");
    assert_eq!(after.columns[3].data_type, "decimal(12,2)");
    assert_eq!(after.columns[5].generated.as_deref(), Some("[qty]*[price] PERSISTED"));
    assert_eq!(after.columns[6].default.as_deref(), Some("0"));
    assert_eq!(after.indexes.len(), 3, "index on price was recreated: {stmts:#?}");
    assert_eq!(after.comment.as_deref(), Some("Copied orders"));

    db.drop(s.as_mut()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_plans() {
    let Some(mut s) = connect_to(None).await else { return };
    let db = TestDb::create(s.as_mut()).await;
    s.set_current_database(&db.name).await.unwrap();
    exec(s.as_mut(), "CREATE TABLE dbo.p (id int PRIMARY KEY, v int); INSERT INTO dbo.p SELECT TOP (500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), 1 FROM sys.all_columns").await;
    let plan = s.explain("SELECT p1.id FROM dbo.p p1 JOIN dbo.p p2 ON p2.id = p1.v WHERE p1.id > 10", false).await.unwrap();
    assert!(!plan.analyzed);
    assert_eq!(plan.roots.len(), 1);
    assert_eq!(plan.roots[0].node_type, "SELECT");
    assert!(plan.roots[0].count() >= 3, "{plan:?}");
    let mut relations = Vec::new();
    plan.roots[0].walk(0, &mut |n, _| {
        if let Some(r) = &n.relation {
            relations.push(r.clone());
        }
    });
    assert!(relations.iter().any(|r| r == "dbo.p"), "{relations:?}");
    assert!(plan.raw.as_deref().unwrap_or("").contains("ShowPlanXML"));
    // the session is back to normal execution afterwards
    assert_eq!(scalar(s.as_mut(), "SELECT COUNT(*) FROM dbo.p").await.as_i64(), Some(500));

    let plan = s.explain("SELECT COUNT(*) FROM dbo.p WHERE v = 1", true).await.unwrap();
    assert!(plan.analyzed);
    let mut actual = None;
    plan.roots[0].walk(0, &mut |n, _| {
        if n.relation.is_some() && actual.is_none() {
            actual = n.actual_rows;
        }
    });
    assert_eq!(actual, Some(500.0), "{plan:?}");
    assert_eq!(scalar(s.as_mut(), "SELECT 1").await.as_i64(), Some(1));
    db.drop(s.as_mut()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_locks_metrics() {
    let Some(mut s) = connect_to(None).await else { return };
    let db = TestDb::create(s.as_mut()).await;
    s.set_current_database(&db.name).await.unwrap();
    exec(s.as_mut(), "CREATE TABLE dbo.l (id int PRIMARY KEY, v int); INSERT INTO dbo.l VALUES (1, 1)").await;

    let mut holder = connect_to(Some(&db.name)).await.unwrap();
    exec(holder.as_mut(), "BEGIN TRANSACTION; UPDATE dbo.l SET v = 2 WHERE id = 1").await;
    let holder_spid = holder.server_info().session_id.clone().unwrap();

    let mut waiter = connect_to(Some(&db.name)).await.unwrap();
    let waiter_spid = waiter.server_info().session_id.clone().unwrap();
    let mut blocked = waiter.execute("UPDATE dbo.l SET v = 3 WHERE id = 1", &[], &ExecOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let sessions = s.list_sessions().await.unwrap();
    let id = sessions.id_column;
    let blocked_col = sessions.blocked_by_column.unwrap();
    let row = sessions.data.rows.iter().find(|r| r[id].to_text() == waiter_spid).expect("waiter listed");
    assert_eq!(row[blocked_col].to_text(), holder_spid, "{row:?}");

    let locks = s.list_locks().await.unwrap();
    let granted = locks.granted_column.unwrap();
    let blocker = locks.blocked_by_column.unwrap();
    assert!(locks.data.rows.iter().any(|r| r[locks.session_column].to_text() == holder_spid && r[granted] == Value::Bool(true)));
    assert!(locks.data.rows.iter().any(|r| r[locks.session_column].to_text() == waiter_spid && r[blocker].to_text() == holder_spid), "{:?}", locks.data.rows);

    // terminate the blocked session
    assert_eq!(s.kill_session(&waiter_spid, dbiji_core::admin::KillMode::CancelQuery).await.unwrap_err().kind, ErrorKind::Unsupported);
    s.kill_session(&waiter_spid, dbiji_core::admin::KillMode::Terminate).await.unwrap();
    let mut saw_error = None;
    loop {
        match blocked.next_result().await {
            Ok(Some(ResultHeader::Rows(_))) => while !blocked.fetch(100).await.unwrap_or_default().is_empty() {},
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                saw_error = Some(e);
                break;
            }
        }
    }
    let err = saw_error.expect("the killed statement reports an error");
    assert!(err.is_connection_lost() || err.kind == ErrorKind::Query, "{err:?}");
    drop(blocked);
    // the killed session reconnects transparently
    assert_eq!(scalar(waiter.as_mut(), "SELECT 1").await.as_i64(), Some(1));
    holder.rollback().await.unwrap();

    let metrics = s.server_metrics().await.unwrap();
    assert!(metrics.iter().any(|m| m.key == "connections.user" && m.value >= 2.0), "{metrics:?}");
    assert!(metrics.iter().any(|m| m.key == "batch_requests"));
    drop(waiter);
    drop(holder);
    db.drop(s.as_mut()).await;
}
