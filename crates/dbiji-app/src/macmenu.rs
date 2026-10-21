//! The native macOS menu bar (muda), as in DBeaver: the application menu (About,
//! Check for Updates…, Settings…, Services, Hide, Quit), then File, Database,
//! Tools, View, Window and Help; the in-window menu bar is not shown on macOS.
//! There is no native Edit menu, as it would take ⌘C / ⌘V from egui. ⌘, and ⌘Q
//! are handled by this menu (it receives them before egui) and run the same
//! commands as DBiji's own shortcuts; the other items have no menu keys.

use std::collections::HashMap;
use std::sync::mpsc;

use dbiji_config::settings::Theme;
use dbiji_core::tr;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::menu::{MenuAction, MenuState};

/// Keep AppKit's Touch Bar machinery away from DBiji (it has no Touch Bar
/// items). On MacBooks with a Touch Bar it can raise a KVO exception
/// ("Cannot remove an observer <_NSTouchBarFinderObservation> for the key
/// path nextResponder") that AppKit turns into a crash. Registered (not
/// saved) defaults: call before the application starts.
pub fn disable_touch_bar() {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSNumber, NSString, NSUserDefaults};
    let key = NSString::from_str("NSFunctionBarAPIEnabled");
    let no = NSNumber::new_bool(false);
    let value: &AnyObject = &no;
    let defaults = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[value]);
    // SAFETY: a dictionary of property-list values, as registerDefaults: expects.
    unsafe { NSUserDefaults::standardUserDefaults().registerDefaults(&defaults) };
}

pub struct AppMenu {
    _menu: Menu,
    items: HashMap<MenuId, MenuAction>,
    events: mpsc::Receiver<MenuEvent>,
    new_database: MenuItem,
    new_schema: MenuItem,
    properties: CheckMenuItem,
    themes: Vec<(Theme, CheckMenuItem)>,
    diagnostics: MenuItem,
    updates: Vec<MenuItem>,
    /// The state last shown (`None`: set everything again).
    shown: Option<MenuState>,
}

