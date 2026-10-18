//! Routes application commands that open tools and dialogs.

use crate::app::DbijiApp;
use crate::command::AppCommand;
use dbiji_core::tr;

pub fn route(app: &mut DbijiApp, _ctx: &egui::Context, cmd: AppCommand) {
    let _ = cmd;
    app.toasts.info(tr("This feature is not available in this build yet."));
}
