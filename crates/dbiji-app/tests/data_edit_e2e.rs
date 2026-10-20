//! PRD phase 1 gate: query, edit data, save and cancel work on PostgreSQL,
//! MySQL, MariaDB and SQLite. The table data view is drawn in headless egui
//! frames; edits go through the grid model exactly like keyboard edits, and
//! saving runs the generated statements in one transaction.
//! Servers: `DBIJI_TEST_PG`, `DBIJI_TEST_MYSQL`, `DBIJI_TEST_MARIADB` (SQLite always runs).

use dbiji_app::backend::Backend;
use dbiji_app::command::AppCommand;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_app::cx::AppContext;
use dbiji_app::keys::Keymap;
use dbiji_app::ops::Operations;
use dbiji_app::results::data_tab::TableDataView;
use dbiji_app::services::{Services, Settings};
use dbiji_app::toasts::Toasts;
use dbiji_core::config::{SecretSlot, secret_key};
use dbiji_core::meta::ObjectRef;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn servers() -> Vec<(&'static str, String)> {
    let file = std::env::temp_dir().join(format!("dbiji-edit-e2e-{}.db", uuid::Uuid::new_v4().simple()));
    let mut out = vec![("sqlite", format!("sqlite://{}", file.display()))];
    for (name, var) in [("postgres", "DBIJI_TEST_PG"), ("cockroach", "DBIJI_TEST_COCKROACH"), ("mysql", "DBIJI_TEST_MYSQL"), ("mariadb", "DBIJI_TEST_MARIADB")] {
        if let Ok(url) = std::env::var(var)
            && !url.trim().is_empty()
        {
            out.push((name, url));
        }
    }
    out
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
    dir: std::path::PathBuf,
}

impl Drop for H {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl H {
    fn new(url: &str) -> H {
        Self::new_with(url, |_| {})
    }

    fn new_with(url: &str, adjust: impl FnOnce(&mut dbiji_core::config::ConnectionConfig)) -> H {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let backend = Arc::new(Backend::new().unwrap());
        let ctx = egui::Context::default();
        backend.set_context(ctx.clone());
        let (mut cfg, password) = dbiji_config::parse_connection_url(url).unwrap();
        adjust(&mut cfg);
        cfg.save_password = true;
        if password.is_none() {
            cfg.auth = dbiji_core::config::AuthMethod::None;
        }
        if password.is_none() {
            cfg.auth = dbiji_core::config::AuthMethod::None;
        }
        let secrets = Arc::new(MemorySecrets::default());
        if let Some(pw) = &password {
            secrets.set(&secret_key(&cfg.id, SecretSlot::Password), pw).unwrap();
        }
        let mut cm = ConnectionManager::new(dbiji_app::driver_registry(), secrets, Arc::new(DirectNet));
        let id = cfg.id.clone();
        cm.upsert(cfg);
        cm.connect(&backend, &id, Credentials::default()).unwrap();
        let t0 = Instant::now();
        while !cm.is_connected(&id) {
            assert!(cm.errors().is_empty(), "{:?}", cm.errors());
            assert!(t0.elapsed() < Duration::from_secs(20));
            cm.poll(&backend);
            std::thread::sleep(Duration::from_millis(10));
        }
        let dir = std::env::temp_dir().join(format!("dbiji-edit-e2e-home-{}", uuid::Uuid::new_v4().simple()));
        let services = Services::ephemeral(&dir);
        let settings = Settings::default();
        let (keymap, _) = Keymap::from_config(&settings.keymap, dbiji_app::keys::IS_MAC);
        H { backend, ctx, cm, services, settings, keymap, toasts: Toasts::default(), ops: Operations::default(), commands: Vec::new(), time: 0.0, id, dir }
    }

