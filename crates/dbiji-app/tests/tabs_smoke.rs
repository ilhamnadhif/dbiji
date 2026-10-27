//! Smoke test: every tab kind is drawn for a number of headless egui frames
//! against an embedded SQLite database (always available) and must not panic.

use dbiji_app::backend::Backend;
use dbiji_app::command::AppCommand;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets};
use dbiji_app::cx::AppContext;
use dbiji_app::keys::Keymap;
use dbiji_app::ops::Operations;
use dbiji_app::services::{Services, Settings};
use dbiji_app::tabs::TabView;
use dbiji_app::toasts::Toasts;
use dbiji_core::meta::{ObjectKind, ObjectRef, SchemaRef};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    fn new() -> H {
        let dir = std::env::temp_dir().join(format!("dbiji-smoke-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("smoke.db");
        let backend = Arc::new(Backend::new().unwrap());
        let ctx = egui::Context::default();
        dbiji_app::theme::setup_fonts(&ctx);
        backend.set_context(ctx.clone());
        let mut cfg = dbiji_core::config::ConnectionConfig::new(dbiji_core::DbKind::Sqlite, "smoke");
        cfg.file_path = Some(db.display().to_string());
        cfg.auth = dbiji_core::config::AuthMethod::None;
        let mut cm = ConnectionManager::new(dbiji_app::driver_registry(), Arc::new(MemorySecrets::default()), Arc::new(DirectNet));
        let id = cfg.id.clone();
        cm.upsert(cfg);
        cm.connect(&backend, &id, Credentials::default()).unwrap();
        let t0 = Instant::now();
        while !cm.is_connected(&id) {
            assert!(cm.errors().is_empty(), "{:?}", cm.errors());
            assert!(t0.elapsed() < Duration::from_secs(10));
            cm.poll(&backend);
            std::thread::sleep(Duration::from_millis(5));
        }
        let s = cm.live(&id).unwrap().main.clone();
        backend
            .block_on(async move {
                s.execute_update("CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)", &[]).await?;
                s.execute_update("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customer(id), total REAL)", &[]).await?;
                s.execute_update("INSERT INTO customer VALUES (1, 'Ani', 'a@x'), (2, 'Budi', NULL)", &[]).await?;
                s.execute_update("INSERT INTO orders VALUES (1, 1, 10.5), (2, 2, 3.25)", &[]).await?;
                s.execute_update("CREATE VIEW big AS SELECT * FROM orders WHERE total > 5", &[]).await
            })
            .unwrap();
        let services = Services::ephemeral(&dir.join("home"));
        let settings = Settings::default();
        let (keymap, _) = Keymap::from_config(&settings.keymap, dbiji_app::keys::IS_MAC);
        H { backend, ctx, cm, services, settings, keymap, toasts: Toasts::default(), ops: Operations::default(), commands: Vec::new(), time: 0.0, id, dir }
    }

    fn frames(&mut self, tab: &mut dyn TabView, n: usize) {
        for _ in 0..n {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
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
                egui::CentralPanel::default().show(ui, |ui| tab.ui(ui, &mut cx));
            });
            out.textures_delta.clear();
            let _ = self.ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            self.cm.poll(&self.backend);
            std::thread::sleep(Duration::from_millis(3));
        }
        self.commands.clear();
    }
}

/// Truncate… in the navigator's menu: the maintenance dialog opened on it empties the table.
#[test]
fn truncate_empties_the_table() {
    let mut h = H::new();
    let orders = ObjectRef::table(Some("main".into()), "orders");
    let mut tab = dbiji_app::tabs::dbtools::MaintenanceTab::new(h.id.clone(), vec![orders]).with_op(Some("truncate".into()));
    h.frames(&mut tab, 3);
    assert_eq!(tab.title(), "Truncate: orders");
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
        assert!(tab.available_ops(&cx).iter().any(|o| o == "truncate"), "{:?}", tab.available_ops(&cx));
        assert_eq!(tab.preview_sql(&cx), ["DELETE FROM main.orders"]);
        tab.start(&mut cx);
    }
    let t0 = Instant::now();
    while tab.is_running() {
        assert!(t0.elapsed() < Duration::from_secs(10), "{}", tab.log_text());
        h.frames(&mut tab, 1);
    }
    assert!(tab.error().is_none() && tab.results().iter().all(|r| r.error.is_none()), "{}", tab.log_text());
    let s = h.cm.live(&h.id).unwrap().main.clone();
    let (_, rows) = h.backend.block_on(async move { s.query_all("SELECT COUNT(*) FROM orders", &[]).await }).unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(0));
}

