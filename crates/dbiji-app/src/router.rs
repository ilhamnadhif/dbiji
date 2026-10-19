//! Routes application commands that open tools and dialogs.

use crate::app::DbijiApp;
use crate::command::AppCommand;
use dbiji_core::tr;

pub fn route(app: &mut DbijiApp, _ctx: &egui::Context, cmd: AppCommand) {
    match cmd {
        AppCommand::OpenCatalogObject { connection, folder, ctx } => {
            if !app.conns.is_connected(&connection) {
                app.request_connect(&connection);
            }
            app.open_tab(Box::new(crate::tabs::catalog::CatalogObjectTab::new(connection, folder, ctx)));
        }
        AppCommand::OpenCatalogTable { connection, folder, ctx } => {
            let Some(live) = app.conns.live(&connection) else { return };
            let Some(f) = live.catalog_folder(folder) else { return };
            let sql = dbiji_core::catalog::fill(&f.list_sql, &ctx, live.dialect.as_ref());
            let title = match &ctx.table {
                Some(t) => format!("{} · {t}", dbiji_core::tr(f.label)),
                None => dbiji_core::tr(f.label).to_string(),
            };
            let schema = ctx.database.clone().map(|d| dbiji_core::SchemaRef { database: Some(d), schema: None });
            let mut tab = crate::tabs::sql_editor::SqlEditorTab::new(Some(connection), schema, sql, None, false, title);
            tab.auto_run = true;
            app.open_tab(Box::new(tab));
        }
        AppCommand::RefreshCatalog { connection, folder } => {
            if let Some(live) = app.conns.live_mut(&connection) {
                match folder {
                    Some(f) => live.meta.clear_catalog(f),
                    None => live.meta.catalog.clear(),
                }
            }
        }
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