    fn frame(&mut self, view: &mut TableDataView) {
        self.time += 1.0 / 60.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
            time: Some(self.time),
            focused: true,
            ..Default::default()
        };
        let ctx = self.ctx.clone();
        let mut out = ctx.run_ui(input, |ui| {
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
                popup: false,
            };
            view.ui(ui, &mut cx);
        });
        out.textures_delta.clear();
        self.cm.poll(&self.backend);
    }

    fn until(&mut self, view: &mut TableDataView, what: &str, mut done: impl FnMut(&mut TableDataView) -> bool) {
        let t0 = Instant::now();
        loop {
            self.frame(view);
            if done(view) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn try_sql(&self, sql: &str) -> Result<(), String> {
        let s = self.cm.live(&self.id).unwrap().main.clone();
        let owned = sql.to_string();
        self.backend.block_on(async move { s.execute_update(&owned, &[]).await }).map(|_| ()).map_err(|e| e.to_string())
    }

    fn sql(&self, sql: &str) {
        let s = self.cm.live(&self.id).unwrap().main.clone();
        let owned = sql.to_string();
        self.backend.block_on(async move { s.execute_update(&owned, &[]).await }).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        let s = self.cm.live(&self.id).unwrap().main.clone();
        let sql = sql.to_string();
        let (_, rows) = self.backend.block_on(async move { s.query_all(&sql, &[]).await }).unwrap();
        rows.iter().map(|r| r.iter().map(|v| v.to_text().into_owned()).collect()).collect()
    }
}

fn save_and_wait(h: &mut H, view: &mut TableDataView) {
    {
        let mut cx = AppContext {
            backend: &h.backend,
            conns: &mut h.cm,
            services: &h.services,
            settings: &mut h.settings,
            commands: &mut h.commands,
            toasts: &mut h.toasts,
            ops: &mut h.ops,
            keymap: &h.keymap,
            tab_active: true,
            current_tab: Some(1),
            popup: false,
        };
        view.panel.as_mut().unwrap().save(&mut cx);
    }
    h.until(view, "save", |v| !v.panel.as_ref().unwrap().has_changes());
}

#[test]
fn edit_save_and_cancel() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let table = format!("dbiji_edit_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        h.sql(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(40), qty INT)"));
        h.sql(&format!("INSERT INTO {table} VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)"));
        let schema = {
            let live = h.cm.live(&h.id).unwrap();
            if name == "sqlite" { None } else { live.current_schema.clone().or(live.current_database.clone()) }
        };
        let obj = ObjectRef::table(schema, table.clone());
        let mut view = TableDataView::new(h.id.clone(), obj, None);
        h.until(&mut view, "rows", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() == 3 && p.model.is_editable()));

        // Cancel: edits are discarded and nothing reaches the database.
        {
            let p = view.panel.as_mut().unwrap();
            p.model.set_cell(0, 1, "changed").unwrap();
            assert!(p.model.has_changes());
            p.discard();
            assert!(!p.model.has_changes());
        }
        assert_eq!(h.query(&format!("SELECT name FROM {table} WHERE id = 1")), vec![vec!["one".to_string()]], "{name}: cancel");

        // Update, insert and delete, saved together.
        let rows = {
            let p = view.panel.as_mut().unwrap();
            let id_col = p.model.columns().iter().position(|c| c.name.eq_ignore_ascii_case("id")).unwrap();
            let name_col = p.model.columns().iter().position(|c| c.name.eq_ignore_ascii_case("name")).unwrap();
            let qty_col = p.model.columns().iter().position(|c| c.name.eq_ignore_ascii_case("qty")).unwrap();
            let row_of =
                |p: &dbiji_app::results::panel::ResultPanel, id: i64| (0..p.model.row_count()).find(|&r| p.model.cell(r, id_col).and_then(|v| v.as_i64()) == Some(id)).unwrap();
            let r1 = row_of(p, 1);
            p.model.set_cell(r1, name_col, "uno").unwrap();
            let r2 = row_of(p, 2);
            p.model.delete_rows(&[r2]).unwrap();
            let new = p.model.add_row(None).unwrap();
            p.model.set_cell(new, id_col, "4").unwrap();
            p.model.set_cell(new, name_col, "four").unwrap();
            p.model.set_cell(new, qty_col, "40").unwrap();
            p.model.row_count()
        };
        assert!(rows >= 3);
        save_and_wait(&mut h, &mut view);
        let got = h.query(&format!("SELECT id, name, qty FROM {table} ORDER BY id"));
        assert_eq!(
            got,
            vec![vec!["1".to_string(), "uno".into(), "10".into()], vec!["3".to_string(), "three".into(), "30".into()], vec!["4".to_string(), "four".into(), "40".into()],],
            "{name}: saved"
        );

        // NULL and duplicate rows.
        {
            let p = view.panel.as_mut().unwrap();
            let qty_col = p.model.columns().iter().position(|c| c.name.eq_ignore_ascii_case("qty")).unwrap();
            let id_col = p.model.columns().iter().position(|c| c.name.eq_ignore_ascii_case("id")).unwrap();
            let r = (0..p.model.row_count()).find(|&r| p.model.cell(r, id_col).and_then(|v| v.as_i64()) == Some(3)).unwrap();
            p.model.set_cells(vec![(r, qty_col, dbiji_grid::model::CellValue::Value(dbiji_core::Value::Null))]).unwrap();
            let dup = p.model.duplicate_rows(&[r]).unwrap();
            p.model.set_cell(dup[0], id_col, "5").unwrap();
        }
        save_and_wait(&mut h, &mut view);
        let got = h.query(&format!("SELECT id, qty FROM {table} WHERE id IN (3, 5) ORDER BY id"));
        assert_eq!(got.len(), 2, "{name}: duplicate saved");
        assert!(got.iter().all(|r| r[1].is_empty() || r[1].eq_ignore_ascii_case("null")), "{name}: NULL saved: {got:?}");

        h.sql(&format!("DROP TABLE {table}"));
        h.cm.disconnect_all(&h.backend);
        if let Some(p) = url.strip_prefix("sqlite://") {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[test]
fn virtual_model_dictionary_and_virtual_columns() {
    let file = std::env::temp_dir().join(format!("dbiji-vm-e2e-{}.db", uuid::Uuid::new_v4().simple()));
    let url = format!("sqlite://{}", file.display());
    let mut h = H::new(&url);
    h.sql("CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT)");
    h.sql("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customer(id), qty INTEGER, price REAL)");
    h.sql("INSERT INTO customer VALUES (1, 'Ani'), (2, 'Budi')");
    h.sql("INSERT INTO orders VALUES (1, 1, 2, 10.0), (2, 2, 3, 1.5)");
    // Virtual model: dictionary on customer, virtual column on orders.
    let p = h.services.projects.as_ref().unwrap();
    let mut vm = p.virtual_model(&h.id).unwrap_or_default();
    let cust = ObjectRef::table(None, "customer");
    let ord = ObjectRef::table(None, "orders");
    vm.set(
        dbiji_config::project::VirtualTableKey::of(&cust),
        dbiji_config::VirtualTable { dictionary: Some(dbiji_config::project::DictionaryConfig { description_columns: vec!["name".into()] }), ..Default::default() },
    );
    vm.set(
        dbiji_config::project::VirtualTableKey::of(&ord),
        dbiji_config::VirtualTable {
            virtual_columns: vec![dbiji_config::project::VirtualColumn { name: "amount".into(), expression: "qty * price".into(), ..Default::default() }],
            ..Default::default()
        },
    );
    p.save_virtual_model(&h.id, &vm).unwrap();
    let mut view = TableDataView::new(h.id.clone(), ord, None);
    h.until(&mut view, "rows", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() == 2));
    let fk_col = view.panel.as_ref().unwrap().model.columns().iter().position(|c| c.name == "customer_id").unwrap();
    h.until(&mut view, "dictionary", |v| v.panel.as_ref().unwrap().model.dictionary(fk_col).is_some());
    let p = view.panel.as_ref().unwrap();
    let d = p.model.dictionary(fk_col).unwrap();
    assert_eq!(d.describe("1"), Some("Ani"));
    let vcol = p.model.columns().len();
    assert_eq!(p.model.cell(0, vcol).and_then(|v| v.as_f64()), Some(20.0), "virtual column qty * price");
    h.cm.disconnect_all(&h.backend);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn server_side_sort_asc_desc() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let table = format!("dbiji_sort_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        h.sql(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(10))"));
        h.sql(&format!("INSERT INTO {table} VALUES (2, 'b'), (3, 'c'), (1, 'a')"));
        let mut view = TableDataView::new(h.id.clone(), ObjectRef::table(None, table.clone()), None);
        h.until(&mut view, "rows", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() == 3));
        for (dir, expect) in [("", [1, 2, 3]), (" DESC", [3, 2, 1])] {
            {
                let mut cx = AppContext {
                    backend: &h.backend,
                    conns: &mut h.cm,
                    services: &h.services,
                    settings: &mut h.settings,
                    commands: &mut h.commands,
                    toasts: &mut h.toasts,
                    ops: &mut h.ops,
                    keymap: &h.keymap,
                    tab_active: true,
                    current_tab: Some(1),
                    popup: false,
                };
                let d = cx.conns.live(&h.id).unwrap().dialect.clone();
                let order = format!("{}{dir}", d.quote_ident("id"));
                view.panel.as_mut().unwrap().handle_for_tests(dbiji_grid::GridAction::ServerQueryChanged { where_clause: None, order_by: Some(order) }, &mut cx);
            }
            h.until(&mut view, "sorted", |v| {
                let p = v.panel.as_ref().unwrap();
                !p.src.is_fetching() && p.model.row_count() == 3 && (0..3).all(|r| p.model.cell(r, 0).and_then(|x| x.as_i64()) == Some(expect[r]))
            });
        }
        h.sql(&format!("DROP TABLE {table}"));
        let _ = name;
        if let Some(p) = url.strip_prefix("sqlite://") {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Production-style connection (manual commit) on PostgreSQL / CockroachDB:
/// changing the ORDER BY while a big table is still loading cancels the old
/// query, which used to leave the transaction aborted ("25P02 current
/// transaction is aborted").
#[test]
fn manual_commit_reorder_while_loading() {
    for (name, url) in servers().into_iter().filter(|(n, _)| matches!(*n, "postgres" | "cockroach")) {
        let table = format!("dbiji_big_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let setup = H::new(&url);
        setup.sql(&format!("CREATE TABLE {table} AS SELECT g AS id, repeat('x', 100) AS pad FROM generate_series(1, 200000) g"));
        let mut h = H::new_with(&url, |cfg| cfg.auto_commit = Some(false));
        let mut view = TableDataView::new(h.id.clone(), ObjectRef::table(None, table.clone()), None);
        h.until(&mut view, "first page", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() > 0));
        for (dir, first) in [(" DESC", 200000), ("", 1), (" DESC", 200000)] {
            {
                let mut cx = AppContext {
                    backend: &h.backend,
                    conns: &mut h.cm,
                    services: &h.services,
                    settings: &mut h.settings,
                    commands: &mut h.commands,
                    toasts: &mut h.toasts,
                    ops: &mut h.ops,
                    keymap: &h.keymap,
                    tab_active: true,
                    current_tab: Some(1),
                    popup: false,
                };
                let order = format!("id{dir}");
                view.panel.as_mut().unwrap().handle_for_tests(dbiji_grid::GridAction::ServerQueryChanged { where_clause: None, order_by: Some(order) }, &mut cx);
            }
            h.until(&mut view, "re-ordered", |v| {
                let p = v.panel.as_ref().unwrap();
                if let Some((msg, true)) = p.status.as_ref().or(p.view.toolbar.notice.as_ref()) {
                    panic!("[{name}] {msg}");
                }
                p.model.row_count() > 0 && p.model.cell(0, 0).and_then(|x| x.as_i64()) == Some(first)
            });
        }
        // The view's manual-commit transaction holds a lock on the table
        // until its session is closed (asynchronously): retry the DROP.
        drop(view);
        let (backend, id) = (h.backend.clone(), h.id.clone());
        h.cm.disconnect(&backend, &id);
        drop(h);
        setup.sql("SET lock_timeout = '2s'");
        let t0 = Instant::now();
        while let Err(e) = setup.try_sql(&format!("DROP TABLE {table}")) {
            assert!(t0.elapsed() < Duration::from_secs(60), "[{name}] DROP TABLE: {e}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// "Save as default filter": the table opens with the saved WHERE / ORDER BY
/// until the default is reset (stored in the project's virtual model).
#[test]
fn default_filter_per_table() {
    for (name, url) in servers().into_iter().filter(|(n, _)| matches!(*n, "sqlite" | "postgres" | "mysql")) {
        let mut h = H::new(&url);
        let table = format!("dbiji_deflt_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        h.sql(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(10))"));
        h.sql(&format!("INSERT INTO {table} VALUES (1, 'a'), (2, 'b'), (3, 'c')"));
        let obj = ObjectRef::table(None, table.clone());
        let mut view = TableDataView::new(h.id.clone(), obj.clone(), None);
        h.until(&mut view, "rows", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() == 3));
        let act = |h: &mut H, view: &mut TableDataView, a: dbiji_grid::GridAction| {
            let mut cx = AppContext {
                backend: &h.backend,
                conns: &mut h.cm,
                services: &h.services,
                settings: &mut h.settings,
                commands: &mut h.commands,
                toasts: &mut h.toasts,
                ops: &mut h.ops,
                keymap: &h.keymap,
                tab_active: true,
                current_tab: Some(1),
                popup: false,
            };
            view.panel.as_mut().unwrap().handle_for_tests(a, &mut cx);
        };
        act(&mut h, &mut view, dbiji_grid::GridAction::DefaultFilter { where_clause: Some("id > 1".into()), order_by: Some("id DESC".into()) });
        let mut again = TableDataView::new(h.id.clone(), obj.clone(), None);
        h.until(&mut again, "default filter applied", |v| {
            v.panel.as_ref().is_some_and(|p| p.model.row_count() == 2 && p.model.cell(0, 0).and_then(|x| x.as_i64()) == Some(3) && p.view.toolbar.filter_text == "id > 1")
        });
        act(&mut h, &mut again, dbiji_grid::GridAction::DefaultFilter { where_clause: None, order_by: None });
        let mut reset = TableDataView::new(h.id.clone(), obj.clone(), None);
        h.until(&mut reset, "default removed", |v| v.panel.as_ref().is_some_and(|p| p.model.row_count() == 3 && p.view.toolbar.filter_text.is_empty()));
        drop((view, again, reset));
        h.sql(&format!("DROP TABLE {table}"));
        let _ = name;
    }
}
