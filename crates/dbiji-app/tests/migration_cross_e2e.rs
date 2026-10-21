//! Migrations between different servers: MySQL → PostgreSQL, PostgreSQL →
//! MySQL and PostgreSQL → SQL Server, with the types that need care
//! (TIMESTAMP time zones, text keys, NULL binary values, identities, BIT and
//! geometry). Set `DBIJI_TEST_PG` and `DBIJI_TEST_MYSQL` (and for SQL
//! Server `DBIJI_TEST_MSSQL`) to run.

use async_trait::async_trait;
use dbiji_core::driver::{execute_update, query_all};
use dbiji_core::{ConnectParams, DbResult, Dialect, Driver, ObjectRef, Session, Value};
use dbiji_transfer::{MigrationJob, MigrationMode, MigrationSummary, Progress, SessionFactory, run_migration};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Factory {
    driver: Arc<dyn Driver>,
    params: ConnectParams,
}

#[async_trait]
impl SessionFactory for Factory {
    async fn open(&self) -> DbResult<Box<dyn Session>> {
        self.driver.connect(&self.params).await
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.driver.dialect()
    }
}

fn factory(var: &str, database: Option<&str>, properties: &[(&str, &str)]) -> Option<Arc<Factory>> {
    let url = std::env::var(var).ok()?;
    let (mut cfg, password) = dbiji_config::parse_connection_url(&url).unwrap();
    if let Some(db) = database {
        cfg.database = Some(db.to_string());
    }
    let driver = dbiji_app::driver_registry().get(cfg.kind).unwrap();
    let mut params = ConnectParams::from_config(&cfg, password);
    for (k, v) in properties {
        params.properties.insert(k.to_string(), v.to_string());
    }
    Some(Arc::new(Factory { driver, params }))
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

async fn exec(f: &Factory, sqls: &[&str]) {
    let mut s = f.open().await.unwrap();
    for sql in sqls {
        execute_update(s.as_mut(), sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
    }
    let _ = s.close().await;
}

async fn rows(f: &Factory, sql: &str) -> Vec<Vec<Value>> {
    let mut s = f.open().await.unwrap();
    let (_, rows) = query_all(s.as_mut(), sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {}", e.full_text()));
    let _ = s.close().await;
    rows
}

async fn migrate(job: MigrationJob, src: &Arc<Factory>, dst: &Arc<Factory>) -> MigrationSummary {
    let s = run_migration(job, src.clone(), dst.clone(), &Progress::new(), &CancellationToken::new()).await.unwrap_or_else(|e| panic!("{}", e.full_text()));
    for t in &s.tables {
        assert!(t.error.is_none() && t.rows_failed == 0, "{t:?}");
    }
    s
}

#[tokio::test]
async fn mysql_to_postgres_keeps_timestamps_and_fills_identities() {
    let (Some(pg), Some(my)) = (factory("DBIJI_TEST_PG", None, &[]), factory("DBIJI_TEST_MYSQL", None, &[])) else { return };
    let db = unique("dbiji_x");
    exec(&my, &[&format!("CREATE DATABASE {db}")]).await;
    // The session's time zone is not UTC: TIMESTAMP values come in +05:00.
    let src = factory("DBIJI_TEST_MYSQL", Some(&db), &[("time_zone", "+05:00")]).unwrap();
    let (ts, gid) = (unique("ts"), unique("gid"));
    exec(
        &src,
        &[
            &format!("CREATE TABLE {ts} (id INT PRIMARY KEY, at TIMESTAMP NULL, local DATETIME NULL)"),
            &format!("INSERT INTO {ts} VALUES (1, '2024-01-02 15:00:00', '2024-01-02 15:00:00'), (2, NULL, NULL)"),
            &format!("CREATE TABLE {gid} (id INT PRIMARY KEY, v TEXT)"),
            &format!("INSERT INTO {gid} VALUES (7, 'a'), (9, 'b')"),
        ],
    )
    .await;
    // An existing table whose ids are GENERATED ALWAYS.
    exec(&pg, &[&format!("CREATE TABLE public.{gid} (id INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT)")]).await;

    let job = MigrationJob {
        tables: vec![ObjectRef::table(Some(db.clone()), ts.clone()), ObjectRef::table(Some(db.clone()), gid.clone())],
        target_schema: Some("public".into()),
        ..Default::default()
    };
    let result = std::panic::AssertUnwindSafe(async {
        migrate(job, &src, &pg).await;
        let r = rows(&pg, &format!("SELECT extract(epoch FROM at)::bigint, local::text FROM public.{ts} ORDER BY id")).await;
        // 15:00 at +05:00 is 10:00 UTC.
        assert_eq!(r[0][0].as_i64(), Some(1_704_189_600), "{r:?}");
        assert_eq!(r[0][1], Value::text("2024-01-02 15:00:00"), "DATETIME keeps its wall clock time");
        assert!(r[1][0].is_null());
        let r = rows(&pg, &format!("SELECT id, v FROM public.{gid} ORDER BY id")).await;
        assert_eq!(r.iter().map(|r| r[0].as_i64()).collect::<Vec<_>>(), vec![Some(7), Some(9)]);
    });
    let outcome = futures::FutureExt::catch_unwind(result).await;
    exec(&pg, &[&format!("DROP TABLE IF EXISTS public.{ts}"), &format!("DROP TABLE IF EXISTS public.{gid}")]).await;
    exec(&my, &[&format!("DROP DATABASE {db}")]).await;
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
async fn postgres_to_mysql_text_keys_bits_and_geometry() {
    let (Some(pg), Some(my)) = (factory("DBIJI_TEST_PG", None, &[]), factory("DBIJI_TEST_MYSQL", None, &[])) else { return };
    let db = unique("dbiji_x");
    let (db2, keys, geo) = (unique("dbiji_y"), unique("keys"), unique("geo"));
    exec(&my, &[&format!("CREATE DATABASE {db}"), &format!("CREATE DATABASE {db2}")]).await;
    let target = factory("DBIJI_TEST_MYSQL", Some(&db), &[]).unwrap();
    let mysrc = factory("DBIJI_TEST_MYSQL", Some(&db2), &[]).unwrap();
    exec(
        &pg,
        &[
            &format!("CREATE TABLE public.{keys} (id TEXT PRIMARY KEY, code TEXT UNIQUE, note TEXT)"),
            &format!("INSERT INTO public.{keys} VALUES ('a1', 'x', NULL), ('b2', 'y', 'n')"),
        ],
    )
    .await;
    exec(
        &mysrc,
        &[
            &format!("CREATE TABLE {geo} (id INT PRIMARY KEY, flags BIT(8), p POINT)"),
            &format!("INSERT INTO {geo} VALUES (1, b'00000101', ST_GeomFromText('POINT(1 2)')), (2, NULL, NULL)"),
        ],
    )
    .await;
    let result = std::panic::AssertUnwindSafe(async {
        let job = MigrationJob { tables: vec![ObjectRef::table(Some("public".into()), keys.clone())], target_schema: Some(db.clone()), ..Default::default() };
        let s = migrate(job, &pg, &target).await;
        assert!(s.warnings.iter().any(|w| w.contains("varchar(255)")), "{:?}", s.warnings);
        let r = rows(&target, &format!("SELECT id, code FROM {keys} ORDER BY id")).await;
        assert_eq!(r.len(), 2);
        // MySQL → MySQL: BIT and geometry values are written as such, not as strings.
        let job = MigrationJob { tables: vec![ObjectRef::table(Some(db2.clone()), geo.clone())], target_schema: Some(db.clone()), ..Default::default() };
        migrate(job, &mysrc, &target).await;
        let r = rows(&target, &format!("SELECT CAST(flags AS UNSIGNED), ST_AsText(p) FROM {geo} ORDER BY id")).await;
        assert_eq!(r[0][0].as_i64(), Some(5), "{r:?}");
        assert_eq!(r[0][1], Value::text("POINT(1 2)"));
        assert!(r[1][0].is_null() && r[1][1].is_null());
    });
    let outcome = futures::FutureExt::catch_unwind(result).await;
    exec(&pg, &[&format!("DROP TABLE IF EXISTS public.{keys}")]).await;
    exec(&my, &[&format!("DROP DATABASE {db}"), &format!("DROP DATABASE {db2}")]).await;
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
async fn postgres_to_sql_server_text_keys_null_binary_and_datetime_keys() {
    let (Some(pg), Some(ms)) = (factory("DBIJI_TEST_PG", None, &[]), factory("DBIJI_TEST_MSSQL", None, &[])) else { return };
    let (t, dt) = (unique("bin"), unique("dt"));
    exec(
        &pg,
        &[
            &format!("CREATE TABLE public.{t} (id TEXT PRIMARY KEY, b BYTEA, at TIMESTAMP(3))"),
            &format!("INSERT INTO public.{t} VALUES ('a', NULL, '2024-01-02 10:00:00.003'), ('b', '\\x0102', NULL)"),
        ],
    )
    .await;
    exec(
        &ms,
        &[
            &format!("CREATE TABLE dbo.{dt} (k DATETIME PRIMARY KEY, v INT)"),
            &format!("INSERT INTO dbo.{dt} VALUES ('2024-01-02T10:00:00.003', 1), ('2024-01-02T10:00:00.007', 2)"),
        ],
    )
    .await;
    let result = std::panic::AssertUnwindSafe(async {
        let job = MigrationJob { tables: vec![ObjectRef::table(Some("public".into()), t.clone())], target_schema: Some("dbo".into()), ..Default::default() };
        migrate(job, &pg, &ms).await;
        let r = rows(&ms, &format!("SELECT id, DATALENGTH(b) FROM dbo.{t} ORDER BY id")).await;
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[0][1].is_null());
        assert_eq!(r[1][1].as_i64(), Some(2));
        // Replace mode matches datetime keys exactly (no duplicates, no key violations).
        let copy = format!("{dt}_c");
        exec(&ms, &[&format!("SELECT * INTO dbo.{copy} FROM dbo.{dt}"), &format!("ALTER TABLE dbo.{copy} ADD PRIMARY KEY (k)")]).await;
        let mut job =
            MigrationJob { tables: vec![ObjectRef::table(Some("dbo".into()), dt.clone())], target_schema: Some("dbo".into()), mode: MigrationMode::Replace, ..Default::default() };
        job.table_names.insert(dt.clone(), copy.clone());
        migrate(job, &ms, &ms).await;
        let r = rows(&ms, &format!("SELECT COUNT(*) FROM dbo.{copy}")).await;
        assert_eq!(r[0][0].as_i64(), Some(2));
        exec(&ms, &[&format!("DROP TABLE dbo.{copy}")]).await;
    });
    let outcome = futures::FutureExt::catch_unwind(result).await;
    exec(&pg, &[&format!("DROP TABLE IF EXISTS public.{t}")]).await;
    exec(&ms, &[&format!("DROP TABLE IF EXISTS dbo.{t}"), &format!("DROP TABLE IF EXISTS dbo.{dt}")]).await;
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}
