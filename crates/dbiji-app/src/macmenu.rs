//! The native macOS application menu (muda): About, Check for Updates…,
//! Settings…, Services, Hide, Quit. The in-window menu bar stays the main
//! menu; there is no native Edit menu, as it would take ⌘C / ⌘V from egui.
//! ⌘, and ⌘Q are handled by this menu (it receives them before egui) and run
//! the same commands as DBiji's own shortcuts.

use std::collections::HashMap;
use std::sync::mpsc;

use dbiji_core::tr;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::command::AppCommand;

/// What a menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    About,
    CheckForUpdates,
    Settings,
    Quit,
}

pub struct AppMenu {
    _menu: Menu,
    items: HashMap<MenuId, Item>,
    events: mpsc::Receiver<MenuEvent>,
}

impl AppMenu {
    /// Builds the menu and installs it as the application's main menu. Call
    /// on the main thread once the app finished launching (eframe's creation
    /// closure).
    pub fn install(ctx: &egui::Context) -> Option<Self> {
        let mut items = HashMap::new();
        let mut item = |text: &str, accel: Option<Accelerator>, what: Item| {
            let it = MenuItem::new(text, true, accel);
            items.insert(it.id().clone(), what);
            it
        };
        let about = item(tr("About DBiji"), None, Item::About);
        let updates = item(tr("Check for Updates…"), None, Item::CheckForUpdates);
        let settings = item(tr("Settings…"), Some(Accelerator::new(Modifiers::META, Code::Comma)), Item::Settings);
        let quit = item(tr("Quit DBiji"), Some(Accelerator::new(Modifiers::META, Code::KeyQ)), Item::Quit);
        let app_menu = Submenu::with_items(
            "DBiji",
            true,
            &[
                &about,
                &updates,
                &PredefinedMenuItem::separator(),
                &settings,
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(Some(tr("Services"))),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(Some(tr("Hide DBiji"))),
                &PredefinedMenuItem::hide_others(Some(tr("Hide Others"))),
                &PredefinedMenuItem::show_all(Some(tr("Show All"))),
                &PredefinedMenuItem::separator(),
                &quit,
            ],
        )
        .ok()?;
        let menu = Menu::with_items(&[&app_menu]).ok()?;
        menu.init_for_nsapp();
        // Clicks arrive on the menu's thread: forward them and wake egui.
        let (tx, events) = mpsc::channel();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.send(event);
            ctx.request_repaint();
        }));
        Some(Self { _menu: menu, items, events })
    }

    /// The commands of the items clicked since the last call.
    pub fn commands(&self) -> Vec<AppCommand> {
        self.events
            .try_iter()
            .filter_map(|e| self.items.get(e.id()).copied())
            .map(|item| match item {
                Item::About => AppCommand::ShowAbout,
                Item::CheckForUpdates => AppCommand::CheckForUpdates,
                Item::Settings => AppCommand::OpenSettings,
                Item::Quit => AppCommand::Quit,
            })
            .collect()
    }
}
