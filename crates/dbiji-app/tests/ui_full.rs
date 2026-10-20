//! Full application walkthrough: the real `DbijiApp` runs in a headless
//! egui_kittest harness and is driven like a user would — clicks on buttons,
//! menus and navigator rows, typing in the editor, keyboard shortcuts. Uses
//! the built-in SQLite sample database (always available) and a throw-away
//! configuration directory; no Keychain, no network.

use dbiji_app::app::DbijiApp;
use dbiji_app::backend::Backend;
use dbiji_app::command::AppCommand;
use dbiji_app::conn::{ConnectionManager, DirectNet, MemorySecrets};
use dbiji_app::keys::Action;
use dbiji_app::services::{Services, Settings};
use dbiji_app::tabs::sql_editor::SqlEditorTab;
use dbiji_app::toasts::ToastKind;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::sync::Arc;
use std::time::{Duration, Instant};

type H = Harness<'static, DbijiApp>;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };

fn harness(dir: &std::path::Path) -> H {
    let mut settings = Settings::default();
    // The update question would cover part of the window.
    settings.updates.check_automatically = Some(false);
    harness_with(dir, settings)
}

fn harness_with(dir: &std::path::Path, settings: Settings) -> H {
    let conns = ConnectionManager::new(dbiji_app::driver_registry(), Arc::new(MemorySecrets::default()), Arc::new(DirectNet));
    harness_conns(dir, settings, conns)
}

fn harness_conns(dir: &std::path::Path, mut settings: Settings, conns: ConnectionManager) -> H {
    let backend = Arc::new(Backend::new().unwrap());
    let services = Services::ephemeral(&dir.join("home"));
    settings.general.restore_session = false;
    let h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_max_steps(400).with_step_dt(1.0 / 60.0).build_eframe(move |cc| {
        dbiji_app::theme::setup_fonts(&cc.egui_ctx);
        backend.set_context(cc.egui_ctx.clone());
        DbijiApp::new(backend, conns, services, settings)
    });
    // After the harness set its own theme, as eframe's start-up does.
    dbiji_app::theme::apply_settings(&h.ctx, &h.state().settings);
    h
}

/// Run frames, giving background tasks a moment between them.
fn frames(h: &mut H, n: usize) {
    for _ in 0..n {
        h.step();
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn wait(h: &mut H, what: &str, mut cond: impl FnMut(&mut H) -> bool) {
    let t0 = Instant::now();
    while !cond(h) {
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; labels: {:?}", labels(h));
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Let a second of simulated time pass, so the next click is not taken as
/// part of a double / triple click.
fn pause(h: &mut H) {
    let t = h.ctx.input(|i| i.time);
    h.input_mut().time = Some(t + 1.0);
    h.step();
}

fn labels(h: &H) -> Vec<String> {
    h.query_all_by_label_contains("").filter_map(|n| n.accesskit_node().label()).collect()
}

fn has(h: &H, label: &str) -> bool {
    h.query_all_by_label_contains(label).next().is_some()
}

/// Click the first widget whose label contains `label`.
fn click(h: &mut H, label: &str) {
    pause(h);
    {
        let Some(n) = h.query_all_by_label_contains(label).next() else {
            panic!("no widget labelled {label:?}; labels: {:?}", labels(h));
        };
        n.click();
    }
    frames(h, 2);
}

/// Fails on any error toast since the last check.
fn no_errors(h: &mut H, step: &str) {
    let errors: Vec<String> =
        h.state_mut().toasts.take_log().into_iter().filter(|t| t.kind == ToastKind::Error).map(|t| format!("{}: {}", t.title, t.detail.unwrap_or_default())).collect();
    assert!(errors.is_empty(), "{step}: {errors:#?}");
}

/// Click into the (first visible) SQL editor text.
fn click_editor(h: &mut H) {
    {
        let n = h.query_all_by_role(egui::accesskit::Role::MultilineTextInput).next().expect("an editor");
        n.click();
    }
    frames(h, 2);
}

fn focus_tab(h: &mut H, title: &str) {
    if let Some(path) = h.state().dock.find_tab_from(|t| t.view.title() == title) {
        let _ = h.state_mut().dock.set_active_tab(path);
        h.state_mut().dock.set_focused_node_and_surface(path.node_path());
        h.step();
    }
}

fn tab_titles(h: &H) -> Vec<String> {
    h.state().dock.iter_all_tabs().map(|(_, t)| t.view.title()).collect()
}

fn focused_editor(h: &mut H) -> &mut SqlEditorTab {
    let (_, tab) = h.state_mut().dock.find_active_focused().expect("a focused tab");
    tab.view.as_any_mut().downcast_mut::<SqlEditorTab>().expect("the focused tab is a SQL editor")
}

struct Dir(std::path::PathBuf, #[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The UI language is process-wide: the walkthroughs run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn setup() -> (Dir, H, String) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dbiji_core::i18n::set_locale(dbiji_core::i18n::Locale::En);
    let dir = Dir(std::env::temp_dir().join(format!("dbiji-ui-{}", uuid::Uuid::new_v4().simple())), guard);
    std::fs::create_dir_all(dir.0.join("Downloads")).unwrap();
    // Exports and backups go to ~/Downloads by default: never the real one.
    // SAFETY: the walkthroughs run one at a time (SERIAL) and nothing else
    // in this test binary reads the environment concurrently.
    // Help › Check for updates… never contacts GitHub from the tests.
    unsafe {
        std::env::set_var("HOME", &dir.0);
        std::env::set_var("DBIJI_UPDATE_URL", "off");
    }
    let mut h = harness(&dir.0);
    frames(&mut h, 3);
    assert!(has(&h, "Create sample database"), "welcome page: {:?}", labels(&h));
    click(&mut h, "Create sample database");
    wait(&mut h, "the sample connection", |h| {
        let app = h.state();
        app.conns.configs().len() == 1 && app.conns.is_connected(&app.conns.configs()[0].id.clone())
    });
    let conn = h.state().conns.configs()[0].id.clone();
    no_errors(&mut h, "sample database");
    (dir, h, conn)
}

#[test]
fn editor_execute_sort_and_edit() {
    let (_dir, mut h, conn) = setup();
    // ⌘] opens an editor on the selected connection.
    h.key_press_modifiers(CMD, Key::CloseBracket);
    frames(&mut h, 3);
    let titles = tab_titles(&h);
    assert_eq!(focused_editor(&mut h).conn.as_deref(), Some(conn.as_str()), "tabs: {titles:?}");
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    h.event(egui::Event::Text("select id, name from customer order by id".into()));
    frames(&mut h, 2);
    assert_eq!(focused_editor(&mut h).text(), "select id, name from customer order by id");
    h.key_press_modifiers(CMD, Key::Enter);
    wait(&mut h, "query result", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == 1 && e.result_rows(0).is_some_and(|n| n > 0)
    });
    assert!(!focused_editor(&mut h).result_is_error(0));
    no_errors(&mut h, "execute");
    assert!(has(&h, "Data grid,"), "grid shown: {:?}", labels(&h));

    // Header click: ascending → descending → unsorted.
    let body = h.query_all_by_label_contains("Data grid,").next().unwrap().rect();
    let header = egui::pos2(body.left() + 12.0, body.top() - 8.0);
    for _ in 0..2 {
        h.event(egui::Event::PointerMoved(header));
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: header, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        }
        frames(&mut h, 4);
        std::thread::sleep(Duration::from_millis(400));
        frames(&mut h, 4);
    }
    no_errors(&mut h, "sort");

    // Back to the editor: format, explain, script execution.
    click_editor(&mut h);
    h.key_press_modifiers(CMD | Modifiers::SHIFT, Key::F);
    frames(&mut h, 3);
    let text = focused_editor(&mut h).text().to_string();
    assert!(text.to_lowercase().contains("order by"), "{text}");
    h.key_press_modifiers(CMD | Modifiers::SHIFT, Key::E);
    wait(&mut h, "explain plan", |h| {
        let e = focused_editor(h);
        !e.is_running() && (0..e.result_count()).any(|i| e.result_is_plan(i))
    });
    no_errors(&mut h, "explain");
}

/// A navigator row (the navigator is the left panel).
fn nav_row<'a>(h: &'a H, label: &'a str) -> Option<egui_kittest::Node<'a>> {
    h.query_all_by_label(label).find(|n| n.rect().left() < 120.0 && n.rect().top() > 60.0)
}

fn nav_double_click(h: &mut H, label: &str) {
    pause(h);
    {
        let n = nav_row(h, label).unwrap_or_else(|| panic!("no navigator row {label:?}; labels: {:?}", labels(h)));
        n.click();
        n.click();
    }
    frames(h, 3);
}

fn label_set(h: &H) -> std::collections::BTreeSet<String> {
    labels(h).into_iter().collect()
}

/// Opens a context menu / menu with `open`, returns the labels it added.
fn menu_items(h: &mut H, open: &dyn Fn(&mut H)) -> Vec<String> {
    let before = label_set(h);
    pause(h);
    open(h);
    frames(h, 2);
    let items: Vec<String> = labels(h).into_iter().filter(|l| !before.contains(l) && !l.trim().is_empty()).collect();
    h.key_press(Key::Escape);
    frames(h, 2);
    items
}

/// Click every item of a (context) menu once, reopening it for each item;
/// sub-menus are walked one level deep. Dialogs that open are closed again.
fn sweep_menu(h: &mut H, name: &str, open: &dyn Fn(&mut H), skip: &[&str]) -> usize {
    let items = menu_items(h, open);
    assert!(!items.is_empty(), "{name}: the menu has items");
    let mut clicked = 0;
    for item in items {
        if skip.iter().any(|s| item.contains(s)) {
            continue;
        }
        pause(h);
        open(h);
        frames(h, 2);
        let before = label_set(h);
        let node = h.query_all_by_label(&item).last().unwrap_or_else(|| panic!("{name}: {item} gone"));
        node.click();
        frames(h, 3);
        // A sub-menu: new items appeared and the menu is still open.
        let sub: Vec<String> = labels(h).into_iter().filter(|l| !before.contains(l) && !l.trim().is_empty()).collect();
        let is_submenu = item.contains('⏵');
        if is_submenu && !sub.is_empty() {
            h.key_press(Key::Escape);
            frames(h, 2);
            for s in sub {
                if skip.iter().any(|k| s.contains(k)) {
                    continue;
                }
                pause(h);
                open(h);
                frames(h, 2);
                let n = h.query_all_by_label(&item).last().unwrap_or_else(|| panic!("{name}: {item} gone"));
                n.click();
                frames(h, 3);
                let n = h.query_all_by_label(&s).last().unwrap_or_else(|| panic!("{name}: {item} › {s} gone"));
                n.click();
                settle(h, &format!("{name} › {item} › {s}"));
                clicked += 1;
            }
            h.key_press(Key::Escape);
            frames(h, 2);
        } else {
            settle(h, &format!("{name} › {item}"));
            clicked += 1;
        }
    }
    clicked
}

/// Let the clicked thing run, fail on error toasts, close dialogs.
fn settle(h: &mut H, step: &str) {
    frames(h, 6);
    let t0 = Instant::now();
    while h.state().ops.running() > 0 && t0.elapsed() < Duration::from_secs(10) {
        frames(h, 2);
    }
    frames(h, 4);
    eprintln!("ok: {step}");
    no_errors(h, step);
    h.key_press(Key::Escape);
    h.state_mut().dialogs = Default::default();
    h.state_mut().tip = None;
    h.state_mut().pending_tx.open = false;
    frames(h, 1);
}

fn sample_db(h: &H, conn: &str) -> std::path::PathBuf {
    h.state().conns.config(conn).and_then(|c| c.file_path.clone()).expect("sample path").into()
}

fn query_one(path: &std::path::Path, sql: &str) -> String {
    let backend = Backend::new().unwrap();
    let mut c = dbiji_core::config::ConnectionConfig::new(dbiji_core::DbKind::Sqlite, "check");
    c.file_path = Some(path.display().to_string());
    let d = dbiji_driver_sqlite::SqliteDriver::new();
    let mut s = backend.block_on(dbiji_core::Driver::connect(&d, &dbiji_core::ConnectParams::from_config(&c, None))).unwrap();
    let v = backend.block_on(dbiji_core::driver::query_scalar(s.as_mut(), sql, &[])).unwrap();
    v.to_text().into_owned()
}

#[test]
fn navigator_data_editing_and_context_menus() {
    let (_dir, mut h, conn) = setup();
    let db = sample_db(&h, &conn);
    wait(&mut h, "Tables folder", |h| nav_row(h, "Tables").is_some());
    nav_double_click(&mut h, "Tables");
    wait(&mut h, "customer table", |h| nav_row(h, "customer").is_some());
    nav_double_click(&mut h, "customer");
    wait(&mut h, "customer data", |h| has(h, "Data grid,") && !has(h, "Data grid, 0 rows"));
    no_errors(&mut h, "open table data");

    // Edit the second cell of the first row, save with ⌘S.
    let body = h.query_all_by_label_contains("Data grid,").next().unwrap().rect();
    let cell = egui::pos2(body.left() + 12.0, body.top() + 9.0);
    h.event(egui::Event::PointerMoved(cell));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: cell, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
    }
    frames(&mut h, 3);
    h.key_press(Key::ArrowRight);
    frames(&mut h, 2);
    let selected = labels(&h).into_iter().find(|l| l.starts_with("Data grid,")).unwrap();
    assert!(selected.contains("selected cell: row 1, name = "), "{selected}");
    h.event(egui::Event::Text("Zed".into()));
    frames(&mut h, 2);
    h.event(egui::Event::Text("ka".into()));
    frames(&mut h, 2);
    h.key_press(Key::Enter);
    frames(&mut h, 2);
    let edited = labels(&h).into_iter().find(|l| l.starts_with("Data grid,")).unwrap();
    assert!(edited.contains("name = Zedka"), "{edited}");
    assert!(h.state().dock.iter_all_tabs().any(|(_, t)| t.view.is_dirty()), "the data tab is dirty");
    h.key_press_modifiers(CMD, Key::S);
    wait(&mut h, "saved", |h| !h.state().dock.iter_all_tabs().any(|(_, t)| t.view.is_dirty()));
    no_errors(&mut h, "save data");
    assert_eq!(query_one(&db, "SELECT COUNT(*) FROM customer WHERE name = 'Zedka'"), "1");

    // Every navigator context menu, on every kind of node.
    let skip = ["Delete", "Drop", "Disconnect", "Duplicate", "Copy name", "Connect", "Hide in navigator"];
    for node in ["dbiji-sample.db", "Tables", "customer", "Views"] {
        let label = node.to_string();
        let open = move |h: &mut H| {
            let n = nav_row(h, &label).unwrap_or_else(|| panic!("no navigator row {label:?}"));
            n.click_secondary();
        };
        let n = sweep_menu(&mut h, node, &open, &skip);
        eprintln!("{node}: {n} items");
    }
    nav_double_click(&mut h, "Views");
    wait(&mut h, "order_total view", |h| nav_row(h, "order_total").is_some());
    let open = |h: &mut H| nav_row(h, "order_total").unwrap().click_secondary();
    sweep_menu(&mut h, "order_total", &open, &skip);
    let titles = tab_titles(&h);
    eprintln!("tabs: {titles:?}");
}

/// The text field on the same row as (to the right of) a label.
fn field_right_of(h: &mut H, label: &str, text: &str) {
    pause(h);
    {
        let l = h.query_all_by_label(label).last().unwrap_or_else(|| panic!("no label {label:?}; labels: {:?}", labels(h)));
        let lr = l.rect();
        let f = h
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .find(|n| n.rect().left() > lr.left() && (n.rect().center().y - lr.center().y).abs() < 12.0)
            .unwrap_or_else(|| panic!("no field next to {label:?}"));
        f.click();
    }
    frames(h, 2);
    h.key_press_modifiers(CMD, Key::A);
    h.event(egui::Event::Text(text.into()));
    frames(h, 2);
}

fn exec_sql(h: &mut H, sql: &str) {
    let conn = h.state().nav.selected.as_ref().and_then(|n| n.connection()).map(str::to_string);
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: conn, schema: None, text: Some(sql.into()) });
    frames(h, 3);
    wait(h, "editor session", |h| focused_editor(h).session_ready());
    click_editor(h);
    h.key_press_modifiers(Modifiers::ALT, Key::X);
    wait(h, "script", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.stats_len() > 0
    });
    no_errors(h, sql);
}

