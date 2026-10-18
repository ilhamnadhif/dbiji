//! Headless end-to-end tests of the SQL editor tab: the tab is drawn in real
//! egui frames (no window) and executes against a real server.
//! Set `DBIJI_TEST_PG` / `DBIJI_TEST_MYSQL` / `DBIJI_TEST_MARIADB` to run.

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

fn servers() -> Vec<(&'static str, String)> {
    // SQLite is embedded: always tested, on a throw-away file.
    let file = std::env::temp_dir().join(format!("dbiji-editor-e2e-{}.db", uuid::Uuid::new_v4().simple()));
    let mut out = vec![("sqlite", format!("sqlite://{}", file.display()))];
    for (name, var) in [("postgres", "DBIJI_TEST_PG"), ("mysql", "DBIJI_TEST_MYSQL"), ("mariadb", "DBIJI_TEST_MARIADB")] {
        if let Ok(url) = std::env::var(var)
            && !url.trim().is_empty() {
                out.push((name, url));
            }
    }
    out
}

impl H {
    fn new(url: &str) -> H {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let backend = Arc::new(Backend::new().unwrap());
        let ctx = egui::Context::default();
        backend.set_context(ctx.clone());
        let (mut cfg, password) = dbiji_config::parse_connection_url(url).unwrap();
        cfg.name = "editor-e2e".into();
        cfg.save_password = true;
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
        let dir = std::env::temp_dir().join(format!("dbiji-editor-e2e-{}", uuid::Uuid::new_v4().simple()));
        let services = Services::ephemeral(&dir);
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
            _dir: tempdir::Dir(dir),
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

    /// Draw frames until `done` or the timeout.
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

#[test]
fn editor_executes_statements_scripts_parameters_and_explain() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        let mut tab = SqlEditorTab::new(Some(h.id.clone()), None, String::new(), None, false, "e2e".into());
        h.until(&mut tab, "session", |t| t.session_ready());
        let ctx = h.ctx.clone();

        // Statement under the cursor: only the second statement runs.
        tab.set_text(&ctx, "select 1 as a;\nselect 2 as b, 3 as c;\n");
        tab.set_cursor(&ctx, 18);
        h.action(&mut tab, Action::ExecuteStatement);
        h.until(&mut tab, "statement", |t| !t.is_running() && t.result_count() > 0);
        assert_eq!(tab.result_count(), 1, "{name}");
        assert_eq!(tab.result_rows(0), Some(1), "{name}");

        // Whole script: two row results plus statistics.
        h.action(&mut tab, Action::ExecuteScript);
        h.until(&mut tab, "script", |t| !t.is_running() && t.stats_len() == 2);
        assert!(tab.result_count() >= 2, "{name}: results {}", tab.result_count());

        // Parameters are asked for, then substituted.
        tab.set_text(&ctx, "select :answer as x");
        tab.set_cursor(&ctx, 3);
        h.action(&mut tab, Action::ExecuteStatement);
        h.frame(&mut tab);
        assert_eq!(tab.pending_params(), Some(vec!["answer".to_string()]), "{name}");
        let before = tab.result_count();
        h.with_cx(|cx| tab.answer_params(cx, &["42"]));
        h.until(&mut tab, "parameter run", |t| !t.is_running() && t.result_count() >= before);
        assert_eq!(tab.variables().get("answer").map(String::as_str), Some("42"));

        // @set variables and @echo are handled by the client.
        tab.set_text(&ctx, "@set v = 7\n@echo value ${v}\nselect ${v} as v;");
        h.action(&mut tab, Action::ExecuteScript);
        h.until(&mut tab, "client commands", |t| !t.is_running() && t.output_text().contains("value 7"));

        // Errors are reported and marked in the editor.
        tab.set_text(&ctx, "select * from dbiji_no_such_table_xyz");
        tab.set_cursor(&ctx, 2);
        h.action(&mut tab, Action::ExecuteStatement);
        h.until(&mut tab, "error", |t| !t.is_running() && t.output_text().contains("ERROR"));
        assert!(tab.diagnostics_len() >= 1, "{name}: error marker");

        // Dangerous statements need a confirmation.
        let table = format!("dbiji_ed_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        tab.set_text(&ctx, &format!("create table {table} (id int);\ninsert into {table} values (1);\ndelete from {table};\ndrop table {table};"));
        let mark = tab.output_text().len();
        h.action(&mut tab, Action::ExecuteScript);
        h.frame(&mut tab);
        let reasons = tab.pending_confirmation().expect("confirmation for DELETE without WHERE and DROP");
        assert!(reasons.len() >= 2, "{name}: {reasons:?}");
        h.with_cx(|cx| tab.answer_confirmation(cx, true));
        h.until(&mut tab, "dangerous script", |t| !t.is_running() && t.stats_len() == 4);
        assert!(!tab.output_text()[mark..].contains("ERROR"), "{name}: {}", &tab.output_text()[mark..]);

        // Explain shows a plan result.
        tab.set_text(&ctx, "select 1");
        h.action(&mut tab, Action::ExplainPlan);
        h.until(&mut tab, "explain", |t| !t.is_running() && (0..t.result_count()).any(|i| t.result_is_plan(i)));

        // Formatting rewrites the text.
        tab.set_text(&ctx, "select a,b from t where x=1");
        h.action(&mut tab, Action::FormatSql);
        assert!(tab.text().contains("SELECT"), "{name}: formatted: {}", tab.text());
        h.cm.disconnect_all(&h.backend);
        if let Some(p) = url.strip_prefix("sqlite://") {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[test]
fn read_only_connections_block_writes() {
    for (name, url) in servers() {
        let mut h = H::new(&url);
        h.cm.config_mut(&h.id).unwrap().read_only = true;
        let mut tab = SqlEditorTab::new(Some(h.id.clone()), None, "create table dbiji_never (id int)".into(), None, false, "ro".into());
        h.until(&mut tab, "session", |t| t.session_ready());
        h.action(&mut tab, Action::ExecuteStatement);
        h.frame(&mut tab);
        assert!(!tab.is_running(), "{name}: must not run");
        assert!(tab.output_text().contains("read-only"), "{name}: {}", tab.output_text());
        if let Some(p) = url.strip_prefix("sqlite://") {
            let _ = std::fs::remove_file(p);
        }
    }
}
