//! Integration tests against a live Oracle Database.
//!
//! Set `DBIJI_TEST_ORACLE=oracle://user:password@host:port/service` (for
//! example `oracle://dbiji:dbiji@127.0.0.1:51521/FREEPDB1` with the
//! `gvenzl/oracle-free` image and `APP_USER=dbiji`) to run them; without it
//! every test returns immediately. Each test works in tables of its own.

use dbiji_core::config::{ConnectionConfig, SslMode};
use dbiji_core::ddl::SnapshotOptions;
use dbiji_core::driver::{execute_update, query_all, query_scalar};
use dbiji_core::meta::*;
use dbiji_core::value::DataKind;
use dbiji_core::{
    ConnectParams, DbKind, Driver, ErrorKind, ExecOptions, ResultHeader, Secret, Session,
    TransactionMode, Value,
};
use dbiji_driver_oracle::OracleDriver;

fn params() -> Option<ConnectParams> {
    let url = std::env::var("DBIJI_TEST_ORACLE")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, hostpart) = rest.rsplit_once('@')?;
    let (user, password) = auth.split_once(':')?;
    let (hostport, service) = hostpart.split_once('/').unwrap_or((hostpart, "FREEPDB1"));
    let (host, port) = hostport
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse().ok()))
        .unwrap_or((hostport.to_string(), None));
    let mut cfg = ConnectionConfig::new(DbKind::Oracle, "test");
    cfg.host = host;
    cfg.port = port.or(Some(1521));
    cfg.user = Some(user.to_string());
    cfg.database = Some(service.to_string());
    cfg.ssl.mode = SslMode::Disable;
    cfg.init.connect_timeout_secs = 30;
    Some(ConnectParams::from_config(
        &cfg,
        Some(Secret::new(password)),
    ))
}

async fn connect() -> Option<Box<dyn Session>> {
    let p = params()?;
    Some(OracleDriver::new().connect(&p).await.expect("connect"))
}

async fn exec(s: &mut dyn Session, sql: &str) {
    execute_update(s, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
}

async fn drop_table(s: &mut dyn Session, name: &str) {
    let _ = execute_update(
        s,
        &format!("DROP TABLE {name} CASCADE CONSTRAINTS PURGE"),
        &[],
    )
    .await;
}

#[tokio::test]
async fn connects_and_reads_every_kind_of_value() {
    let Some(mut s) = connect().await else { return };
    let info = s.server_info().clone();
    assert_eq!(info.product, "Oracle Database");
    assert!(info.version_number.0 >= 12, "{info:?}");
    let (cols, rows) = query_all(
        s.as_mut(),
        "SELECT 1 AS n, 2.5 AS d, CAST(7 AS NUMBER(5)) AS i, 'x' AS c, DATE '2024-01-02' AS dt, TIMESTAMP '2024-01-02 03:04:05.5' AS ts, \
         TO_CLOB('long text') AS cl, HEXTORAW('ABCD') AS r, CAST(NULL AS VARCHAR2(5)) AS nul, INTERVAL '1 2:03:04' DAY TO SECOND AS iv FROM dual",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["N", "D", "I", "C", "DT", "TS", "CL", "R", "NUL", "IV"]
    );
    assert_eq!(cols[2].kind, DataKind::Int);
    assert_eq!(cols[4].kind, DataKind::DateTime);
    let r = &rows[0];
    assert_eq!(r[0], Value::Int(1));
    assert_eq!(r[1], Value::Decimal("2.5".into()));
    assert_eq!(r[2], Value::Int(7));
    assert_eq!(r[3], Value::Text("x".into()));
    assert_eq!(r[4].to_text(), "2024-01-02 00:00:00");
    assert_eq!(r[5].to_text(), "2024-01-02 03:04:05.5");
    assert_eq!(r[6], Value::Text("long text".into()));
    assert_eq!(r[7], Value::Bytes(vec![0xab, 0xcd].into()));
    assert_eq!(r[8], Value::Null);
    assert_eq!(r[9], Value::Text("+1 02:03:04".into()));
    // Bind parameters (:1, :2 ...) of every kind.
    let (_, rows) = query_all(
        s.as_mut(),
        "SELECT :1 + 1, :2 || '!', :3, :4 FROM dual",
        &[
            Value::Int(41),
            Value::Text("hi".into()),
            Value::Date(chrono::NaiveDate::from_ymd_opt(2024, 5, 6).unwrap()),
            Value::Null,
        ],
    )
    .await
    .unwrap();
    assert_eq!(rows[0][0], Value::Int(42));
    assert_eq!(rows[0][1], Value::Text("hi!".into()));
    assert_eq!(rows[0][2].to_text(), "2024-05-06 00:00:00");
    assert_eq!(rows[0][3], Value::Null);
    s.close().await.unwrap();
}

#[tokio::test]
async fn errors_carry_code_and_position() {
    let Some(mut s) = connect().await else { return };
    let e = query_all(s.as_mut(), "SELECT * FROM no_such_table_xyz", &[])
        .await
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("ORA-00942"));
    assert_eq!(e.position, Some(15));
    let e = query_all(s.as_mut(), "SELEC 1 FROM dual", &[])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Syntax, "{e:?}");
    // The session still works.
    assert_eq!(
        query_scalar(s.as_mut(), "SELECT 1 FROM dual;", &[])
            .await
            .unwrap(),
        Value::Int(1)
    );
}