#[test]
fn menus_actions_commands_and_dialogs() {
    let (dir, mut h, conn) = setup();
    let db = sample_db(&h, &conn);

    // Menu bar: every item of every menu.
    let skip = ["Open script", "Quit", "Import connections", "Disconnect all", "Create sample database"];
    for menu in ["File", "Database", "Tools", "View", "Help"] {
        let open = move |h: &mut H| h.query_all_by_label(menu).next().expect("menu").click();
        let n = sweep_menu(&mut h, menu, &open, &skip);
        eprintln!("{menu}: {n} items");
    }
    // View › Toggle navigator ran an odd number of times? Make sure it is shown.
    if nav_row(&h, "dbiji-sample.db").is_none() {
        h.state_mut().push_command(AppCommand::ToggleNavigator);
        frames(&mut h, 3);
    }
    // View › Properties panel too: its labels would be taken for navigator rows.
    h.state_mut().settings.navigator.show_properties = false;

    // Every action, with an editor focused and with a table data tab focused.
    // Quit closes the window; LoadExecutionPlan opens a native file dialog.
    let skip_actions = [Action::Quit, Action::Disconnect, Action::CloseTab, Action::LoadExecutionPlan];
    h.state_mut().nav.select(dbiji_app::nav::NavNode::Connection(conn.clone()));
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: Some("select * from product;\nselect 1;".into()) });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    for &a in Action::ALL.iter().filter(|a| !skip_actions.contains(a)) {
        let ctx = h.ctx.clone();
        h.state_mut().dispatch_action(&ctx, a);
        settle(&mut h, &format!("editor action {a:?}"));
        // Some actions open other tabs: go back to the editor.
        let eid = h.state().dock.iter_all_tabs().filter(|(_, t)| t.view.as_any().is::<SqlEditorTab>()).map(|(_, t)| t.id).last().unwrap();
        let path = h.state().dock.find_tab_from(|t| t.id == eid).unwrap();
        let _ = h.state_mut().dock.set_active_tab(path);
        h.state_mut().dock.set_focused_node_and_surface(path.node_path());
        frames(&mut h, 2);
    }
    h.state_mut().push_command(AppCommand::OpenObject {
        connection: conn.clone(),
        object: dbiji_core::ObjectRef::table(Some("main".into()), "product"),
        page: dbiji_app::command::ObjectPage::Data,
    });
    wait(&mut h, "product data", |h| has(h, "Data grid,") && h.state_mut().dock.find_active_focused().is_some_and(|(_, t)| t.view.title() == "product"));
    for &a in Action::ALL.iter().filter(|a| !skip_actions.contains(a)) {
        let ctx = h.ctx.clone();
        h.state_mut().dispatch_action(&ctx, a);
        settle(&mut h, &format!("data action {a:?}"));
        if let Some(path) = h.state().dock.find_tab_from(|t| t.view.title() == "product") {
            let _ = h.state_mut().dock.set_active_tab(path);
            h.state_mut().dock.set_focused_node_and_surface(path.node_path());
        }
        frames(&mut h, 2);
    }

    // Keyboard: command palette, run a command from it.
    pause(&mut h);
    assert!(!has(&h, "Test connection"), "a connection dialog is open after the actions");
    h.key_press_modifiers(CMD | Modifiers::SHIFT, Key::P);
    frames(&mut h, 3);
    assert!(h.state().dialogs.palette_open, "palette open");
    h.event(egui::Event::Text("Query History".into()));
    frames(&mut h, 2);
    h.key_press(Key::Enter);
    frames(&mut h, 4);
    assert!(tab_titles(&h).iter().any(|t| t.contains("history") || t.contains("History")), "{:?}", tab_titles(&h));
    no_errors(&mut h, "palette");
    assert!(!has(&h, "Test connection"), "a connection dialog is open after the palette");

    // Drop a table through the navigator and the confirmation dialog.
    exec_sql(&mut h, "CREATE TABLE zz_drop_me (id INTEGER PRIMARY KEY);");
    h.state_mut().push_command(AppCommand::RefreshMetadata { connection: conn.clone(), object: None, schema: None });
    frames(&mut h, 5);
    if nav_row(&h, "zz_drop_me").is_none() && nav_row(&h, "customer").is_none() {
        nav_double_click(&mut h, "Tables");
    }
    wait(&mut h, "zz_drop_me in the navigator", |h| nav_row(h, "zz_drop_me").is_some());
    pause(&mut h);
    nav_row(&h, "zz_drop_me").unwrap().click_secondary();
    frames(&mut h, 2);
    click(&mut h, "Drop…");
    assert!(has(&h, "Drop objects"), "drop dialog: {:?}", labels(&h));
    pause(&mut h);
    h.query_all_by_label("Drop").last().unwrap().click();
    wait(&mut h, "dropped", |h| nav_row(h, "zz_drop_me").is_none());
    no_errors(&mut h, "drop");
    assert_eq!(query_one(&db, "SELECT COUNT(*) FROM sqlite_master WHERE name = 'zz_drop_me'"), "0");

    // New SQLite connection through the dialog: test, save and connect.
    let other = dir.0.join("other.db");
    std::fs::write(&other, b"").unwrap();
    pause(&mut h);
    h.query_all_by_label("Database").next().unwrap().click();
    frames(&mut h, 2);
    click(&mut h, "New connection…");
    wait(&mut h, "connection dialog", |h| has(h, "SQLite"));
    pause(&mut h);
    h.query_all_by_label_contains("SQLite").last().unwrap().click();
    frames(&mut h, 3);
    field_right_of(&mut h, "Connection name", "Second db");
    field_right_of(&mut h, "Database file", &other.display().to_string());
    click(&mut h, "Test connection");
    wait(&mut h, "test result", |h| has(h, "Connected:"));
    click(&mut h, "Save and connect");
    wait(&mut h, "second connection", |h| {
        let app = h.state();
        app.conns.configs().iter().any(|c| c.name == "Second db" && app.conns.is_connected(&c.id))
    });
    no_errors(&mut h, "new connection");
    assert!(nav_row(&h, "Second db").is_some());

    // Duplicate, then delete the duplicate through the confirmation.
    pause(&mut h);
    nav_row(&h, "Second db").unwrap().click_secondary();
    frames(&mut h, 2);
    click(&mut h, "Duplicate");
    frames(&mut h, 4);
    let n = h.state().conns.configs().len();
    assert_eq!(n, 3, "duplicated: {:?}", h.state().conns.configs().iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    let dup = h.state().conns.configs().iter().find(|c| c.name != "Second db" && c.name.starts_with("Second db")).map(|c| c.name.clone()).expect("the copy");
    pause(&mut h);
    nav_row(&h, &dup).unwrap().click_secondary();
    frames(&mut h, 2);
    click(&mut h, "Delete");
    frames(&mut h, 2);
    let delete_buttons: Vec<String> = labels(&h).into_iter().filter(|l| l.contains("Delete")).collect();
    pause(&mut h);
    h.query_all_by_label_contains("Delete").last().unwrap_or_else(|| panic!("{delete_buttons:?}")).click();
    frames(&mut h, 4);
    assert_eq!(h.state().conns.configs().len(), 2, "deleted the copy");
    no_errors(&mut h, "delete connection");

    // Disconnect all, then connect again from the navigator.
    pause(&mut h);
    h.query_all_by_label("Database").next().unwrap().click();
    frames(&mut h, 2);
    click(&mut h, "Disconnect all");
    frames(&mut h, 4);
    assert!(!h.state().conns.is_connected(&conn));
    pause(&mut h);
    nav_row(&h, "dbiji-sample.db").unwrap().click_secondary();
    frames(&mut h, 2);
    click(&mut h, " Connect");
    wait(&mut h, "reconnected", |h| h.state().conns.is_connected(&conn));
    no_errors(&mut h, "reconnect");

    // Unsaved changes ask before quitting.
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    click_editor(&mut h);
    h.event(egui::Event::Text("select 42".into()));
    frames(&mut h, 2);
    h.state_mut().push_command(AppCommand::Quit);
    h.step();
    let close = |h: &H| h.output().viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.commands.contains(&egui::ViewportCommand::Close));
    assert!(close(&h), "Quit asks the window to close");
    // The platform answers with a close request: DBiji cancels it and asks.
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    h.step();
    let cancelled = h.output().viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.commands.contains(&egui::ViewportCommand::CancelClose));
    assert!(cancelled, "close cancelled while a tab is dirty");
    frames(&mut h, 2);
    assert!(has(&h, "Unsaved changes"), "quit confirmation: {:?}", labels(&h));
    no_errors(&mut h, "quit");
}

