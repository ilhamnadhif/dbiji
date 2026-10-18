//! Long-lived application services shared by all UI components.

use crate::theme::ThemeChoice;

/// Application settings. Replaced by `dbiji_config::settings::AppSettings`
/// once the configuration crate is wired in.
#[derive(Debug, Clone)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub high_contrast: bool,
    pub fetch_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { theme: ThemeChoice::System, high_contrast: false, fetch_size: 200 }
    }
}

/// Shared services (stores, history, projects, scheduler ...).
#[derive(Default)]
pub struct Services {}
