//! Headless end-to-end tests of the data transfer tabs (export, import, mock
//! data, migration) and of the transfer task runners: the tabs are drawn in
//! real egui frames (no window) and run against real servers.
//! Set `DBIJI_TEST_PG` / `DBIJI_TEST_MYSQL` / `DBIJI_TEST_MARIADB` to run.

use dbiji_app::backend::Backend;
use dbiji_app::command::AppCommand;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_app::cx::AppContext;
use dbiji_app::keys::Keymap;
use dbiji_app::ops::Operations;
use dbiji_app::services::{Services, Settings};
use dbiji_app::tabs::TabView;
use dbiji_app::tabs::transfer::{ExportTab, ImportTab, MigrationTab, MockDataTab, ResultSource};
use dbiji_app::taskexec::AppTaskExecutor;
use dbiji_app::toasts::Toasts;
use dbiji_app::transferexec::{ExportTaskExtras, export_task_config, import_task_config, migration_task_config};
use dbiji_config::{AppPaths, ConnectionStore};
use dbiji_core::config::{ConnectionConfig, SecretSlot, secret_key};
use dbiji_core::meta::ObjectRef;
use dbiji_core::{ColumnMeta, DataKind, DbKind, Secret, Value};
use dbiji_tasks::{RunStatus, TaskDef, TaskKind, Trigger};
use dbiji_transfer::{CancellationToken, ExportFormat, ExportItem, ExportJob, ImportJob, ImportMapping, ImportSourceConfig, MigrationJob};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROWS: i64 = 25;

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn servers() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (name, var) in [("postgres", "DBIJI_TEST_PG"), ("mysql", "DBIJI_TEST_MYSQL"), ("mariadb", "DBIJI_TEST_MARIADB")] {
        if let Ok(url) = std::env::var(var)
            && !url.trim().is_empty()
        {
            out.push((name, url));
        }
    }
    out
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

struct H {
    backend: Arc<Backend>,
    ctx: egui::Context,
    cm: ConnectionManager,
    services: Services,
    settings: Settings,
    keymap: Keymap,
    toasts: Toasts,
    ops: Operations,
    commands: Vec<AppCommand>,
    time: f64,
    id: String,
    cfg: ConnectionConfig,
    password: Option<Secret>,
    root: PathBuf,
    tables: Vec<String>,
    schemas: Vec<String>,
    _dir: tempdir::Dir,
}

impl H {
    fn new(url: &str) -> H {
        Self::create(Some(url))
    }

    /// Harness without any server (exports of fetched rows).
    fn offline() -> H {
        Self::create(None)
    }