/// Click the combo box on the row of `label`, then the option `option`.
fn combo_pick(h: &mut H, label: &str, option: &str) {
    pause(h);
    {
        let l = h.query_all_by_label(label).last().unwrap_or_else(|| panic!("no label {label:?}; labels: {:?}", labels(h)));
        let lr = l.rect();
        let c = h
            .query_all_by_role(egui::accesskit::Role::ComboBox)
            .find(|n| n.rect().left() > lr.left() && (n.rect().center().y - lr.center().y).abs() < 12.0)
            .unwrap_or_else(|| panic!("no combo box next to {label:?}"));
        c.click();
    }
    frames(h, 3);
    click(h, option);
    frames(h, 3);
}

#[test]
fn settings_grid_menu_and_editor_helpers() {
    let (_dir, mut h, conn) = setup();

    // Settings: every section, then Indonesian and the dark theme.
    h.state_mut().push_command(AppCommand::OpenSettings);
    frames(&mut h, 3);
    for section in [
        "SQL editor",
        "Result sets",
        "Execution",
        "Query history",
        "Navigator",
        "Keyboard shortcuts",
        "Toolbar",
        "Connection types",
        "Network profiles",
        "AI",
        "Security",
        "Accessibility",
        "General",
    ] {
        pause(&mut h);
        let key = format!("  {section}");
        let Some(n) = h.query_all_by_label_contains(&key).next() else {
            panic!("settings section {section:?}; labels: {:?}", labels(&h));
        };
        n.click();
        frames(&mut h, 3);
        no_errors(&mut h, section);
    }
    combo_pick(&mut h, "Theme", "Dark");
    assert_eq!(h.ctx.theme(), egui::Theme::Dark, "dark theme applied");
    combo_pick(&mut h, "Language", "Bahasa Indonesia");
    frames(&mut h, 3);
    assert!(has(&h, "Umum"), "Indonesian UI: {:?}", labels(&h));
    combo_pick(&mut h, "Bahasa", "English");
    frames(&mut h, 3);
    assert!(has(&h, "General"), "back to English: {:?}", labels(&h));
    no_errors(&mut h, "settings");

    // Result grid context menu on a table data tab.
    h.state_mut().push_command(AppCommand::OpenObject {
        connection: conn.clone(),
        object: dbiji_core::ObjectRef::table(Some("main".into()), "product"),
        page: dbiji_app::command::ObjectPage::Data,
    });
    wait(&mut h, "product data", |h| has(h, "Data grid,") && !has(h, "Data grid, 0 rows"));
    let body = h.query_all_by_label_contains("Data grid,").next().unwrap().rect();
    let cell = egui::pos2(body.left() + 40.0, body.top() + 30.0);
    let open = move |h: &mut H| {
        focus_tab(h, "product");
        h.event(egui::Event::PointerMoved(cell));
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: cell, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE });
        }
    };
    let n = sweep_menu(&mut h, "grid", &open, &["Delete", "Paste", "Save", "Filter by value", "Columns"]);
    eprintln!("grid menu: {n} items");
    let rows_label = |h: &H| labels(h).into_iter().find(|l| l.starts_with("Data grid,")).unwrap_or_default();
    // "Hide column" ran: Columns › Show all brings it back.
    assert!(rows_label(&h).contains("4 columns"), "{}", rows_label(&h));
    pause(&mut h);
    open(&mut h);
    frames(&mut h, 2);
    click(&mut h, "Columns");
    click(&mut h, "Show all");
    assert!(rows_label(&h).contains("5 columns"), "{}", rows_label(&h));
    // Filter by value, then clear filter and order from the grid toolbar.
    let all = rows_label(&h).split(',').nth(1).unwrap_or_default().to_string();
    pause(&mut h);
    open(&mut h);
    frames(&mut h, 2);
    click(&mut h, "Filter by value");
    let eq = labels(&h).into_iter().find(|l| l.starts_with("id = ")).unwrap_or_else(|| panic!("{:?}", labels(&h)));
    click(&mut h, &eq);
    wait(&mut h, "filtered", |h| rows_label(h).starts_with("Data grid, 1 rows"));
    no_errors(&mut h, "filter by value");
    click(&mut h, egui_phosphor::regular::X);
    wait(&mut h, "filter cleared", |h| rows_label(h).split(',').nth(1).unwrap_or_default() == all);
    no_errors(&mut h, "clear filter");

    // Column menu (▾ in the header): order, disable the order, filters.
    let column_menu = |h: &mut H| {
        pause(h);
        h.query_all_by_label("Column menu: id").next().unwrap_or_else(|| panic!("no ▾ for id: {:?}", labels(h))).click();
        frames(h, 3);
        assert!(has(h, "Order by id ASC"), "column menu: {:?}", labels(h));
    };
    // The id of the first row, through the grid's selected cell.
    let first_id = |h: &mut H| -> String {
        let body = h.query_all_by_label_contains("Data grid,").next().unwrap().rect();
        let p = egui::pos2(body.left() + 20.0, body.top() + 8.0);
        pause(h);
        h.event(egui::Event::PointerMoved(p));
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        }
        frames(h, 2);
        let l = rows_label(h);
        l.split("id = ").nth(1).map(|v| v.split(',').next().unwrap_or("").trim().to_string()).unwrap_or_default()
    };
    let ascending = first_id(&mut h);
    column_menu(&mut h);
    click(&mut h, "Order by id DESC");
    wait(&mut h, "ordered by id DESC", |h| {
        let id = first_id(h);
        !id.is_empty() && id != ascending
    });
    column_menu(&mut h);
    click(&mut h, "Disable order by id");
    wait(&mut h, "order removed", |h| first_id(h) == ascending);
    column_menu(&mut h);
    let eq = labels(&h).into_iter().find(|l| l.starts_with("id = '")).unwrap_or_else(|| panic!("cell value filters: {:?}", labels(&h)));
    click(&mut h, &eq);
    wait(&mut h, "filtered by the cell value", |h| rows_label(h).starts_with("Data grid, 1 rows"));
    column_menu(&mut h);
    click(&mut h, "Remove all filters and orderings");
    wait(&mut h, "filters removed", |h| rows_label(h).split(',').nth(1).unwrap_or_default() == all);
    // Custom: the condition goes into the filter box for the value.
    column_menu(&mut h);
    click(&mut h, "id > …");
    let filter = h.query_all_by_role(egui::accesskit::Role::TextInput).filter_map(|n| n.accesskit_node().value()).find(|v| v.contains("id") && v.trim_end().ends_with('>'));
    assert!(filter.is_some(), "filter box: {:?}", h.query_all_by_role(egui::accesskit::Role::TextInput).filter_map(|n| n.accesskit_node().value()).collect::<Vec<_>>());
    settle(&mut h, "column menu");

    // Tab: the selected row as a record (column → value), Tab again: the grid.
    let _ = first_id(&mut h);
    h.key_press(Key::Tab);
    frames(&mut h, 3);
    wait(&mut h, "record view", |h| has(h, "Record view, row 1 of"));
    assert!(!has(&h, "Data grid,"), "the grid is replaced by the record view");
    assert!(h.query_all_by_label("name").next().is_some() && h.query_all_by_label("price").next().is_some(), "{:?}", labels(&h));
    h.key_press(Key::ArrowRight);
    frames(&mut h, 2);
    assert!(has(&h, "Record view, row 2 of"), "→ shows the next row: {:?}", labels(&h));
    h.key_press(Key::Tab);
    frames(&mut h, 3);
    wait(&mut h, "grid again", |h| has(h, "Data grid,"));
    // Several rows selected: side by side, one column per row.
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    frames(&mut h, 2);
    h.key_press(Key::Tab);
    frames(&mut h, 3);
    wait(&mut h, "rows side by side", |h| has(h, "Record view, 3 rows"));
    assert!(has(&h, "3 selected rows"), "{:?}", labels(&h));
    // Drag a column border: wider; double-click it: fitted to the values.
    let handle = |h: &H| h.query_all_by_label_contains("Resize Row #").next().map(|n| n.rect()).unwrap_or_else(|| panic!("{:?}", labels(h)));
    let start = handle(&h).center();
    pause(&mut h);
    h.event(egui::Event::PointerMoved(start));
    h.event(egui::Event::PointerButton { pos: start, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    frames(&mut h, 1);
    for dx in [20.0, 40.0, 80.0] {
        h.event(egui::Event::PointerMoved(egui::pos2(start.x + dx, start.y)));
        frames(&mut h, 1);
    }
    h.event(egui::Event::PointerButton { pos: egui::pos2(start.x + 80.0, start.y), button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    frames(&mut h, 2);
    let dragged = handle(&h).center().x;
    assert!((dragged - start.x - 80.0).abs() < 6.0, "dragged from {} to {dragged}", start.x);
    pause(&mut h);
    for _ in 0..2 {
        let p = handle(&h).center();
        h.event(egui::Event::PointerMoved(p));
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
        frames(&mut h, 1);
    }
    frames(&mut h, 2);
    assert!((handle(&h).center().x - dragged).abs() > 6.0, "double-click fits the column");
    h.key_press(Key::Tab);
    frames(&mut h, 3);
    wait(&mut h, "grid after the rows", |h| has(h, "Data grid,"));
    settle(&mut h, "record view");

    // Editor helpers: autocomplete, parameters, find, comment toggling.
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    click_editor(&mut h);
    h.event(egui::Event::Text("select * from cust".into()));
    frames(&mut h, 2);
    h.key_press_modifiers(Modifiers::CTRL, Key::Space);
    wait(&mut h, "completion popup", |h| h.query_all_by_label("customer").any(|n| n.rect().left() > 300.0));
    h.key_press(Key::Enter);
    frames(&mut h, 3);
    assert!(focused_editor(&mut h).text().contains("customer"), "{}", focused_editor(&mut h).text());
    let ctx = h.ctx.clone();
    focused_editor(&mut h).set_text(&ctx, "select * from customer where id = :id");
    frames(&mut h, 2);
    click_editor(&mut h);
    h.key_press_modifiers(CMD, Key::Enter);
    wait(&mut h, "parameter prompt", |h| focused_editor(h).pending_params().is_some());
    assert!(has(&h, "id"), "parameter dialog shown");
    // The value field has the focus: type and press Enter.
    h.event(egui::Event::Text("1".into()));
    frames(&mut h, 2);
    h.key_press(Key::Enter);
    wait(&mut h, "parameterised result", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == 1 && e.result_rows(0) == Some(1)
    });
    no_errors(&mut h, "parameters");
    click_editor(&mut h);
    h.key_press_modifiers(CMD, Key::Slash);
    frames(&mut h, 2);
    assert!(focused_editor(&mut h).text().starts_with("--"), "{}", focused_editor(&mut h).text());
    h.key_press_modifiers(CMD, Key::F);
    frames(&mut h, 3);
    assert!(has(&h, "Aa") && has(&h, ".*"), "find bar: {:?}", labels(&h));
    no_errors(&mut h, "editor helpers");
}

/// Text field by its hint text.
fn field_with_hint(h: &mut H, hint: &str, text: &str) {
    pause(h);
    {
        let f = h
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .find(|n| n.accesskit_node().placeholder().is_some_and(|p| p.contains(hint)))
            .unwrap_or_else(|| panic!("no field with hint {hint:?}"));
        f.click();
    }
    frames(h, 2);
    h.key_press_modifiers(CMD, Key::A);
    h.event(egui::Event::Text(text.into()));
    frames(h, 2);
}

fn wait_file(h: &mut H, dir: &std::path::Path, what: &str, pred: impl Fn(&str) -> bool) -> std::path::PathBuf {
    let mut found = None;
    wait(h, what, |_| {
        found = std::fs::read_dir(dir)
            .ok()
            .and_then(|d| d.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| pred(&n.to_string_lossy())) && p.metadata().is_ok_and(|m| m.len() > 0)));
        found.is_some()
    });
    found.unwrap()
}