#[tokio::test]
async fn transactions_auto_and_manual() {
    let Some(mut s) = connect().await else { return };
    drop_table(s.as_mut(), "dbiji_tx").await;
    exec(
        s.as_mut(),
        "CREATE TABLE dbiji_tx (id NUMBER(10) PRIMARY KEY, name VARCHAR2(50))",
    )
    .await;
    // Auto-commit: another session sees the row at once.
    exec(s.as_mut(), "INSERT INTO dbiji_tx VALUES (1, 'a')").await;
    let mut other = connect().await.unwrap();
    assert_eq!(
        query_scalar(other.as_mut(), "SELECT COUNT(*) FROM dbiji_tx", &[])
            .await
            .unwrap(),
        Value::Int(1)
    );
    // Manual: invisible until commit, gone after rollback.
    s.set_transaction_mode(TransactionMode::Manual)
        .await
        .unwrap();
    exec(s.as_mut(), "INSERT INTO dbiji_tx VALUES (2, 'b')").await;
    assert!(s.in_transaction());
    assert_eq!(
        query_scalar(other.as_mut(), "SELECT COUNT(*) FROM dbiji_tx", &[])
            .await
            .unwrap(),
        Value::Int(1)
    );
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    exec(s.as_mut(), "INSERT INTO dbiji_tx VALUES (3, 'c')").await;
    s.commit().await.unwrap();
    assert_eq!(
        query_scalar(other.as_mut(), "SELECT COUNT(*) FROM dbiji_tx", &[])
            .await
            .unwrap(),
        Value::Int(2)
    );
    // Savepoints as used by the grid when saving changes in a transaction.
    exec(s.as_mut(), "INSERT INTO dbiji_tx VALUES (4, 'd')").await;
    exec(s.as_mut(), "SAVEPOINT dbiji_save").await;
    exec(s.as_mut(), "INSERT INTO dbiji_tx VALUES (5, 'e')").await;
    exec(s.as_mut(), "ROLLBACK TO SAVEPOINT dbiji_save").await;
    s.commit().await.unwrap();
    assert_eq!(
        query_scalar(other.as_mut(), "SELECT COUNT(*) FROM dbiji_tx", &[])
            .await
            .unwrap(),
        Value::Int(3)
    );
    s.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    drop_table(s.as_mut(), "dbiji_tx").await;
}

