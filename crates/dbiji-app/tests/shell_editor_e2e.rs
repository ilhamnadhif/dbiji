//! Headless end-to-end tests of the editor with the shell languages: the tab
//! is drawn in real egui frames (no window) and executes against a live
//! MongoDB or Redis server.
//!
//! Set `DBIJI_TEST_MONGO` (`host:port`) and/or `DBIJI_TEST_REDIS`
//! (`host:port`) to run them; without the variables the test is skipped.

use dbiji_app::backend::Backend;
use dbiji_app::command::AppCommand;
use dbiji_app::conn::{ConnectionManager, Credentials, DirectNet, MemorySecrets, SecretSource};
use dbiji_app::cx::AppContext;
use dbiji_app::keys::{Action, Keymap};
use dbiji_app::ops::Operations;
use dbiji_app::services::{Services, Settings};
use dbiji_app::tabs::TabView;
use dbiji_app::tabs::sql_editor::SqlEditorTab;
use dbiji_app::toasts::Toasts;
use dbiji_core::config::{SecretSlot, secret_key};
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
    _dir: tempdir::Dir,
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl H {
    fn new(url: &str) -> H {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let backend = Arc::new(Backend::new().unwrap());
        let ctx = egui::Context::default();
        backend.set_context(ctx.clone());
        let (mut cfg, password) = dbiji_config::parse_connection_url(url).unwrap();
        cfg.name = "shell-e2e".into();
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
            assert!(t0.elapsed() < Duration::from_secs(20), "connect timeout");
            cm.poll(&backend);
            std::thread::sleep(Duration::from_millis(10));
        }
        let dir = std::env::temp_dir().join(format!("dbiji-shell-e2e-{}", uuid::Uuid::new_v4().simple()));
        let services = Services::ephemeral(&dir);
        let settings = Settings::default();
        let (keymap, _) = Keymap::from_config(&settings.keymap, dbiji_app::keys::IS_MAC);
        H { backend, ctx, cm, services, settings, keymap, toasts: Toasts::default(), ops: Operations::default(), commands: Vec::new(), time: 0.0, id, _dir: tempdir::Dir(dir) }
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
            popup: false,
        };
        f(&mut cx)
    }

    fn frame(&mut self, tab: &mut SqlEditorTab) {
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
    }

    fn until(&mut self, tab: &mut SqlEditorTab, what: &str, mut done: impl FnMut(&SqlEditorTab) -> bool) {
        let t0 = Instant::now();
        loop {
            self.frame(tab);
            if done(tab) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}; output:\n{}", tab.output_text());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn action(&mut self, tab: &mut SqlEditorTab, a: Action) {
        self.with_cx(|cx| tab.on_action(a, cx));
    }
}

fn mongo_url() -> Option<String> {
    let addr = std::env::var("DBIJI_TEST_MONGO").ok()?;
    Some(format!("mongodb://{addr}/dbiji_editor_e2e"))
}

fn redis_url() -> Option<String> {
    let addr = std::env::var("DBIJI_TEST_REDIS").ok()?;
    Some(format!("redis://{addr}/0"))
}

#[test]
fn the_editor_runs_shell_statements() {
    let mongo = mongo_url();
    let redis = redis_url();
    if mongo.is_none() && redis.is_none() {
        eprintln!("DBIJI_TEST_MONGO / DBIJI_TEST_REDIS are not set: skipping");
        return;
    }

    for (name, url) in [("mongodb", mongo), ("redis", redis)] {
        let Some(url) = url else { continue };
        let mut h = H::new(&url);
        let mut tab = SqlEditorTab::new(Some(h.id.clone()), None, String::new(), None, false, "shell e2e".into());
        h.until(&mut tab, "session", |t| t.session_ready());
        let ctx = h.ctx.clone();

        if name == "mongodb" {
            // Chained calls and multi-line documents stay one statement; the
            // script runs one statement after another.
            // No `drop` / `deleteMany({})`: those ask for a confirmation.
            tab.set_text(&ctx, "db.items.deleteMany({n: {$gte: 0}});\ndb.items.insertMany([{n: 1}, {n: 2}]);\ndb.items.find({}).sort({n: 1})");
            h.action(&mut tab, Action::ExecuteScript);
            h.until(&mut tab, "script", |t| !t.is_running() && t.stats_len() == 3);
            // Only the `find` produces rows; the other two report affected rows.
            assert_eq!(tab.result_count(), 1, "{name}: {}", tab.output_text());
            assert_eq!(tab.result_rows(0), Some(2), "{name}: {}", tab.output_text());

            // The statement under the cursor runs alone.
            tab.set_text(&ctx, "db.items.countDocuments({})\ndb.items.find({n: 2})");
            tab.set_cursor(&ctx, 2);
            h.action(&mut tab, Action::ExecuteStatement);
            h.until(&mut tab, "statement", |t| !t.is_running() && t.result_count() > 0);
            assert_eq!(tab.result_rows(0), Some(1), "{name}: {}", tab.output_text());

            // Errors are reported.
            tab.set_text(&ctx, "db.items.mapReduce()");
            h.action(&mut tab, Action::ExecuteStatement);
            h.until(&mut tab, "error", |t| !t.is_running() && t.output_text().contains("ERROR"));

            // The catalog is browsable: databases are shown as schemas.
            let id = h.id.clone();
            h.with_cx(|cx| {
                let live = cx.conns.live(&id).expect("live");
                assert_eq!(live.driver.info().capabilities.schema_label, "Databases");
            });
        } else {
            // One command per line.
            tab.set_text(&ctx, "SET shell:e2e 1\nGET shell:e2e\nDEL shell:e2e");
            h.action(&mut tab, Action::ExecuteScript);
            h.until(&mut tab, "script", |t| !t.is_running() && t.stats_len() == 3);
            assert!(tab.output_text().contains("OK") || tab.result_count() >= 1, "{name}: {}", tab.output_text());
            // A server error is shown, not hidden.
            tab.set_text(&ctx, "NOSUCHCOMMAND x");
            h.action(&mut tab, Action::ExecuteStatement);
            h.until(&mut tab, "error", |t| !t.is_running() && t.output_text().contains("ERROR"));
        }
    }
}
