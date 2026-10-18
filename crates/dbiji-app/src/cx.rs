//! Per-frame context handed to tabs, panels and dialogs.

use crate::backend::Backend;
use crate::command::AppCommand;
use crate::conn::ConnectionManager;
use crate::ops::Operations;
use crate::services::Services;
use crate::toasts::Toasts;

pub struct AppContext<'a> {
    pub backend: &'a Backend,
    pub conns: &'a mut ConnectionManager,
    pub services: &'a Services,
    pub settings: &'a mut crate::services::Settings,
    pub commands: &'a mut Vec<AppCommand>,
    pub toasts: &'a mut Toasts,
    pub ops: &'a mut Operations,
    pub keymap: &'a crate::keys::Keymap,
    /// The tab currently being drawn is the focused one (receives shortcuts).
    pub tab_active: bool,
}

impl AppContext<'_> {
    pub fn push(&mut self, cmd: AppCommand) {
        self.commands.push(cmd);
    }
}