fn nav_menu(h: &mut H, row: &str, items: &[&str]) {
    pause(h);
    nav_row(h, row).unwrap_or_else(|| panic!("no navigator row {row:?}")).click_secondary();
    frames(h, 2);
    for item in items {
        click(h, item);
    }
    frames(h, 4);
}

#[test]
fn tool_tabs_run() {
    let (dir, mut h, conn) = setup();
    let db = sample_db(&h, &conn);
    let downloads = dir.0.join("Downloads");
    nav_double_click(&mut h, "Tables");
    wait(&mut h, "tables", |h| nav_row(h, "customer").is_some());

    // Export the customer table to CSV (default format) in ~/Downloads.
    nav_menu(&mut h, "customer", &["Export data"]);
    click(&mut h, "Run export");
    // Named database (SQLite: the file) _ table _ date _ time.
    let date = chrono::Local::now().format("%Y%m%d").to_string();
    let csv = wait_file(&mut h, &downloads, "export file", |n| n.starts_with(&format!("dbiji-sample_customer_{date}_")) && n.ends_with(".csv"));
    let lines = std::fs::read_to_string(&csv).unwrap().lines().count();
    assert!(lines > 10, "{} has {lines} lines", csv.display());
    settle(&mut h, "export");

    // Backup of the SQLite database.
    nav_menu(&mut h, "dbiji-sample.db", &["Tools ⏵", "Backup"]);
    click(&mut h, "Start backup");
    let backup = wait_file(&mut h, &dir.0, "backup file", |n| n.starts_with("dbiji-sample") && n.ends_with(".sql"));
    let dump = std::fs::read_to_string(&backup).unwrap();
    assert!(dump.contains("CREATE TABLE customer") && dump.contains("INSERT INTO"), "{}", &dump[..dump.len().min(400)]);
    settle(&mut h, "backup");

    // Maintenance (the default operation) on customer.
    nav_menu(&mut h, "customer", &["Maintenance"]);
    click(&mut h, "Run");
    settle(&mut h, "maintenance");

    // Mock data: 'Generate' asks, then inserts.
    let before: i64 = query_one(&db, "SELECT COUNT(*) FROM category").parse().unwrap();
    nav_menu(&mut h, "category", &["Generate mock data"]);
    click(&mut h, "Generate");
    if has(&h, "Delete and generate") || h.query_all_by_label_contains("Generate").count() > 1 {
        pause(&mut h);
        h.query_all_by_label_contains("Generate").last().unwrap().click();
        frames(&mut h, 3);
    }
    wait(&mut h, "mock rows", |_| query_one(&db, "SELECT COUNT(*) FROM category").parse::<i64>().unwrap() > before);
    settle(&mut h, "mock data");

    // Metadata search.
    nav_menu(&mut h, "dbiji-sample.db", &["Tools ⏵", "Search metadata"]);
    field_with_hint(&mut h, "Name or pattern", "cust*");
    click(&mut h, "Search");
    wait(&mut h, "search results", |h| h.query_all_by_label_contains("customer").count() > 2);
    settle(&mut h, "search");

    // Data and schema compare of the database with itself: no differences.
    nav_menu(&mut h, "customer", &["Compare data"]);
    click(&mut h, "Compare");
    settle(&mut h, "data compare");
    nav_menu(&mut h, "Tables", &["Compare schema"]);
    click(&mut h, "Compare");
    settle(&mut h, "schema compare");

    // Table designer: a new table with one column.
    nav_menu(&mut h, "Tables", &["Create table"]);
    frames(&mut h, 3);
    field_with_hint(&mut h, "table name", "zz_designed");
    // The designer starts with an "id INTEGER" primary key column: rename it.
    pause(&mut h);
    {
        let name = h.query_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.value().as_deref() == Some("id")).expect("column name field");
        name.click();
    }
    frames(&mut h, 2);
    h.key_press_modifiers(CMD, Key::A);
    h.event(egui::Event::Text("code".into()));
    frames(&mut h, 2);
    click(&mut h, "Create table…");
    assert!(has(&h, "Execute"), "designer confirmation: {:?}", labels(&h));
    pause(&mut h);
    h.query_all_by_label_contains("Execute").last().unwrap().click();
    wait(&mut h, "table created", |_| query_one(&db, "SELECT COUNT(*) FROM sqlite_master WHERE name = 'zz_designed'") == "1");
    settle(&mut h, "table designer");
    let ddl = query_one(&db, "SELECT sql FROM sqlite_master WHERE name = 'zz_designed'");
    assert!(ddl.contains("code"), "{ddl}");
}