impl AppMenu {
    /// Builds the menus and installs them as the application's main menu. Call
    /// on the main thread once the app finished launching (eframe's creation
    /// closure).
    pub fn install(ctx: &egui::Context) -> Option<Self> {
        use MenuAction as M;
        let mut items = HashMap::new();
        let mut item = |text: &str, accel: Option<Accelerator>, what: MenuAction| {
            let it = MenuItem::new(text, true, accel);
            items.insert(it.id().clone(), what);
            it
        };
        let sep = PredefinedMenuItem::separator;
        let about = item(tr("About DBiji"), None, M::About);
        let updates = item(tr("Check for Updates…"), None, M::CheckForUpdates);
        let settings = item(tr("Settings…"), Some(Accelerator::new(Modifiers::META, Code::Comma)), M::Settings);
        let quit = item(tr("Quit DBiji"), Some(Accelerator::new(Modifiers::META, Code::KeyQ)), M::Quit);
        let app_menu = Submenu::with_items(
            "DBiji",
            true,
            &[
                &about,
                &updates,
                &sep(),
                &settings,
                &sep(),
                &PredefinedMenuItem::services(Some(tr("Services"))),
                &sep(),
                &PredefinedMenuItem::hide(Some(tr("Hide DBiji"))),
                &PredefinedMenuItem::hide_others(Some(tr("Hide Others"))),
                &PredefinedMenuItem::show_all(Some(tr("Show All"))),
                &sep(),
                &quit,
            ],
        )
        .ok()?;
        // No accelerators below: DBiji's own (configurable) shortcuts run them,
        // and a menu key would be taken from egui (⌘C in a grid, ⌘Enter in an editor).
        let file = Submenu::with_items(tr("File"), true, &[&item(tr("New SQL editor"), None, M::NewSqlEditor), &item(tr("Open script…"), None, M::OpenScript)]).ok()?;
        let new_database = item(tr("New database…"), None, M::NewDatabase);
        let new_schema = item(tr("New schema…"), None, M::NewSchema);
        let database = Submenu::with_items(
            tr("Database"),
            true,
            &[
                &item(tr("New connection…"), None, M::NewConnection),
                &item(tr("Import connections…"), None, M::ImportConnections),
                &sep(),
                &new_database,
                &new_schema,
                &sep(),
                &item(tr("Disconnect all"), None, M::DisconnectAll),
                &item(tr("Pending transactions…"), None, M::PendingTransactions),
                &sep(),
                &item(tr("Compare schemas…"), None, M::CompareSchemas),
                &item(tr("Compare data…"), None, M::CompareData),
                &item(tr("Search…"), None, M::Search),
            ],
        )
        .ok()?;
        let tools = Submenu::with_items(
            tr("Tools"),
            true,
            &[
                &item(tr("Query history"), None, M::QueryHistory),
                &item(tr("Tasks"), None, M::Tasks),
                &item(tr("AI assistant"), None, M::AiAssistant),
                &item(tr("Background operations"), None, M::BackgroundOperations),
            ],
        )
        .ok()?;
        let mut check = |text: &str, what: MenuAction| {
            let it = CheckMenuItem::new(text, true, false, None);
            items.insert(it.id().clone(), what);
            it
        };
        let properties = check(tr("Properties panel"), M::PropertiesPanel);
        let themes: Vec<(Theme, CheckMenuItem)> =
            [(Theme::System, tr("System")), (Theme::Light, tr("Light")), (Theme::Dark, tr("Dark"))].into_iter().map(|(t, label)| (t, check(label, M::Theme(t)))).collect();
        let theme_items: Vec<&dyn muda::IsMenuItem> = themes.iter().map(|(_, it)| it as &dyn muda::IsMenuItem).collect();
        let theme = Submenu::with_items(tr("Theme"), true, &theme_items).ok()?;
        let mut item = |text: &str, what: MenuAction| {
            let it = MenuItem::new(text, true, None);
            items.insert(it.id().clone(), what);
            it
        };
        let view = Submenu::with_items(
            tr("View"),
            true,
            &[&item(tr("Toggle navigator"), M::ToggleNavigator), &properties, &theme, &sep(), &item(tr("Command palette"), M::CommandPalette)],
        )
        .ok()?;
        let window = Submenu::with_items(
            tr("Window"),
            true,
            &[
                &PredefinedMenuItem::minimize(Some(tr("Minimize"))),
                &PredefinedMenuItem::maximize(Some(tr("Zoom"))),
                &sep(),
                &PredefinedMenuItem::fullscreen(Some(tr("Enter Full Screen"))),
            ],
        )
        .ok()?;
        let diagnostics = item(tr("Collect diagnostic info"), M::CollectDiagnostics);
        let help_updates = item(tr("Check for updates…"), M::CheckForUpdates);
        let help = Submenu::with_items(
            tr("Help"),
            true,
            &[
                &item(tr("Keyboard shortcuts"), M::KeyboardShortcuts),
                &item(tr("Database drivers"), M::DatabaseDrivers),
                &item(tr("Create sample database"), M::CreateSampleDatabase),
                &item(tr("Tip of the day"), M::TipOfTheDay),
                &sep(),
                &item(tr("Error log"), M::ErrorLog),
                &diagnostics,
                &sep(),
                &item(tr("What's new"), M::WhatsNew),
                &help_updates,
            ],
        )
        .ok()?;
        let menu = Menu::with_items(&[&app_menu, &file, &database, &tools, &view, &window, &help]).ok()?;
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();
        // Clicks arrive on the menu's thread: forward them and wake egui.
        let (tx, events) = mpsc::channel();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.send(event);
            ctx.request_repaint();
        }));
        Some(Self { _menu: menu, items, events, new_database, new_schema, properties, themes, diagnostics, updates: vec![updates, help_updates], shown: None })
    }

    /// The items clicked since the last call.
    pub fn actions(&mut self) -> Vec<MenuAction> {
        let actions: Vec<MenuAction> = self.events.try_iter().filter_map(|e| self.items.get(e.id()).copied()).collect();
        // A check item toggles itself when clicked: set all marks again.
        if !actions.is_empty() {
            self.shown = None;
        }
        actions
    }

    /// Show `state` (enabled items, check marks); only what changed.
    pub fn sync(&mut self, state: &MenuState) {
        let shown = self.shown.as_ref();
        if shown.is_none_or(|s| s.new_database != state.new_database) {
            self.new_database.set_enabled(state.new_database);
        }
        if shown.is_none_or(|s| s.new_schema != state.new_schema) {
            self.new_schema.set_enabled(state.new_schema);
        }
        if shown.is_none_or(|s| s.properties != state.properties) {
            self.properties.set_checked(state.properties);
        }
        if shown.is_none_or(|s| s.theme != state.theme) {
            for (t, it) in &self.themes {
                it.set_checked(*t == state.theme);
            }
        }
        if shown.is_none_or(|s| s.diagnostics != state.diagnostics) {
            self.diagnostics.set_enabled(state.diagnostics);
        }
        if shown.is_none_or(|s| s.updates != state.updates) {
            for it in &self.updates {
                it.set_enabled(state.updates);
            }
        }
        self.shown = Some(state.clone());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn touch_bar_is_disabled() {
        super::disable_touch_bar();
        let key = objc2_foundation::NSString::from_str("NSFunctionBarAPIEnabled");
        let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
        assert!(defaults.objectForKey(&key).is_some(), "registered");
        assert!(!defaults.boolForKey(&key));
    }
}
