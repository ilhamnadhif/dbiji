//! Routes application commands that open tools and dialogs.

use crate::app::DbijiApp;
use crate::command::AppCommand;
use dbiji_core::tr;

pub fn route(app: &mut DbijiApp, _ctx: &egui::Context, cmd: AppCommand) {
    match cmd {
        AppCommand::OpenObject { connection, object, page } => {
            let key = crate::tabs::object::ObjectEditorTab::key(&connection, &object);
            let existing = app.dock.iter_all_tabs().find(|(_, t)| t.view.dedupe_key().as_deref() == Some(key.as_str())).map(|(p, _)| p);
            match existing {
                Some(path) => {
                    let _ = app.dock.set_active_tab(path);
                    if let Some((_, tab)) = app.dock.find_active_focused()
                        && let Some(t) = tab.view.as_any_mut().downcast_mut::<crate::tabs::object::ObjectEditorTab>()
                    {
                        t.page = page;
                    }
                }
                None => {
                    app.open_tab(Box::new(crate::tabs::object::ObjectEditorTab::new(connection, object, page)));
                }
            }
        }
        _ => app.toasts.info(tr("This feature is not available in this build yet.")),
    }
}