#[test]
fn themes_whats_new_and_updates() {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dbiji_core::i18n::set_locale(dbiji_core::i18n::Locale::En);
    let dir = Dir(std::env::temp_dir().join(format!("dbiji-ui-{}", uuid::Uuid::new_v4().simple())), guard);
    // An existing installation: a settings file from an older version.
    let paths = dbiji_config::AppPaths::with_root(dir.0.join("home"));
    paths.ensure_dirs().unwrap();
    std::fs::write(paths.settings_file(), "{}").unwrap();
    let mut settings = Settings::default();
    settings.general.theme = dbiji_config::settings::Theme::Light;
    let mut h = harness_with(&dir.0, settings);
    frames(&mut h, 3);
    let version = dbiji_app::updates::current_version();

    // "What's new" for this version, then the one-time question.
    assert!(has(&h, &format!("What's new in DBiji {version}")), "{:?}", labels(&h));
    assert_eq!(h.state().settings.updates.last_seen_version.as_deref(), Some(version));
    assert!(!has(&h, "Check for updates automatically?"), "one popup at a time");
    // egui centres a modal with the size of the previous frame: let the
    // long notes settle before clicking.
    frames(&mut h, 8);
    click(&mut h, "Continue");
    assert!(!has(&h, "What's new in DBiji"));
    assert!(has(&h, "Check for updates automatically?"), "{:?}", labels(&h));
    click(&mut h, "Don't check");
    assert_eq!(h.state().settings.updates.check_automatically, Some(false));
    assert!(!has(&h, "Check for updates automatically?"));
    // Help › What's new shows the notes again.
    pause(&mut h);
    h.query_all_by_label("Help").next().unwrap().click();
    frames(&mut h, 2);
    click(&mut h, "What's new");
    assert!(has(&h, &format!("What's new in DBiji {version}")));
    h.key_press(Key::Escape);
    frames(&mut h, 2);
    assert!(!has(&h, "What's new in DBiji"));

    // Themes: View › Theme, the status bar button, the command palette.
    assert_eq!(h.ctx.theme(), egui::Theme::Light);
    pause(&mut h);
    h.query_all_by_label("View").next().unwrap().click();
    frames(&mut h, 2);
    click(&mut h, "Theme");
    click(&mut h, "Dark");
    assert_eq!(h.ctx.theme(), egui::Theme::Dark);
    assert_eq!(h.state().settings.general.theme, dbiji_config::settings::Theme::Dark);
    pause(&mut h);
    h.key_press(Key::Escape);
    frames(&mut h, 2);
    click(&mut h, egui_phosphor::regular::SUN);
    assert_eq!(h.ctx.theme(), egui::Theme::Light);
    click(&mut h, egui_phosphor::regular::MOON);
    assert_eq!(h.ctx.theme(), egui::Theme::Dark);
    pause(&mut h);
    h.key_press_modifiers(CMD | Modifiers::SHIFT, Key::P);
    frames(&mut h, 3);
    h.event(egui::Event::Text("Toggle Light".into()));
    frames(&mut h, 2);
    h.key_press(Key::Enter);
    frames(&mut h, 3);
    assert_eq!(h.ctx.theme(), egui::Theme::Light);
    // The choice is saved.
    let saved = dbiji_config::settings::load(&paths);
    assert_eq!(saved.general.theme, dbiji_config::settings::Theme::Light);

    // A newer release: skip it, be reminded later, download.
    let release = dbiji_app::updates::Release {
        version: "99.0.0".into(),
        url: "https://github.com/ilhamnadhif/dbiji/releases/tag/v99.0.0".into(),
        download_url: Some("https://github.com/ilhamnadhif/dbiji/releases/download/v99.0.0/DBiji-99.0.0.dmg".into()),
        notes: "### New\n- Everything".into(),
        ..Default::default()
    };
    h.state_mut().updates.available = Some(release.clone());
    frames(&mut h, 2);
    assert!(has(&h, "A new version of DBiji is available"), "{:?}", labels(&h));
    click(&mut h, "Remind me later");
    assert!(!has(&h, "A new version of DBiji is available"));
    assert_eq!(h.state().settings.updates.skipped_version, None);
    h.state_mut().updates.available = Some(release.clone());
    frames(&mut h, 2);
    click(&mut h, "Skip this version");
    assert_eq!(h.state().settings.updates.skipped_version.as_deref(), Some("99.0.0"));
    h.state_mut().updates.available = Some(release.clone());
    frames(&mut h, 2);
    pause(&mut h);
    h.query_all_by_label_contains("Download").last().unwrap().click();
    let mut opened = None;
    for _ in 0..6 {
        h.step();
        for c in &h.output().platform_output.commands {
            if let egui::OutputCommand::OpenUrl(u) = c {
                opened = Some(u.url.clone());
            }
        }
    }
    assert!(!has(&h, "A new version of DBiji is available"));
    assert_eq!(opened.as_deref(), release.download_url.as_deref());
    // A release found by a later check lights up the toolbar's Update button.
    h.state_mut().updates.found = Some(release.clone());
    frames(&mut h, 2);
    click(&mut h, "Update 99.0.0");
    assert!(has(&h, "Skip this version"), "{:?}", labels(&h));
    // Not installed in /Applications (a test binary): offered as a download.
    assert!(!has(&h, "Install update"));
    click(&mut h, "Remind me later");
    h.state_mut().updates.found = None;
    // Help › Check for updates… (offline in tests) reports the result.
    pause(&mut h);
    h.query_all_by_label("Help").next().unwrap().click();
    frames(&mut h, 2);
    click(&mut h, "Check for updates");
    wait(&mut h, "update check result", |h| !h.state().updates.checking());
    frames(&mut h, 2);
    no_errors(&mut h, "updates");
}

#[test]
fn catalog_folders_in_the_navigator() {
    let (_dir, mut h, _conn) = setup();
    // SQLite: Administer › Settings opens as a table of pragmas.
    wait(&mut h, "Administer group", |h| nav_row(h, "Administer").is_some());
    nav_double_click(&mut h, "Administer");
    wait(&mut h, "Settings folder", |h| nav_row(h, "Settings").is_some());
    nav_double_click(&mut h, "Settings");
    wait(&mut h, "settings table", |h| {
        let e = h
            .state_mut()
            .dock
            .find_active_focused()
            .and_then(|(_, t)| t.view.as_any_mut().downcast_mut::<SqlEditorTab>().map(|e| (e.is_running(), e.result_count(), e.result_rows(0))));
        matches!(e, Some((false, 1, Some(n))) if n > 5)
    });
    no_errors(&mut h, "sqlite settings");
    // Context menu of a folder: View as table, Refresh.
    let n = sweep_menu(&mut h, "Settings", &|h: &mut H| nav_row(h, "Settings").unwrap().click_secondary(), &[]);
    assert_eq!(n, 2);

    // PostgreSQL: Security › Roles › <role> opens the role with its DDL.
    let Ok(url) = std::env::var("DBIJI_TEST_PG") else { return };
    let (mut cfg, password) = dbiji_config::parse_connection_url(&url).unwrap();
    cfg.name = "pg-catalog".into();
    let user = cfg.user.clone().unwrap();
    let id = cfg.id.clone();
    {
        let app = h.state_mut();
        app.conns.upsert(cfg);
        let backend = app.backend.clone();
        let creds = dbiji_app::conn::Credentials { password: password.clone(), ..Default::default() };
        app.conns.connect(&backend, &id, creds).unwrap();
    }
    wait(&mut h, "pg connected", |h| h.state().conns.is_connected(&id));
    h.state_mut().nav.expand(dbiji_app::nav::NavNode::Connection(id.clone()));
    wait(&mut h, "Security group", |h| nav_row(h, "Security").is_some());
    nav_double_click(&mut h, "Security");
    wait(&mut h, "Roles folder", |h| nav_row(h, "Roles").is_some());
    nav_double_click(&mut h, "Roles");
    // The role (the database has the same name: the role row is the last one).
    wait(&mut h, "role row", |h| h.query_all_by_label(&user).filter(|n| n.rect().left() < 120.0).count() >= 2);
    pause(&mut h);
    {
        let n = h.query_all_by_label(&user).rfind(|n| n.rect().left() < 120.0).unwrap();
        n.click();
        n.click();
    }
    frames(&mut h, 3);
    type RoleTab = (Option<String>, Vec<(String, String)>);
    let role_tab = |h: &mut H| -> Option<RoleTab> {
        h.state_mut()
            .dock
            .iter_all_tabs_mut()
            .find_map(|(_, t)| t.view.as_any_mut().downcast_mut::<dbiji_app::tabs::catalog::CatalogObjectTab>().map(|c| (c.ddl_text().map(str::to_string), c.properties())))
    };
    wait(&mut h, "role DDL and properties", |h| role_tab(h).is_some_and(|(ddl, props)| ddl.is_some() && !props.is_empty()));
    let (ddl, props) = role_tab(&mut h).unwrap();
    let ddl = ddl.unwrap();
    assert!(ddl.contains(&format!("CREATE ROLE {user}")) || ddl.contains(&format!("CREATE ROLE \"{user}\"")), "{ddl}");
    assert!(props.iter().any(|(k, _)| k == "can_login"), "{props:?}");
    let titles = tab_titles(&h);
    assert!(titles.iter().any(|t| t == &user), "{titles:?}");
    no_errors(&mut h, "pg role");
}

