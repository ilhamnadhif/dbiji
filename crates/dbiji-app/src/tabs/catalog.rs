//! Object editor for catalog objects (roles, tablespaces, users, foreign
//! servers, policies …): their properties, DDL and sub-objects (e.g. the
//! members of a role).

use crate::backend::Loadable;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_core::catalog::{self, CatalogContext, CatalogFolder, CatalogRow};
use dbiji_core::{SchemaRef, tr};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

pub struct CatalogObjectTab {
    conn: String,
    folder_id: &'static str,
    ctx: CatalogContext,
    folder: Option<CatalogFolder>,
    row: Loadable<Option<CatalogRow>>,
    ddl: Loadable<Option<String>>,
    children: Vec<(CatalogFolder, Loadable<Vec<CatalogRow>>)>,
    started: bool,
}

impl CatalogObjectTab {
    pub fn new(conn: String, folder: &'static str, ctx: CatalogContext) -> Self {
        CatalogObjectTab { conn, folder_id: folder, ctx, folder: None, row: Loadable::NotLoaded, ddl: Loadable::NotLoaded, children: Vec::new(), started: false }
    }

    pub fn key(conn: &str, folder: &str, ctx: &CatalogContext) -> String {
        format!("catalog:{conn}:{folder}:{:?}:{:?}:{:?}:{:?}:{:?}", ctx.database, ctx.schema, ctx.table, ctx.parent, ctx.name)
    }

    /// The loaded DDL.
    pub fn ddl_text(&self) -> Option<&str> {
        self.ddl.get().and_then(|d| d.as_deref())
    }

    /// The loaded properties: (column, value as text).
    pub fn properties(&self) -> Vec<(String, String)> {
        match &self.row {
            Loadable::Loaded(Some(r)) => r.values.iter().map(|(k, v)| (k.clone(), v.to_text().into_owned())).collect(),
            _ => Vec::new(),
        }
    }

    fn name(&self) -> &str {
        self.ctx.name.as_deref().unwrap_or("")
    }

    fn start(&mut self, cx: &mut AppContext) {
        let Some(live) = cx.conns.live_mut(&self.conn) else { return };
        let Some(folder) = live.catalog_folder(self.folder_id).cloned() else { return };
        let Some(session) = live.ensure_session(cx.backend, self.ctx.database.as_deref()) else { return };
        self.started = true;
        let (s, f, c, name) = (session.clone(), folder.clone(), self.ctx.clone(), self.name().to_string());
        let list_ctx = CatalogContext { name: None, ..c.clone() };
        self.row = Loadable::Loading(cx.backend.spawn(async move {
            let rows = catalog::list(s.lock().await.as_mut(), &f, &list_ctx).await?;
            Ok(rows.into_iter().find(|r| r.name == name))
        }));
        let (s, f, c) = (session.clone(), folder.clone(), self.ctx.clone());
        self.ddl = Loadable::Loading(cx.backend.spawn(async move { catalog::ddl(s.lock().await.as_mut(), &f, &c).await }));
        self.children = folder
            .children
            .iter()
            .filter_map(|id| live.catalog_folder(id).cloned())
            .map(|child| {
                let (s, f) = (session.clone(), child.clone());
                let c = CatalogContext { parent: self.ctx.name.clone(), name: None, ..self.ctx.clone() };
                let job = cx.backend.spawn(async move { catalog::list(s.lock().await.as_mut(), &f, &c).await });
                (child, Loadable::Loading(job))
            })
            .collect();
        self.folder = Some(folder);
    }

    fn poll(&mut self) -> bool {
        let mut busy = false;
        self.row.poll();
        self.ddl.poll();
        busy |= self.row.is_loading() || self.ddl.is_loading();
        for (_, l) in &mut self.children {
            l.poll();
            busy |= l.is_loading();
        }
        busy
    }
}

impl TabView for CatalogObjectTab {
    fn title(&self) -> String {
        self.name().to_string()
    }

    fn icon(&self) -> &'static str {
        self.folder.as_ref().map(|f| crate::nav::catalog_icon(f.icon)).unwrap_or(icon::CUBE)
    }

    fn tooltip(&self) -> Option<String> {
        self.folder.as_ref().map(|f| format!("{} › {}", tr(f.label), self.name()))
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(Self::key(&self.conn, self.folder_id, &self.ctx))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !self.started {
            if !cx.conns.is_connected(&self.conn) {
                ui.weak(tr("not connected"));
                return;
            }
            self.start(cx);
        }
        if self.poll() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let label = self.folder.as_ref().map(|f| tr(f.label)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.heading(format!("{} {}", self.icon(), self.name()));
            ui.weak(label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(dbiji_icons::ic(icon::ARROW_CLOCKWISE)).on_hover_text(tr("Refresh")).clicked() {
                    self.started = false;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.strong(tr("Properties"));
            match &self.row {
                Loadable::Loaded(Some(row)) => {
                    egui::Grid::new(("catalog_props", &self.conn, self.folder_id)).num_columns(2).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                        for (k, v) in &row.values {
                            if k.eq_ignore_ascii_case("detail") {
                                continue;
                            }
                            ui.label(RichText::new(k.replace('_', " ")).weak());
                            let text = if v.is_null() { "NULL".to_string() } else { v.to_text().into_owned() };
                            ui.add(egui::Label::new(text).wrap());
                            ui.end_row();
                        }
                    });
                }
                Loadable::Loaded(None) => {
                    ui.weak(tr("The object no longer exists."));
                }
                Loadable::Failed(e) => {
                    ui.colored_label(crate::theme::error_color(ui.visuals().dark_mode), e.full_text());
                }
                _ => {
                    ui.spinner();
                }
            }
            for (folder, items) in &self.children {
                ui.add_space(8.0);
                ui.strong(tr(folder.label));
                match items {
                    Loadable::Loaded(v) if v.is_empty() => {
                        ui.weak(tr("(empty)"));
                    }
                    Loadable::Loaded(v) => {
                        for it in v {
                            ui.horizontal(|ui| {
                                ui.label(format!("{} {}", crate::nav::catalog_icon(folder.icon), it.name));
                                if let Some(d) = &it.detail {
                                    ui.weak(d);
                                }
                            });
                        }
                    }
                    Loadable::Failed(e) => {
                        ui.colored_label(crate::theme::error_color(ui.visuals().dark_mode), e.full_text());
                    }
                    _ => {
                        ui.spinner();
                    }
                }
            }
            match &self.ddl {
                Loadable::Loaded(Some(ddl)) => {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.strong("DDL");
                        if ui.small_button((dbiji_icons::ic(icon::COPY), tr("Copy"))).clicked() {
                            ui.ctx().copy_text(ddl.clone());
                        }
                        if ui.small_button((dbiji_icons::ic(icon::FILE_SQL), tr("Open in SQL editor"))).clicked() {
                            let schema = self.ctx.database.clone().map(|d| SchemaRef { database: Some(d), schema: None });
                            cx.push(AppCommand::NewSqlEditor { connection: Some(self.conn.clone()), schema, text: Some(ddl.clone()) });
                        }
                    });
                    crate::tabs::code(ui, cx, ("catalog_ddl", &self.conn, self.folder_id), ddl);
                }
                Loadable::Failed(e) => {
                    ui.add_space(8.0);
                    ui.strong("DDL");
                    ui.colored_label(crate::theme::error_color(ui.visuals().dark_mode), e.full_text());
                }
                _ => {}
            }
        });
    }

    crate::impl_tab_any!();
}