    fn create(url: Option<&str>) -> H {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let backend = Arc::new(Backend::new().unwrap());
        let ctx = egui::Context::default();
        backend.set_context(ctx.clone());
        let (mut cfg, password) = match url {
            Some(url) => dbiji_config::parse_connection_url(url).unwrap(),
            None => (ConnectionConfig::default(), None),
        };
        cfg.name = "transfer-e2e".into();
        cfg.save_password = true;
        let secrets = Arc::new(MemorySecrets::default());
        if let Some(pw) = &password {
            secrets.set(&secret_key(&cfg.id, SecretSlot::Password), pw).unwrap();
        }
        let mut cm = ConnectionManager::new(dbiji_app::driver_registry(), secrets, Arc::new(DirectNet));
        let id = cfg.id.clone();
        cm.upsert(cfg.clone());
        if url.is_some() {
            cm.connect(&backend, &id, Credentials::default()).unwrap();
            let t0 = Instant::now();
            while !cm.is_connected(&id) {
                assert!(cm.errors().is_empty(), "{:?}", cm.errors());
                assert!(t0.elapsed() < Duration::from_secs(20), "connect timeout");
                cm.poll(&backend);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let root = std::env::temp_dir().join(format!("dbiji-transfer-e2e-{}", uuid::Uuid::new_v4().simple()));
        let services = Services::ephemeral(&root);
        let settings = Settings::default();
        let (keymap, _) = Keymap::from_config(&settings.keymap, dbiji_app::keys::IS_MAC);
        H {
            backend,
            ctx,
            cm,
            services,
            settings,
            keymap,
            toasts: Toasts::default(),
            ops: Operations::default(),
            commands: Vec::new(),
            time: 0.0,
            id,
            cfg,
            password,
            root: root.clone(),
            tables: Vec::new(),
            schemas: Vec::new(),
            _dir: tempdir::Dir(root),
        }
    }

    fn with_cx<R>(&mut self, f: impl FnOnce(&mut AppContext) -> R) -> R {
        let mut cx = AppContext {
            backend: &self.backend,
            conns: &mut self.cm,
            services: &self.services,
            settings: &mut self.settings,
            commands: &mut self.commands,
            toasts: &mut self.toasts,
            ops: &mut self.ops,
            keymap: &self.keymap,
            tab_active: true,
            current_tab: Some(1),
        };
        f(&mut cx)
    }

    fn frame(&mut self, tab: &mut dyn TabView) {
        self.time += 1.0 / 60.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
            time: Some(self.time),
            focused: true,
            ..Default::default()
        };
        let ctx = self.ctx.clone();
        let mut out = ctx.run_ui(input, |ui| {
            self.with_cx(|cx| tab.ui(ui, cx));
        });
        out.textures_delta.clear();
        self.cm.poll(&self.backend);
        // Metadata refreshes requested by the tabs (what the app does with them).
        for cmd in std::mem::take(&mut self.commands) {
            if let AppCommand::RefreshMetadata { connection, object, schema } = cmd
                && let Some(live) = self.cm.live_mut(&connection)
            {
                match (object, schema) {
                    (Some(o), _) => live.meta.clear_object(&o),
                    (None, Some(s)) => live.meta.clear_schema(&s),
                    (None, None) => live.meta.clear(),
                }
            }
        }
    }

    /// Draw frames until `done` or the timeout.
    fn until<T: TabView>(&mut self, tab: &mut T, what: &str, mut done: impl FnMut(&T) -> bool) {
        let t0 = Instant::now();
        loop {
            self.frame(tab);
            if done(tab) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Schema of the connection (PostgreSQL schema, MySQL database).
    fn schema(&self) -> Option<String> {
        let live = self.cm.live(&self.id).unwrap();
        live.current_schema.clone().or_else(|| live.current_database.clone())
    }

    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::table(self.schema(), name)
    }

    /// A new, empty schema (PostgreSQL) or database (MySQL / MariaDB),
    /// dropped at the end of the test.
    fn other_schema(&mut self) -> String {
        let s = unique("dbiji_trs");
        match self.cfg.kind {
            DbKind::Postgres => self.sql(&format!("CREATE SCHEMA {s}")),
            _ => self.sql(&format!("CREATE DATABASE {s}")),
        }
        self.schemas.push(s.clone());
        s
    }

    fn sql(&self, sql: &str) {
        let s = self.cm.live(&self.id).unwrap().main.clone();
        let text = sql.to_string();
        self.backend.block_on(async move { s.execute_update(&text, &[]).await }).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    fn rows(&self, sql: &str) -> Vec<Vec<Value>> {
        let s = self.cm.live(&self.id).unwrap().main.clone();
        let sql = sql.to_string();
        self.backend.block_on(async move { s.query_all(&sql, &[]).await }).unwrap_or_else(|e| panic!("{e}")).1
    }

    fn count(&self, table: &str) -> i64 {
        self.rows(&format!("SELECT COUNT(*) FROM {table}"))[0][0].as_i64().unwrap()
    }

    /// Create a table with `ROWS` rows of mixed types (quotes, commas, line
    /// breaks and NULLs in the text column).
    fn sample_table(&mut self) -> String {
        let t = unique("dbiji_tr");
        self.sql(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, name VARCHAR(50) NOT NULL, amount NUMERIC(10,2), created DATE, note VARCHAR(200))"));
        self.tables.push(t.clone());
        let values: Vec<String> = (1..=ROWS)
            .map(|i| {
                let note = match i % 5 {
                    0 => "NULL".to_string(),
                    1 => "'plain'".to_string(),
                    2 => "'with, comma and \"quotes\"'".to_string(),
                    3 => "'it''s on\ntwo lines'".to_string(),
                    _ => "''".to_string(),
                };
                format!("({i}, 'name {i}', {i}.{:02}, '2024-01-{:02}', {note})", i % 100, (i % 28) + 1)
            })
            .collect();
        self.sql(&format!("INSERT INTO {t} (id, name, amount, created, note) VALUES {}", values.join(", ")));
        t
    }

    fn text_rows(&self, sql: &str) -> Vec<Vec<String>> {
        self.rows(sql).iter().map(|r| r.iter().map(|v| if v.is_null() { "<null>".into() } else { v.to_text().into_owned() }).collect()).collect()
    }

    /// Task executor over the test connection, with `connections.json` and
    /// the password in the temporary configuration.
    fn executor(&self) -> Arc<AppTaskExecutor> {
        let paths = AppPaths::with_root(&self.root);
        paths.ensure_dirs().unwrap();
        let mut store = ConnectionStore::new();
        store.add(self.cfg.clone()).unwrap();
        store.save(&paths).unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        if let Some(pw) = &self.password {
            secrets.set(&store.network_secret_key(&self.cfg, SecretSlot::Password), pw).unwrap();
        }
        Arc::new(AppTaskExecutor { paths, registry: Arc::new(dbiji_app::driver_registry()), secrets, net: Arc::new(DirectNet) })
    }

    fn run_task(&self, exec: &Arc<AppTaskExecutor>, task: TaskDef) -> dbiji_tasks::TaskRun {
        let store = self.services.tasks.clone().expect("task store");
        let log = self.services.runlog.clone().expect("run log");
        let id = store.create(task).unwrap().id;
        let exec: Arc<dyn dbiji_tasks::TaskExecutor> = exec.clone();
        self.backend.block_on(dbiji_tasks::run_task_once(store, log, exec, &id, Trigger::Manual, None, CancellationToken::new())).unwrap()
    }
}

impl Drop for H {
    fn drop(&mut self) {
        let mut stmts: Vec<String> = std::mem::take(&mut self.tables).into_iter().map(|t| format!("DROP TABLE IF EXISTS {t}")).collect();
        for s in std::mem::take(&mut self.schemas) {
            stmts.push(match self.cfg.kind {
                DbKind::Postgres => format!("DROP SCHEMA IF EXISTS {s} CASCADE"),
                _ => format!("DROP DATABASE IF EXISTS {s}"),
            });
        }
        if let Some(live) = self.cm.live(&self.id) {
            let s = live.main.clone();
            self.backend.block_on(async move {
                for sql in stmts {
                    let _ = s.execute_update(&sql, &[]).await;
                }
            });
        }
        self.cm.disconnect_all(&self.backend);
    }
}

fn file_lines(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

#[test]
fn export_to_csv_and_json_and_import_back() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let t = h.sample_table();
        let out = h.root.join("out");

        // ---- CSV export through the tab's run path.
        let mut tab = ExportTab::new(h.id.clone(), vec![h.table(&t)], None);
        {
            let s = tab.settings_mut();
            s.format = ExportFormat::Csv;
            s.output_folder = out.clone();
            s.file_name_pattern = "${table}".into();
            s.count_rows = true;
        }
        h.frame(&mut tab);
        h.with_cx(|cx| tab.start(cx));
        assert!(tab.is_running(), "{name}: {:?}", tab.last_error());
        h.until(&mut tab, "CSV export", |t| !t.is_running());
        let summary = tab.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", tab.last_error())).clone();
        assert_eq!(summary.rows, ROWS as u64, "{name}");
        assert_eq!(summary.failed(), 0, "{name}: {:?}", summary.first_error());
        let csv = out.join(format!("{t}.csv"));
        assert!(csv.is_file(), "{name}: {}", csv.display());
        // Header + rows; the two-line value adds a line per fifth row.
        assert_eq!(file_lines(&csv), 1 + ROWS as usize + (ROWS as usize).div_ceil(5), "{name}");
        assert_eq!(h.ops.running(), 0, "{name}: the operation is finished");

        // ---- JSON export with the same tab.
        tab.settings_mut().format = ExportFormat::Json;
        h.with_cx(|cx| tab.start(cx));
        h.until(&mut tab, "JSON export", |t| !t.is_running());
        let summary = tab.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", tab.last_error())).clone();
        assert_eq!(summary.rows, ROWS as u64, "{name}");
        let json_path = out.join(format!("{t}.json"));
        let json: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(json.len(), ROWS as usize, "{name}");
        assert!(json.iter().any(|o| o["name"] == "name 1" && o["id"] == 1), "{name}: {:?}", json.first());

        // ---- Import the CSV into a new table through the import tab.
        let copy = format!("{t}_copy");
        h.tables.push(copy.clone());
        let mut imp = ImportTab::new(h.id.clone(), None, None);
        imp.set_file(&csv);
        imp.set_new_table(None, &copy);
        h.until(&mut imp, "import preview and mapping", |t| t.mapping_ready() || t.preview_error().is_some());
        assert_eq!(imp.preview_error(), None, "{name}");
        assert_eq!(imp.preview_columns(), vec!["id", "name", "amount", "created", "note"], "{name}");
        assert_eq!(imp.preview_row_count(), ROWS as usize, "{name}");
        let mapping = imp.mapping().unwrap();
        assert!(mapping.create_table, "{name}");
        assert_eq!(mapping.columns.iter().filter(|c| !c.skip).count(), 5, "{name}");
        h.with_cx(|cx| imp.start(cx));
        assert!(imp.is_running(), "{name}: {:?}", imp.last_error());
        h.until(&mut imp, "CSV import", |t| !t.is_running());
        let s = imp.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", imp.last_error())).clone();
        assert_eq!((s.rows_read, s.rows_written, s.rows_failed), (ROWS as u64, ROWS as u64, 0), "{name}: {:?}", s.errors);
        assert!(s.created_table, "{name}");
        assert_eq!(h.count(&copy), ROWS, "{name}");
        let original = h.text_rows(&format!("SELECT id, name, created, note FROM {t} ORDER BY id"));
        let imported = h.text_rows(&format!("SELECT id, name, created, note FROM {copy} ORDER BY id"));
        assert_eq!(imported, original, "{name}: values survive the CSV round trip");
        let sums = h.text_rows(&format!("SELECT SUM(amount) FROM {t} UNION ALL SELECT SUM(amount) FROM {copy}"));
        assert_eq!(sums[0], sums[1], "{name}: amounts");

        // ---- Import the JSON file into the existing table (append).
        let mut imp2 = ImportTab::new(h.id.clone(), Some(h.table(&copy)), None);
        imp2.set_file(&json_path);
        h.until(&mut imp2, "JSON mapping", |t| t.mapping_ready() || t.preview_error().is_some() || t.last_error().is_some_and(|e| e.contains("table definition")));
        assert_eq!(imp2.preview_error(), None, "{name}");
        assert!(!imp2.mapping().unwrap().create_table, "{name}");
        h.with_cx(|cx| imp2.start(cx));
        h.until(&mut imp2, "JSON import", |t| !t.is_running());
        let s = imp2.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", imp2.last_error())).clone();
        assert_eq!((s.rows_written, s.rows_failed), (ROWS as u64, 0), "{name}: {:?}", s.errors);
        assert!(!s.created_table, "{name}");
        assert_eq!(h.count(&copy), 2 * ROWS, "{name}");

        // ---- Save the export as a task and run it headless.
        tab.settings_mut().format = ExportFormat::Csv;
        tab.settings_mut().file_name_pattern = "${table}_saved".into();
        h.with_cx(|cx| tab.save_task(cx, "Saved export", None));
        h.until(&mut tab, "task saved", |t| t.saved_task_id().is_some());
        let task_id = tab.saved_task_id().unwrap().to_string();
        let store = h.services.tasks.clone().unwrap();
        let saved = store.get(&task_id).expect("saved task");
        assert!(matches!(&saved.kind, TaskKind::Export { connection_id, .. } if *connection_id == h.id), "{name}");
        let exec = h.executor();
        let log = h.services.runlog.clone().unwrap();
        let exec_dyn: Arc<dyn dbiji_tasks::TaskExecutor> = exec.clone();
        let run = h.backend.block_on(dbiji_tasks::run_task_once(store, log, exec_dyn, &task_id, Trigger::Manual, None, CancellationToken::new())).unwrap();
        assert_eq!(run.status, RunStatus::Succeeded, "{name}: {:?}", run.error);
        assert_eq!(run.rows, Some(ROWS as u64), "{name}");
        assert!(out.join(format!("{t}_saved.csv")).is_file(), "{name}");
    }
}

#[test]
fn export_fetched_rows_without_database() {
    let mut h = H::offline();
    let out = h.root.join("fetched");
    let store = dbiji_core::RowStore::shared(1 << 20);
    store.lock().extend((0..10).map(|i| vec![Value::Int(i), Value::text(format!("r{i}")), Value::Null]));
    let columns = vec![ColumnMeta::new("n", "int", DataKind::Int), ColumnMeta::new("label", "text", DataKind::Text), ColumnMeta::new("empty", "text", DataKind::Text)];
    let mut tab = ExportTab::from_result(h.id.clone(), "Result 1".into(), store, columns, Some(vec![7, 2, 4]), vec![1, 0], Some("select 1".into()));
    assert_eq!(tab.result_source(), ResultSource::Selected);
    {
        let s = tab.settings_mut();
        s.output_folder = out.clone();
        s.file_name_pattern = "${table}".into();
    }
    h.with_cx(|cx| tab.start(cx));
    h.until(&mut tab, "fetched rows export", |t| !t.is_running());
    let summary = tab.last_summary().unwrap_or_else(|| panic!("{:?}", tab.last_error())).clone();
    assert_eq!(summary.rows, 3);
    let text = std::fs::read_to_string(out.join("Result 1.csv")).unwrap();
    assert_eq!(text, "label,n\nr7,7\nr2,2\nr4,4\n");
    // All fetched rows.
    tab.set_result_source(ResultSource::Fetched);
    h.with_cx(|cx| tab.start(cx));
    h.until(&mut tab, "all fetched rows", |t| !t.is_running());
    assert_eq!(tab.last_summary().map(|s| s.rows), Some(10));
}

#[test]
fn mock_data_and_migration_tabs() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let t = unique("dbiji_mk");
        h.sql(&format!("CREATE TABLE {t} (id INT PRIMARY KEY, email VARCHAR(60) NOT NULL, first_name VARCHAR(30), amount NUMERIC(8,2), created DATE)"));
        h.tables.push(t.clone());

        // ---- Mock data.
        let mut mock = MockDataTab::new(h.id.clone(), h.table(&t));
        h.until(&mut mock, "table definition", |m| m.definition_loaded() || m.last_error().is_some());
        assert_eq!(mock.last_error(), None, "{name}");
        mock.config_mut().rows = 120;
        mock.config_mut().seed = Some(7);
        h.with_cx(|cx| mock.start(cx));
        assert!(mock.is_running(), "{name}: {:?}", mock.last_error());
        h.until(&mut mock, "mock data", |m| !m.is_running());
        let s = mock.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", mock.last_error())).clone();
        assert_eq!((s.rows_inserted, s.rows_failed), (120, 0), "{name}: {:?}", s.errors);
        assert_eq!(h.count(&t), 120, "{name}");
        let emails = h.rows(&format!("SELECT COUNT(*) FROM {t} WHERE email LIKE '%@%'"));
        assert_eq!(emails[0][0].as_i64(), Some(120), "{name}: e-mail generator");

        // ---- Migration into another schema of the same server, renamed.
        let target = h.other_schema();
        let copy = format!("{target}.{t}_mig");
        let mut mig = MigrationTab::new(h.id.clone(), vec![h.table(&t)]);
        mig.set_target(h.id.clone(), Some(target.clone()));
        mig.set_target_name(&t, &format!("{t}_mig"));
        h.frame(&mut mig);
        h.with_cx(|cx| mig.start(cx));
        assert!(mig.is_running(), "{name}: {:?}", mig.last_error());
        h.until(&mut mig, "migration", |m| !m.is_running());
        let s = mig.last_summary().unwrap_or_else(|| panic!("{name}: {:?}", mig.last_error())).clone();
        assert_eq!(s.rows, 120, "{name}: {:?}", s.tables);
        assert!(s.tables.iter().all(|t| t.error.is_none() && t.created), "{name}: {:?}", s.tables);
        assert_eq!(h.count(&copy), 120, "{name}");

        // The target may not be the source table itself.
        let mut same = MigrationTab::new(h.id.clone(), vec![h.table(&t)]);
        same.set_target(h.id.clone(), h.schema());
        h.with_cx(|cx| same.start(cx));
        assert!(!same.is_running() && same.last_error().is_some(), "{name}");
    }
}