#[test]
fn editor_commands_from_dbeaver() {
    let (_dir, mut h, conn) = setup();
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    let ctx = h.ctx.clone();
    let script = "select * from customer;\n\nselect id, name   \nfrom product;  \n";
    focused_editor(&mut h).set_text(&ctx, script);
    focused_editor(&mut h).set_cursor(&ctx, 0);
    frames(&mut h, 2);
    click_editor(&mut h);
    focused_editor(&mut h).set_cursor(&ctx, 0);
    frames(&mut h, 1);
    // Next / previous statement (Cmd+Alt+Down / Up).
    h.key_press_modifiers(CMD | Modifiers::ALT, Key::ArrowDown);
    frames(&mut h, 2);
    let second = script.find("select id").unwrap();
    assert_eq!(focused_editor(&mut h).editor_cursor(), second);
    h.key_press_modifiers(CMD | Modifiers::ALT, Key::ArrowUp);
    frames(&mut h, 2);
    assert_eq!(focused_editor(&mut h).editor_cursor(), 0);
    let run = |h: &mut H, a: Action| {
        let ctx = h.ctx.clone();
        h.state_mut().dispatch_action(&ctx, a);
        frames(h, 2);
    };
    // Copy the current statement.
    run(&mut h, Action::CopyStatement);
    // Count rows of the statement under the cursor.
    run(&mut h, Action::SelectRowCount);
    wait(&mut h, "row count", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == 1 && e.result_rows(0) == Some(1)
    });
    // Evaluate the selected expression.
    let expr = "select 6 * 7 as answer";
    focused_editor(&mut h).set_text(&ctx, expr);
    focused_editor(&mut h).select_range(&ctx, 7..12);
    run(&mut h, Action::EvaluateExpression);
    wait(&mut h, "evaluated", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == 2 && e.result_rows(1) == Some(1)
    });
    // Morph to a delimited list, trim trailing spaces.
    focused_editor(&mut h).set_text(&ctx, "Ani\nBudi\nO'Neil");
    focused_editor(&mut h).select_range(&ctx, 0..15);
    run(&mut h, Action::MorphToDelimited);
    assert_eq!(focused_editor(&mut h).text(), "'Ani', 'Budi', 'O''Neil'");
    focused_editor(&mut h).set_text(&ctx, "select 1   \nfrom t\t\n");
    focused_editor(&mut h).set_cursor(&ctx, 0);
    run(&mut h, Action::TrimTrailingSpaces);
    assert_eq!(focused_editor(&mut h).text(), "select 1\nfrom t\n");
    // Open declaration (F3) on a table name.
    focused_editor(&mut h).set_text(&ctx, "select * from product");
    wait(&mut h, "completion catalog", |h| focused_editor(h).has_catalog());
    focused_editor(&mut h).set_cursor(&ctx, 16);
    run(&mut h, Action::OpenDeclaration);
    frames(&mut h, 3);
    assert!(tab_titles(&h).iter().any(|t| t == "product"), "{:?}", tab_titles(&h));
    // Execution plans: explain, save as JSON, load it back.
    focus_tab(&mut h, "Script-1");
    focused_editor(&mut h).set_text(&ctx, "select * from customer where id > 2");
    focused_editor(&mut h).set_cursor(&ctx, 0);
    let before = focused_editor(&mut h).result_count();
    run(&mut h, Action::ExplainPlan);
    wait(&mut h, "plan", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == before + 1 && e.result_is_plan(before)
    });
    let json = focused_editor(&mut h).plan_json(before).expect("plan json");
    assert!(json.contains("\"dbiji_plan\"") && json.contains("customer"), "{json}");
    focused_editor(&mut h).load_plan_json(&json).unwrap();
    assert!(focused_editor(&mut h).result_is_plan(before + 1));
    assert!(focused_editor(&mut h).load_plan_json("{\"x\": 1}").is_err());
    // Formatter choices: compact, the statement under the cursor only, an external command.
    {
        use dbiji_config::settings::FormatterKind;
        h.state_mut().settings.editor.format.formatter = FormatterKind::Compact;
        h.state_mut().settings.editor.format.statement_only = true;
        focused_editor(&mut h).set_text(&ctx, "select id, name from customer where id > 1 and name like 'A%';\nselect 1;");
        focused_editor(&mut h).set_cursor(&ctx, 0);
        run(&mut h, Action::FormatSql);
        let t = focused_editor(&mut h).text().to_string();
        assert!(t.starts_with("SELECT id, name"), "{t}");
        assert!(t.ends_with("select 1;"), "only the first statement: {t}");
        if cfg!(unix) {
            h.state_mut().settings.editor.format.formatter = FormatterKind::External;
            h.state_mut().settings.editor.format.external_command = "tr a-z A-Z".into();
            h.state_mut().settings.editor.format.statement_only = false;
            focused_editor(&mut h).set_text(&ctx, "select 2 from t;");
            run(&mut h, Action::FormatSql);
            wait(&mut h, "external formatter", |h| focused_editor(h).text() == "SELECT 2 FROM T;");
        }
        h.state_mut().settings.editor.format = Default::default();
    }
    // Folding: collapse all statements, expand them again.
    focus_tab(&mut h, "Script-1");
    focused_editor(&mut h).set_text(&ctx, "select *\nfrom customer;\n\nselect *\nfrom product\nwhere id > 1;\n");
    frames(&mut h, 3);
    assert_eq!(focused_editor(&mut h).fold_regions(), vec![(0, 1), (3, 5)]);
    run(&mut h, Action::CollapseAllFolds);
    frames(&mut h, 2);
    assert_eq!(focused_editor(&mut h).folded_lines(), vec![0, 3]);
    run(&mut h, Action::ExpandAllFolds);
    frames(&mut h, 2);
    assert!(focused_editor(&mut h).folded_lines().is_empty());
    // Results layout toggles between below and right.
    let before = h.state().settings.results.layout;
    run(&mut h, Action::ToggleResultsLayout);
    assert_ne!(h.state().settings.results.layout, before);
    run(&mut h, Action::ToggleResultsLayout);
    assert_eq!(h.state().settings.results.layout, before);
    no_errors(&mut h, "editor commands");
}

/// Navigator › routine › Execute…: a script that calls the routine with
/// parameters, run through the parameter dialog (PostgreSQL).
#[test]
fn execute_routine_script() {
    let Ok(url) = std::env::var("DBIJI_TEST_PG") else { return };
    let (_dir, mut h, _sqlite) = setup();
    let (cfg, password) = dbiji_config::parse_connection_url(&url).unwrap();
    let id = cfg.id.clone();
    {
        let app = h.state_mut();
        app.conns.upsert(cfg);
        let backend = app.backend.clone();
        app.conns.connect(&backend, &id, dbiji_app::conn::Credentials { password, ..Default::default() }).unwrap();
    }
    wait(&mut h, "pg connected", |h| h.state().conns.is_connected(&id));
    let fname = format!("dbiji_add_{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    {
        let app = h.state();
        let s = app.conns.live(&id).unwrap().main.clone();
        let sql = format!("CREATE FUNCTION public.{fname}(a integer, b integer) RETURNS integer LANGUAGE sql AS 'SELECT a + b'");
        app.backend.block_on(async move { s.execute_update(&sql, &[]).await }).unwrap();
    }
    let routine = dbiji_core::ObjectRef::new(dbiji_core::ObjectKind::Function, Some("public".into()), fname.clone()).with_signature("(integer, integer)");
    h.state_mut().push_command(AppCommand::ExecuteRoutine { connection: id.clone(), routine });
    wait(&mut h, "call script", |h| {
        h.state_mut().dock.find_active_focused().and_then(|(_, t)| t.view.as_any().downcast_ref::<SqlEditorTab>().map(|e| e.text().contains(&fname))).unwrap_or(false)
    });
    let text = focused_editor(&mut h).text().to_string();
    assert!(text.contains(&format!("SELECT * FROM public.{fname}(:a, :b);")), "{text}");
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    let ctx = h.ctx.clone();
    let at = text.find("SELECT").unwrap();
    focused_editor(&mut h).set_cursor(&ctx, at);
    click_editor(&mut h);
    focused_editor(&mut h).set_cursor(&ctx, at);
    frames(&mut h, 1);
    h.key_press_modifiers(CMD, Key::Enter);
    wait(&mut h, "parameter prompt", |h| focused_editor(h).pending_params().is_some());
    h.event(egui::Event::Text("2".into()));
    frames(&mut h, 2);
    h.key_press(Key::Tab);
    frames(&mut h, 2);
    h.event(egui::Event::Text("40".into()));
    frames(&mut h, 2);
    h.key_press(Key::Enter);
    wait(&mut h, "result", |h| {
        let e = focused_editor(h);
        !e.is_running() && e.result_count() == 1 && e.result_rows(0) == Some(1) && !e.result_is_error(0)
    });
    assert!(labels(&h).iter().any(|l| l.starts_with("Data grid, 1 rows")), "{:?}", labels(&h));
    no_errors(&mut h, "execute routine");
    {
        let app = h.state();
        let s = app.conns.live(&id).unwrap().main.clone();
        let sql = format!("DROP FUNCTION public.{fname}(integer, integer)");
        app.backend.block_on(async move { s.execute_update(&sql, &[]).await }).unwrap();
    }
}