#[test]
fn every_tab_draws_without_panicking() {
    let mut h = H::new();
    let conn = h.id.clone();
    let schema = SchemaRef::new(None, Some("main".into()));
    let customer = ObjectRef::table(Some("main".into()), "customer");
    let mut tabs: Vec<(&str, Box<dyn TabView>)> = vec![
        ("settings", Box::new(dbiji_app::tabs::settings::SettingsTab::default())),
        ("history", Box::new(dbiji_app::tabs::history::HistoryTab::default())),
        ("editor", Box::new(dbiji_app::tabs::sql_editor::SqlEditorTab::new(Some(conn.clone()), None, "select * from customer;".into(), None, false, "smoke".into()))),
        ("console", Box::new(dbiji_app::tabs::sql_editor::SqlEditorTab::new(Some(conn.clone()), None, String::new(), None, true, "console".into()))),
        ("object", Box::new(dbiji_app::tabs::object::ObjectEditorTab::new(conn.clone(), customer.clone(), dbiji_app::command::ObjectPage::Properties))),
        ("object-data", Box::new(dbiji_app::tabs::object::ObjectEditorTab::new(conn.clone(), customer.clone(), dbiji_app::command::ObjectPage::Data))),
        ("object-ddl", Box::new(dbiji_app::tabs::object::ObjectEditorTab::new(conn.clone(), customer.clone(), dbiji_app::command::ObjectPage::Ddl))),
        (
            "view",
            Box::new(dbiji_app::tabs::object::ObjectEditorTab::new(
                conn.clone(),
                ObjectRef::new(ObjectKind::View, Some("main".into()), "big"),
                dbiji_app::command::ObjectPage::Ddl,
            )),
        ),
        ("data", Box::new(dbiji_app::results::data_tab::TableDataTab::new(conn.clone(), customer.clone(), Some("id > 0".into())))),
        ("er", Box::new(dbiji_app::tabs::er::ErTab::new(conn.clone(), schema.clone(), Vec::new()))),
        ("query-builder", Box::new(dbiji_app::tabs::query_builder::QueryBuilderTab::new(conn.clone(), Some(schema.clone())))),
        ("schema-compare", Box::new(dbiji_app::tabs::compare::SchemaCompareTab::new(Some(conn.clone()), Some(schema.clone())))),
        ("sessions", Box::new(dbiji_app::tabs::admin::SessionManagerTab::new(conn.clone()))),
        ("locks", Box::new(dbiji_app::tabs::admin::LockManagerTab::new(conn.clone()))),
        ("dashboard", Box::new(dbiji_app::tabs::admin::DashboardTab::new(conn.clone()))),
        ("export", Box::new(dbiji_app::tabs::transfer::ExportTab::new(conn.clone(), vec![customer.clone()], None))),
        ("import", Box::new(dbiji_app::tabs::transfer::ImportTab::new(conn.clone(), Some(customer.clone()), Some(schema.clone())))),
        ("migration", Box::new(dbiji_app::tabs::transfer::MigrationTab::new(conn.clone(), vec![customer.clone()]))),
        ("mock", Box::new(dbiji_app::tabs::transfer::MockDataTab::new(conn.clone(), customer.clone()))),
        ("tasks", Box::new(dbiji_app::tabs::tasks::TasksTab::new())),
        ("backup", Box::new(dbiji_app::tabs::dbtools::BackupTab::new(conn.clone(), Some(schema.clone()), false))),
        ("maintenance", Box::new(dbiji_app::tabs::dbtools::MaintenanceTab::new(conn.clone(), vec![customer.clone()]))),
        ("password", Box::new(dbiji_app::tabs::dbtools::ChangePasswordTab::new(conn.clone()))),
        ("ai", Box::new(dbiji_app::tabs::ai::AiAssistantTab::new(Some(conn.clone())))),
        ("data-compare", Box::new(dbiji_app::tabs::data_compare::DataCompareTab::new(Some(conn.clone()), Some(customer.clone())))),
        ("designer-new", Box::new(dbiji_app::tabs::table_designer::TableDesignerTab::create(conn.clone(), schema.clone()))),
        ("designer-edit", Box::new(dbiji_app::tabs::table_designer::TableDesignerTab::edit(conn.clone(), customer.clone()))),
        ("search", Box::new(dbiji_app::tabs::search::SearchTab::new(Some(conn.clone()), false))),
        ("search-data", Box::new(dbiji_app::tabs::search::SearchTab::new(Some(conn.clone()), true))),
    ];
    for (name, tab) in tabs.iter_mut() {
        let t0 = Instant::now();
        h.frames(tab.as_mut(), 40);
        assert!(!tab.title().is_empty(), "{name}: title");
        eprintln!("{name}: {:?}", t0.elapsed());
    }
    // The settings tab in every section.
    let mut settings = dbiji_app::tabs::settings::SettingsTab::default();
    for _ in 0..12 {
        h.frames(&mut settings, 3);
        settings.next_section_for_tests();
    }
}

#[test]
fn connection_dialog_fits_the_screen() {
    let mut h = H::new();
    let mut dialogs = dbiji_app::dialogs::Dialogs::default();
    dialogs.new_connection(Some(dbiji_core::DbKind::Postgres), None, &h.cm, Default::default());
    let screen = egui::vec2(1280.0, 760.0);
    for _ in 0..6 {
        h.time += 1.0 / 60.0;
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)), time: Some(h.time), ..Default::default() };
        let ctx = h.ctx.clone();
        let mut out = ctx.run_ui(input, |ui| {
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
                current_tab: None,
                popup: false,
            };
            dialogs.show(ui.ctx(), &mut cx);
        });
        out.textures_delta.clear();
    }
    let layer = h.ctx.memory(|m| m.areas().top_layer_id(egui::Order::Middle)).expect("dialog drawn");
    let rect = h.ctx.memory(|m| m.area_rect(layer.id)).expect("dialog rect");
    assert!(rect.height() < screen.y - 40.0 && rect.top() >= 0.0 && rect.bottom() <= screen.y, "dialog {rect:?} does not fit {screen:?}");
}