#[tokio::test]
async fn plsql_blocks_output_and_compile_errors() {
    let Some(mut s) = connect().await else { return };
    let mut c = s
        .execute(
            "BEGIN DBMS_OUTPUT.PUT_LINE('hello'); DBMS_OUTPUT.PUT_LINE('world'); END",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    while c.next_result().await.unwrap().is_some() {}
    let lines: Vec<String> = c.take_messages().into_iter().map(|m| m.message).collect();
    assert_eq!(lines, ["hello", "world"]);
    // A procedure with an error: created with a warning, its errors listed.
    let mut c = s
        .execute(
            "CREATE OR REPLACE PROCEDURE dbiji_bad AS BEGIN no_such_proc; END;\n/",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    while c.next_result().await.unwrap().is_some() {}
    let messages = c.take_messages();
    assert!(
        messages
            .iter()
            .any(|m| m.message.contains("PLS-00201") || m.message.contains("NO_SUCH_PROC")),
        "{messages:?}"
    );
    exec(s.as_mut(), "CREATE OR REPLACE FUNCTION dbiji_add(a NUMBER, b NUMBER) RETURN NUMBER AS BEGIN RETURN a + b; END;").await;
    assert_eq!(
        query_scalar(s.as_mut(), "SELECT dbiji_add(2, 3) FROM dual", &[])
            .await
            .unwrap(),
        Value::Int(5)
    );
    let def = s
        .routine_definition(&ObjectRef::new(
            ObjectKind::Function,
            Some("DBIJI".into()),
            "DBIJI_ADD",
        ))
        .await
        .unwrap();
    assert!(
        def.definition
            .starts_with("CREATE OR REPLACE FUNCTION dbiji_add"),
        "{}",
        def.definition
    );
    assert_eq!(def.returns.as_deref(), Some("NUMBER"));
    exec(s.as_mut(), "DROP PROCEDURE dbiji_bad").await;
    exec(s.as_mut(), "DROP FUNCTION dbiji_add").await;
}

#[tokio::test]
async fn catalog_and_table_definitions() {
    let Some(mut s) = connect().await else { return };
    drop_table(s.as_mut(), "dbiji_child").await;
    drop_table(s.as_mut(), "dbiji_parent").await;
    exec(s.as_mut(), "CREATE TABLE dbiji_parent (id NUMBER(10) GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, code VARCHAR2(20 CHAR) NOT NULL UNIQUE, price NUMBER(10,2) DEFAULT 0 CHECK (price >= 0))").await;
    exec(s.as_mut(), "CREATE TABLE dbiji_child (id NUMBER(10) PRIMARY KEY, parent_id NUMBER(10) REFERENCES dbiji_parent(id) ON DELETE CASCADE, note CLOB, created DATE DEFAULT SYSDATE)").await;
    exec(
        s.as_mut(),
        "CREATE INDEX dbiji_child_parent ON dbiji_child (parent_id, created DESC)",
    )
    .await;
    exec(s.as_mut(), "COMMENT ON TABLE dbiji_parent IS 'Parents'").await;
    exec(
        s.as_mut(),
        "COMMENT ON COLUMN dbiji_parent.code IS 'Unique code'",
    )
    .await;

    let schemas = s.list_schemas().await.unwrap();
    let me = schemas
        .iter()
        .find(|x| x.name == "DBIJI")
        .expect("own schema listed");
    assert!(me.is_current && !me.is_system);
    assert!(schemas.iter().any(|x| x.name == "SYS" && x.is_system));
    let tables = s
        .list_objects(&SchemaRef::schema("DBIJI"), ObjectKind::Table)
        .await
        .unwrap();
    let parent = tables
        .iter()
        .find(|t| t.obj.name == "DBIJI_PARENT")
        .expect("table listed");
    assert_eq!(parent.comment.as_deref(), Some("Parents"));
    let cols = s
        .list_children(&parent.obj, ObjectKind::Column)
        .await
        .unwrap();
    assert_eq!(
        cols.iter()
            .map(|c| (c.obj.name.as_str(), c.detail.as_deref().unwrap_or("")))
            .collect::<Vec<_>>(),
        [
            ("ID", "NUMBER(10) NOT NULL"),
            ("CODE", "VARCHAR2(20 CHAR) NOT NULL"),
            ("PRICE", "NUMBER(10,2)")
        ]
    );
    // NOT NULL is not listed as a check constraint.
    let checks = s
        .list_children(&parent.obj, ObjectKind::CheckConstraint)
        .await
        .unwrap();
    assert_eq!(checks.len(), 1, "{checks:?}");

    let t = s
        .table_definition(&ObjectRef::table(Some("DBIJI".into()), "DBIJI_PARENT"))
        .await
        .unwrap();
    assert!(t.columns[0].auto_increment);
    assert_eq!(t.columns[1].comment.as_deref(), Some("Unique code"));
    assert_eq!(t.columns[2].default.as_deref(), Some("0"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["ID"]);
    assert_eq!(t.unique_keys[0].columns, ["CODE"]);
    assert_eq!(t.checks.len(), 1);
    assert_eq!(t.referenced_by.len(), 1);
    assert_eq!(t.referenced_by[0].table, "DBIJI_CHILD");
    let c = s
        .table_definition(&ObjectRef::table(Some("DBIJI".into()), "DBIJI_CHILD"))
        .await
        .unwrap();
    assert_eq!(c.foreign_keys[0].ref_table, "DBIJI_PARENT");
    assert_eq!(
        c.foreign_keys[0].on_delete,
        dbiji_core::ddl::FkAction::Cascade
    );
    let idx = c
        .indexes
        .iter()
        .find(|i| i.name == "DBIJI_CHILD_PARENT")
        .expect("index");
    assert_eq!(idx.columns.len(), 2);
    assert!(c.indexes.iter().any(|i| i.primary));

    let ddl = s
        .object_ddl(&ObjectRef::table(Some("DBIJI".into()), "DBIJI_CHILD"))
        .await
        .unwrap();
    assert!(
        ddl.contains("CREATE TABLE") && ddl.contains("DBIJI_CHILD"),
        "{ddl}"
    );

    let snap = s
        .schema_snapshot(&SchemaRef::schema("DBIJI"), &SnapshotOptions::light())
        .await
        .unwrap();
    assert!(snap.table("DBIJI_PARENT").is_some() && snap.table("DBIJI_CHILD").is_some());

    let found = s
        .search_metadata(&MetadataSearch {
            pattern: "dbiji_par".into(),
            kinds: vec![ObjectKind::Table, ObjectKind::Column],
            schemas: vec!["DBIJI".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(found.iter().any(|f| f.obj.name == "DBIJI_PARENT"));

    // Browsing through the dialect, as the data editor does.
    let d = s.dialect();
    let sql = d.select_table(
        &ObjectRef::table(Some("DBIJI".into()), "DBIJI_PARENT"),
        None,
        None,
        Some("ID"),
        Some(10),
    );
    exec(
        s.as_mut(),
        "INSERT INTO dbiji_parent (code, price) VALUES ('a', 1.5)",
    )
    .await;
    let (_, rows) = query_all(s.as_mut(), &sql, &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let count = query_scalar(s.as_mut(), &d.count_query(&sql), &[])
        .await
        .unwrap();
    assert_eq!(count, Value::Int(1));

    let plan = s
        .explain("SELECT * FROM dbiji_child WHERE parent_id = 1", false)
        .await
        .unwrap();
    assert_eq!(plan.roots.len(), 1);
    assert!(plan.text.contains("Plan hash value"), "{}", plan.text);

    drop_table(s.as_mut(), "dbiji_child").await;
    drop_table(s.as_mut(), "dbiji_parent").await;
}

#[tokio::test]
async fn large_results_stream_in_batches() {
    let Some(mut s) = connect().await else { return };
    let mut c = s
        .execute(
            "SELECT LEVEL n FROM dual CONNECT BY LEVEL <= 2500",
            &[],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        c.next_result().await.unwrap(),
        Some(ResultHeader::Rows(_))
    ));
    let mut total = 0;
    loop {
        let rows = c.fetch(1000).await.unwrap();
        if rows.is_empty() {
            break;
        }
        assert!(rows.len() <= 1000);
        total += rows.len();
    }
    assert_eq!(total, 2500);
    assert!(c.next_result().await.unwrap().is_none());
}