#[test]
fn transfer_tasks_run_headless() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let t = h.sample_table();
        let out = h.root.join("tasks");
        let exec = h.executor();

        // ---- Export task: a filtered table to CSV and a query to SQL for another product.
        let other = if h.cfg.kind == DbKind::Postgres { DbKind::MySql } else { DbKind::Postgres };
        let job = ExportJob {
            items: vec![ExportItem::Table { table: h.table(&t), where_clause: Some("id <= 10".into()) }],
            format: ExportFormat::Csv,
            output_folder: out.clone(),
            file_name_pattern: "${table}_task".into(),
            ..Default::default()
        };
        let config = export_task_config(&job, &ExportTaskExtras::default()).unwrap();
        let run = h.run_task(&exec, TaskDef::new("csv export", TaskKind::Export { connection_id: h.id.clone(), config }));
        assert_eq!(run.status, RunStatus::Succeeded, "{name}: {:?}", run.error);
        assert_eq!(run.rows, Some(10), "{name}");
        let csv = out.join(format!("{t}_task.csv"));
        assert_eq!(run.output_files, vec![csv.clone()], "{name}");

        let sql_job = ExportJob {
            items: vec![ExportItem::Query { sql: format!("SELECT id, name FROM {t} ORDER BY id"), name: Some("names".into()) }],
            format: ExportFormat::Sql,
            output_folder: out.clone(),
            file_name_pattern: "${table}".into(),
            ..Default::default()
        };
        let config = export_task_config(&sql_job, &ExportTaskExtras { sql_dialect: Some(other) }).unwrap();
        let run = h.run_task(&exec, TaskDef::new("sql export", TaskKind::Export { connection_id: h.id.clone(), config }));
        assert_eq!(run.status, RunStatus::Succeeded, "{name}: {:?}", run.error);
        let script = std::fs::read_to_string(out.join("names.sql")).unwrap();
        assert!(script.contains("INSERT INTO"), "{name}: {script}");

        // ---- Import task: the CSV into a new table (automatic mapping).
        let copy = format!("{t}_imp");
        h.tables.push(copy.clone());
        let import = ImportJob {
            source: ImportSourceConfig::Csv { path: csv.clone(), options: Default::default() },
            mapping: ImportMapping { target: h.table(&copy), create_table: true, ..Default::default() },
            sample_rows: 100,
            ..Default::default()
        };
        let config = import_task_config(&import).unwrap();
        let run = h.run_task(&exec, TaskDef::new("csv import", TaskKind::Import { connection_id: h.id.clone(), config }));
        assert_eq!(run.status, RunStatus::Succeeded, "{name}: {:?}", run.error);
        assert_eq!(run.rows, Some(10), "{name}");
        assert_eq!(h.count(&copy), 10, "{name}");

        // ---- Migration task into another schema of the same server.
        let target = h.other_schema();
        let mig = format!("{target}.{t}");
        let job = MigrationJob { tables: vec![h.table(&t)], target_schema: Some(target.clone()), ..Default::default() };
        let config = migration_task_config(&job).unwrap();
        let run = h.run_task(&exec, TaskDef::new("migration", TaskKind::Migration { source_connection_id: h.id.clone(), target_connection_id: h.id.clone(), config }));
        assert_eq!(run.status, RunStatus::Succeeded, "{name}: {:?}", run.error);
        assert_eq!(run.rows, Some(ROWS as u64), "{name}");
        assert_eq!(h.count(&mig), ROWS, "{name}");

        // ---- Invalid configurations fail with a message.
        let run = h.run_task(&exec, TaskDef::new("broken", TaskKind::Export { connection_id: h.id.clone(), config: serde_json::json!({"items": 5}) }));
        assert_eq!(run.status, RunStatus::Failed, "{name}");
        assert!(run.error.as_deref().is_some_and(|e| !e.is_empty()), "{name}");
    }
}