#[test]
fn navigator_behaviour_from_dbeaver() {
    let (dir, mut h, conn) = setup();
    wait(&mut h, "Tables", |h| nav_row(h, "Tables").is_some());
    nav_double_click(&mut h, "Tables");
    wait(&mut h, "customer", |h| nav_row(h, "customer").is_some());
    // Hide an object, then clear the navigator filter of the folder.
    nav_menu_item(&mut h, "customer", "Hide in navigator");
    wait(&mut h, "customer hidden", |h| nav_row(h, "customer").is_none());
    assert!(h.state().conns.config(&conn).unwrap().filters.tables.exclude.contains(&"customer".to_string()));
    nav_menu_item(&mut h, "Tables", "Clear navigator filter");
    wait(&mut h, "customer back", |h| nav_row(h, "customer").is_some());
    // Column › Create index…: DDL in a new editor.
    {
        let id = conn.clone();
        let table = dbiji_core::ObjectRef::table(Some("main".into()), "customer");
        h.state_mut().nav.expand(dbiji_app::nav::NavNode::Object { conn: id.clone(), obj: table.clone() });
        h.state_mut().nav.expand(dbiji_app::nav::NavNode::ChildFolder { conn: id, parent: table, kind: dbiji_core::ObjectKind::Column });
    }
    wait(&mut h, "columns", |h| nav_row(h, "email").is_some());
    nav_menu_item(&mut h, "email", "Create index…");
    wait(&mut h, "index DDL", |h| {
        h.state_mut().dock.find_active_focused().and_then(|(_, t)| t.view.as_any().downcast_ref::<SqlEditorTab>().map(|e| e.text().starts_with("CREATE INDEX"))).unwrap_or(false)
    });
    let ddl = focused_editor(&mut h).text().to_string();
    assert!(ddl.contains("customer") && ddl.contains("email"), "{ddl}");
    // Link with editor: the active tab's table is selected in the navigator.
    h.state_mut().settings.navigator.link_with_editor = true;
    h.state_mut().push_command(AppCommand::OpenObject {
        connection: conn.clone(),
        object: dbiji_core::ObjectRef::table(Some("main".into()), "product"),
        page: dbiji_app::command::ObjectPage::Data,
    });
    wait(&mut h, "product selected", |h| matches!(&h.state().nav.selected, Some(dbiji_app::nav::NavNode::Object { obj, .. }) if obj.name == "product"));
    h.state_mut().settings.navigator.link_with_editor = false;
    // A second connection: order and "Disconnect others".
    let other = dir.0.join("other.db");
    std::fs::write(&other, b"").unwrap();
    h.state_mut().open_path(&other, false);
    wait(&mut h, "second connected", |h| h.state().conns.configs().len() == 2 && h.state().conns.configs().iter().all(|c| h.state().conns.is_connected(&c.id)));
    let names = |h: &H| -> Vec<String> {
        let mut v: Vec<(Option<u32>, String)> = h.state().conns.configs().iter().map(|c| (c.sort_index, c.name.clone())).collect();
        v.sort_by_key(|(i, n)| (i.unwrap_or(u32::MAX), n.to_lowercase()));
        v.into_iter().map(|(_, n)| n).collect()
    };
    assert_eq!(names(&h), vec!["dbiji-sample.db", "other.db"]);
    let other_id = h.state().conns.configs().iter().find(|c| c.name == "other.db").unwrap().id.clone();
    h.state_mut().push_command(AppCommand::MoveConnection { id: other_id.clone(), to: dbiji_app::command::MovePos::Top });
    frames(&mut h, 2);
    assert_eq!(names(&h), vec!["other.db", "dbiji-sample.db"]);
    nav_menu_item(&mut h, "other.db", "Disconnect others");
    wait(&mut h, "sample disconnected", |h| !h.state().conns.is_connected(&conn));
    assert!(h.state().conns.is_connected(&other_id));
    // Projects: create, switch, back.
    {
        let pm = h.state().services.projects.clone().unwrap();
        pm.create("Reports").unwrap();
    }
    h.state_mut().push_command(AppCommand::SetActiveProject("Reports".into()));
    frames(&mut h, 2);
    assert_eq!(h.state().services.projects.as_ref().unwrap().active_name(), "Reports");
    h.state_mut().push_command(AppCommand::SetActiveProject("General".into()));
    frames(&mut h, 2);
    // Open Database Object (Cmd+Shift+D) opens the palette.
    pause(&mut h);
    h.key_press_modifiers(CMD | Modifiers::SHIFT, Key::D);
    frames(&mut h, 2);
    assert!(h.state().dialogs.palette_open);
    h.key_press(Key::Escape);
    frames(&mut h, 2);
    no_errors(&mut h, "navigator behaviour");
}

/// Right-click a navigator row and click a menu item.
fn nav_menu_item(h: &mut H, row: &str, item: &str) {
    pause(h);
    nav_row(h, row).unwrap_or_else(|| panic!("no navigator row {row:?}")).click_secondary();
    frames(h, 2);
    click(h, item);
    frames(h, 3);
}

#[test]
fn workspace_from_dbeaver() {
    let (dir, mut h, conn) = setup();
    let run = |h: &mut H, a: Action| {
        let ctx = h.ctx.clone();
        h.state_mut().dispatch_action(&ctx, a);
        frames(h, 3);
    };
    let name = h.state().conns.config(&conn).unwrap().name.clone();

    // Tip of the day: next tip, turn off the tips on start, close.
    run(&mut h, Action::TipOfTheDay);
    let first = h.state().tip.as_ref().expect("tip window").index;
    assert!(has(&h, dbiji_app::tips::TIPS[first]), "{:?}", labels(&h));
    click(&mut h, "Next tip");
    let second = h.state().tip.as_ref().unwrap().index;
    assert_eq!(second, (first + 1) % dbiji_app::tips::TIPS.len());
    assert!(has(&h, dbiji_app::tips::TIPS[second]));
    click(&mut h, "Show tips on start");
    assert!(!h.state().settings.general.show_tips);
    h.state_mut().settings.general.show_tips = true;
    settle(&mut h, "tip of the day");
    assert!(h.state().tip.is_none());

    // Main toolbar: SQL editor actions are disabled without an editor.
    let toolbar_enabled = |h: &H, icon: &str| -> bool {
        let n = h.query_all_by_label(icon).find(|n| n.rect().top() < 70.0).unwrap_or_else(|| panic!("toolbar {icon:?}: {:?}", labels(h)));
        !n.accesskit_node().is_disabled()
    };
    focus_tab(&mut h, "Welcome");
    frames(&mut h, 2);
    // The default toolbar has global actions only (run, commit … are in the editor).
    assert!(h.query_all_by_label(egui_phosphor::regular::PLAY).all(|n| n.rect().top() >= 70.0), "no Run in the main toolbar by default");
    // A customised toolbar with editor actions: disabled without an editor.
    h.state_mut().settings.toolbar.items = ["sql.new_editor", "sql.execute_statement", "tx.commit"].map(String::from).to_vec();
    frames(&mut h, 2);
    assert!(!toolbar_enabled(&h, egui_phosphor::regular::PLAY), "Run needs a SQL editor");
    assert!(toolbar_enabled(&h, egui_phosphor::regular::FILE_SQL), "New SQL editor is always available");
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    frames(&mut h, 2);
    assert!(toolbar_enabled(&h, egui_phosphor::regular::PLAY), "Run in a SQL editor");
    assert!(!toolbar_enabled(&h, egui_phosphor::regular::CHECK_CIRCLE), "nothing to commit in auto-commit mode");
    h.state_mut().settings.toolbar = Default::default();
    settle(&mut h, "toolbar states");

    // Production: reading runs without a question, changing data asks.
    {
        let mut cfg = h.state().conns.config(&conn).unwrap().clone();
        cfg.connection_type = "prod".into();
        h.state_mut().conns.upsert(cfg);
    }
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    let ctx = h.ctx.clone();
    focused_editor(&mut h).set_text(&ctx, "select count(*) from customer");
    let run_stmt = |h: &mut H| {
        let ctx = h.ctx.clone();
        h.state_mut().dispatch_action(&ctx, Action::ExecuteStatement);
        frames(h, 3);
    };
    run_stmt(&mut h);
    assert!(focused_editor(&mut h).pending_confirmation().is_none(), "no question for a SELECT on production");
    wait(&mut h, "select result", |h| !focused_editor(h).is_running() && focused_editor(h).result_count() == 1);
    focused_editor(&mut h).set_text(&ctx, "update customer set name = name where id = 1");
    run_stmt(&mut h);
    let reasons = focused_editor(&mut h).pending_confirmation().expect("a question before changing data");
    assert!(reasons.iter().any(|r| r.contains("change data or schema")), "{reasons:?}");
    {
        let mut cfg = h.state().conns.config(&conn).unwrap().clone();
        cfg.connection_type = "dev".into();
        h.state_mut().conns.upsert(cfg);
    }
    settle(&mut h, "production confirmation");

    // Error log: errors of the session, filtered by level.
    dbiji_app::applog::push(tracing::Level::ERROR, "dbiji::test", "probe failure 42");
    dbiji_app::applog::push(tracing::Level::INFO, "dbiji::test", "probe info 43");
    run(&mut h, Action::ShowErrorLog);
    wait(&mut h, "error log entry", |h| has(h, "probe failure 42"));
    assert!(!has(&h, "probe info 43"), "info entries are hidden by default");
    {
        let (_, tab) = h.state_mut().dock.find_active_focused().unwrap();
        tab.view.as_any_mut().downcast_mut::<dbiji_app::tabs::errorlog::ErrorLogTab>().expect("error log tab").set_level(tracing::Level::INFO);
    }
    frames(&mut h, 2);
    assert!(has(&h, "probe info 43"));

    // Properties panel of the selected navigator object.
    run(&mut h, Action::ToggleProperties);
    assert!(h.state().settings.navigator.show_properties);
    pause(&mut h);
    nav_row(&h, &name).expect("connection row").click();
    frames(&mut h, 3);
    assert!(has(&h, "Database type") && has(&h, "SQLite"), "{:?}", labels(&h));
    wait(&mut h, "Tables", |h| nav_row(h, "Tables").is_some());
    nav_double_click(&mut h, "Tables");
    wait(&mut h, "customer", |h| nav_row(h, "customer").is_some());
    pause(&mut h);
    nav_row(&h, "customer").unwrap().click();
    frames(&mut h, 3);
    assert!(has(&h, "Schema") && h.query_all_by_label("customer").count() >= 2, "{:?}", labels(&h));
    settle(&mut h, "properties panel");

    // Pending transactions: an editor in manual commit mode with an insert.
    h.state_mut().push_command(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: None, text: None });
    frames(&mut h, 3);
    wait(&mut h, "editor session", |h| focused_editor(h).session_ready());
    run(&mut h, Action::ToggleAutoCommit);
    let ctx = h.ctx.clone();
    focused_editor(&mut h).set_text(&ctx, "create table tx_probe (id integer);\ninsert into tx_probe values (1);");
    frames(&mut h, 2);
    run(&mut h, Action::ExecuteScript);
    wait(&mut h, "uncommitted insert", |h| {
        let app = h.state();
        !dbiji_app::pending_tx::collect(&app.dock, &app.conns).is_empty()
    });
    run(&mut h, Action::PendingTransactions);
    assert!(has(&h, "Pending transactions") && has(&h, "Commit all"), "{:?}", labels(&h));
    assert!(h.query_all_by_label(&name).count() >= 2, "the connection is listed: {:?}", labels(&h));
    click(&mut h, "Commit all");
    wait(&mut h, "committed", |h| {
        let app = h.state();
        dbiji_app::pending_tx::collect(&app.dock, &app.conns).is_empty()
    });
    frames(&mut h, 2);
    assert!(has(&h, "No connection has uncommitted changes."));
    assert_eq!(query_one(&sample_db(&h, &conn), "select count(*) from tx_probe"), "1");
    settle(&mut h, "pending transactions");

    // Collect diagnostic info: a zip in ~/Downloads without identifying data.
    run(&mut h, Action::CollectDiagnostics);
    let zip_path = wait_file(&mut h, &dir.0.join("Downloads"), "diagnostic zip", |n| n.starts_with("dbiji-diagnostics-") && n.ends_with(".zip"));
    wait(&mut h, "diagnostics done", |h| !h.state().collecting_diagnostics());
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let mut all = String::new();
    for i in 0..zip.len() {
        std::io::Read::read_to_string(&mut zip.by_index(i).unwrap(), &mut all).unwrap();
    }
    assert!(all.contains("SQLite") && all.contains("probe failure 42"), "{all}");
    let db_file = sample_db(&h, &conn).display().to_string();
    assert!(!all.contains(&name) && !all.contains(&db_file), "no connection name or file path");
    settle(&mut h, "diagnostics");

    // Reset settings (the update choices are kept).
    h.state_mut().settings.navigator.link_with_editor = true;
    run(&mut h, Action::OpenSettings);
    click(&mut h, "Reset all settings");
    pause(&mut h);
    h.query_all_by_label("Reset").next().expect("confirm button").click();
    frames(&mut h, 3);
    let s = &h.state().settings;
    assert!(!s.navigator.link_with_editor && !s.navigator.show_properties);
    assert_eq!(s.updates.check_automatically, Some(false));
    settle(&mut h, "reset settings");

    // A tip on start of an existing installation.
    let mut settings = Settings::default();
    settings.updates.check_automatically = Some(false);
    settings.updates.last_seen_version = Some(dbiji_app::updates::current_version().to_string());
    let mut h2 = harness_with(&dir.0, settings);
    h2.state_mut().existing_install = true;
    frames(&mut h2, 3);
    assert!(h2.state().tip.is_some(), "tip on start");
}

