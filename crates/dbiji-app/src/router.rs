//! Routes application commands that open tools and dialogs.

use crate::app::DbijiApp;
use crate::command::AppCommand;
use dbiji_core::tr;

pub fn route(app: &mut DbijiApp, _ctx: &egui::Context, cmd: AppCommand) {
    match cmd {
        AppCommand::MoveConnection { id, to } => {
            use crate::command::MovePos;
            let Some(folder) = app.conns.config(&id).map(|c| c.folder.clone()) else { return };
            let mut siblings: Vec<(Option<u32>, String, String)> =
                app.conns.configs().iter().filter(|c| c.folder == folder).map(|c| (c.sort_index, c.name.to_lowercase(), c.id.clone())).collect();
            siblings.sort_by_key(|(i, n, _)| (i.unwrap_or(u32::MAX), n.clone()));
            let mut order: Vec<String> = siblings.into_iter().map(|(_, _, id)| id).collect();
            let Some(pos) = order.iter().position(|c| *c == id) else { return };
            let item = order.remove(pos);
            let new_pos = match to {
                MovePos::Top => 0,
                MovePos::Up => pos.saturating_sub(1),
                MovePos::Down => (pos + 1).min(order.len()),
                MovePos::Bottom => order.len(),
            };
            order.insert(new_pos, item);
            for (i, c) in order.iter().enumerate() {
                if let Some(cfg) = app.conns.config_mut(c) {
                    cfg.sort_index = Some(i as u32);
                }
            }
        }
        AppCommand::HideInNavigator { connection, object } => {
            let update = |f: &mut dbiji_core::config::ObjectFilters| {
                let nf = crate::nav::name_filter_mut(f, object.kind);
                if !nf.exclude.contains(&object.name) {
                    nf.exclude.push(object.name.clone());
                }
                nf.enabled = true;
            };
            if let Some(cfg) = app.conns.config_mut(&connection) {
                update(&mut cfg.filters);
            }
            if let Some(live) = app.conns.live_mut(&connection) {
                update(&mut live.config.filters);
            }
        }
        AppCommand::ClearNavigatorFilter { connection, kind } => {
            let clear = |f: &mut dbiji_core::config::ObjectFilters| *crate::nav::name_filter_mut(f, kind) = Default::default();
            if let Some(cfg) = app.conns.config_mut(&connection) {
                clear(&mut cfg.filters);
            }
            if let Some(live) = app.conns.live_mut(&connection) {
                clear(&mut live.config.filters);
            }
        }
        AppCommand::SetActiveProject(name) => {
            if let Some(p) = app.services.projects.as_ref()
                && let Err(e) = p.set_active(&name)
            {
                app.toasts.error(dbiji_core::tr("Cannot switch the project"), Some(e.to_string()));
            }
        }
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