/// New database… from the navigator on PostgreSQL (DBIJI_TEST_PG): the
/// dialog, CREATE DATABASE and the new database in the tree.
#[test]
fn create_database_from_the_navigator() {
    let Some(url) = std::env::var("DBIJI_TEST_PG").ok().filter(|u| !u.trim().is_empty()) else { return };
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    dbiji_core::i18n::set_locale(dbiji_core::i18n::Locale::En);
    let dir = Dir(std::env::temp_dir().join(format!("dbiji-ui-{}", uuid::Uuid::new_v4().simple())), guard);
    std::fs::create_dir_all(&dir.0).unwrap();
    // SAFETY: the walkthroughs run one at a time (SERIAL).
    unsafe {
        std::env::set_var("HOME", &dir.0);
        std::env::set_var("DBIJI_UPDATE_URL", "off");
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (mut cfg, password) = dbiji_config::parse_connection_url(&url).unwrap();
    cfg.name = "pg-test".into();
    cfg.save_password = true;
    let current = cfg.database.clone().unwrap_or_else(|| "postgres".into());
    let secrets = Arc::new(MemorySecrets::default());
    if let Some(pw) = &password {
        dbiji_app::conn::SecretSource::set(secrets.as_ref(), &dbiji_core::config::secret_key(&cfg.id, dbiji_core::config::SecretSlot::Password), pw).unwrap();
    }
    let mut conns = ConnectionManager::new(dbiji_app::driver_registry(), secrets, Arc::new(DirectNet));
    let id = cfg.id.clone();
    conns.upsert(cfg.clone());
    let mut settings = Settings::default();
    settings.updates.check_automatically = Some(false);
    let mut h = harness_conns(&dir.0, settings, conns);
    frames(&mut h, 3);
    h.state_mut().push_command(AppCommand::Connect(id.clone()));
    wait(&mut h, "connected", |h| h.state().conns.is_connected(&id));
    wait(&mut h, "current database", |h| nav_row(h, &current).is_some());
    // A schema belongs to a database: not on the connection, on the database.
    pause(&mut h);
    nav_row(&h, "pg-test").unwrap().click_secondary();
    frames(&mut h, 2);
    assert!(has(&h, "New database…") && !has(&h, "New schema…"), "connection menu: {:?}", labels(&h));
    h.key_press(Key::Escape);
    frames(&mut h, 2);
    pause(&mut h);
    nav_row(&h, &current).unwrap().click_secondary();
    frames(&mut h, 2);
    assert!(has(&h, "New schema…"), "database menu: {:?}", labels(&h));
    h.key_press(Key::Escape);
    frames(&mut h, 2);
    // Right-click the database: New database…
    let name = format!("dbiji_ui_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    pause(&mut h);
    nav_row(&h, &current).unwrap().click_secondary();
    frames(&mut h, 2);
    click(&mut h, "New database…");
    wait(&mut h, "dialog", |h| has(h, "Show all databases in the navigator"));
    frames(&mut h, 2);
    h.event(egui::Event::Text(name.clone()));
    frames(&mut h, 2);
    let preview = |h: &H| {
        h.query_all_by_label_contains("")
            .chain(h.query_all_by_role(egui::accesskit::Role::MultilineTextInput))
            .filter_map(|n| n.accesskit_node().value().or(n.accesskit_node().label()))
            .any(|t| t.contains(&format!("CREATE DATABASE {name}")))
    };
    assert!(preview(&h), "statement preview: {:?}", labels(&h));
    // The dialog keeps its size: the button is where it was a frame ago.
    let label = format!("{} Create", egui_phosphor::regular::PLUS);
    frames(&mut h, 2);
    let before = h.query_all_by_label(&label).next().expect("Create button").rect();
    frames(&mut h, 5);
    assert_eq!(h.query_all_by_label(&label).next().unwrap().rect(), before, "the dialog does not grow");
    pause(&mut h);
    h.query_all_by_label(&format!("{} Create", egui_phosphor::regular::PLUS)).next().expect("Create button").click();
    frames(&mut h, 2);
    // Created, and listed because "Show all databases" was turned on.
    wait(&mut h, "new database in the navigator", |h| nav_row(h, &name).is_some());
    assert!(h.state().conns.config(&id).unwrap().init.show_all_databases);
    no_errors(&mut h, "create database");
    // Clean up on a separate session.
    let backend = Backend::new().unwrap();
    let driver = dbiji_app::driver_registry().get(dbiji_core::DbKind::Postgres).unwrap();
    let params = dbiji_core::ConnectParams::from_config(&cfg, password);
    let mut s = backend.block_on(driver.connect(&params)).unwrap();
    backend.block_on(dbiji_core::driver::execute_update(s.as_mut(), &format!("DROP DATABASE {name} WITH (FORCE)"), &[])).unwrap();
}

/// Booleans in the record view are check boxes: a click toggles, nothing is typed.
#[test]
fn boolean_check_boxes_in_the_record_view() {
    let (_dir, mut h, conn) = setup();
    exec_sql(&mut h, "CREATE TABLE flags (id INTEGER PRIMARY KEY, active BOOLEAN NOT NULL); INSERT INTO flags VALUES (1, 0), (2, 1);");
    h.state_mut().push_command(AppCommand::RefreshMetadata { connection: conn.clone(), object: None, schema: None });
    h.state_mut().push_command(AppCommand::OpenObject {
        connection: conn.clone(),
        object: dbiji_core::ObjectRef::table(Some("main".into()), "flags"),
        page: dbiji_app::command::ObjectPage::Data,
    });
    wait(&mut h, "flags data", |h| has(h, "Data grid, 2 rows"));
    let body = h.query_all_by_label_contains("Data grid,").next().unwrap().rect();
    let p = egui::pos2(body.left() + 20.0, body.top() + 8.0);
    pause(&mut h);
    h.event(egui::Event::PointerMoved(p));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
    }
    frames(&mut h, 2);
    h.key_press(Key::Tab);
    wait(&mut h, "record view", |h| has(h, "Record view, row 1 of"));
    let boxes = |h: &H| h.query_all_by_role(egui::accesskit::Role::CheckBox).filter_map(|n| n.accesskit_node().label()).collect::<Vec<_>>();
    assert!(boxes(&h).iter().any(|l| l == "false"), "a check box for active: {:?}", boxes(&h));
    pause(&mut h);
    h.query_all_by_role(egui::accesskit::Role::CheckBox).find(|n| n.accesskit_node().label().as_deref() == Some("false")).unwrap().click();
    frames(&mut h, 3);
    assert!(boxes(&h).iter().any(|l| l == "true"), "toggled: {:?}", boxes(&h));
    assert!(has(&h, "Save (1)"), "one pending change: {:?}", labels(&h));
    assert!(h.query_all_by_role(egui::accesskit::Role::TextInput).all(|n| n.accesskit_node().value().as_deref() != Some("false")), "no text field for a boolean");
}
