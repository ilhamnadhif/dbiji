//! Demo application shared by the eframe demo and the headless snapshot tool.
//! A background thread plays the role of the application's fetch task.

#![allow(dead_code)]

use super::datagen;
use dbiji_core::{ColumnMeta, DataKind, ObjectRef, RowStore, SharedRowStore, Value};
use dbiji_grid::model::ForeignKeyLink;
use dbiji_grid::panels::{Relation, RelationDirection};
use dbiji_grid::{GridAction, GridOptions, ResultModel, ResultViewState, ResultViewer};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CATEGORIES: [&str; 12] = ["Books", "Music", "Garden", "Toys", "Food", "Sports", "Health", "Office", "Travel", "Pets", "Tools", "Games"];

/// Simulated fetch running on another thread.
struct Fetch {
    done: Arc<AtomicBool>,
    fetched: Arc<AtomicUsize>,
    started: Instant,
}

/// The demo application.
pub struct DemoApp {
    pub model: ResultModel,
    pub view: ResultViewState,
    pub options: GridOptions,
    pub total_rows: usize,
    pub ncols: usize,
    pub page: usize,
    fetch: Option<Fetch>,
    pub save_preview: Option<String>,
    pub log: Vec<String>,
    pub dark: bool,
    /// Stream all remaining rows in the background on the first frame.
    pub fetch_all_at_start: bool,
}

fn new_store() -> SharedRowStore {
    RowStore::shared(dbiji_core::rowstore::DEFAULT_STORE_BUDGET)
}

impl DemoApp {
    /// Demo over `total_rows` × `ncols`; `initial` rows are fetched synchronously.
    pub fn new(total_rows: usize, ncols: usize, initial: usize) -> Self {
        let store = new_store();
        let initial = initial.min(total_rows);
        datagen::fill(&store, initial, ncols, 42);
        let mut model = ResultModel::new(datagen::columns(ncols), store);
        Self::configure(&mut model, total_rows, initial);
        let mut view = ResultViewState::new(&model);
        let names = model.column_names();
        // Widen a few interesting columns.
        for (c, n) in names.iter().enumerate() {
            if n == "notes" || n == "attributes" {
                view.grid.set_column_width(c, 260.0);
            }
        }
        view.grid.select_cell(0, 1);
        view.references.set_relations(vec![
            Relation {
                name: Some("big_category_fk".into()),
                direction: RelationDirection::References,
                table: ObjectRef::table(Some("demo".into()), "categories"),
                columns: vec!["id".into()],
                key_columns: vec![3],
            },
            Relation {
                name: Some("orders_customer_fk".into()),
                direction: RelationDirection::ReferencedBy,
                table: ObjectRef::table(Some("demo".into()), "orders"),
                columns: vec!["customer_id".into()],
                key_columns: vec![0],
            },
        ]);
        let options = GridOptions { sql_table_name: Some("demo.big".into()), ..Default::default() };
        DemoApp { model, view, options, total_rows, ncols, page: 5_000, fetch: None, save_preview: None, log: Vec::new(), dark: true, fetch_all_at_start: false }
    }

    fn configure(model: &mut ResultModel, total_rows: usize, fetched: usize) {
        let target = dbiji_grid::determine_editability(model.columns(), |_| Some(vec!["id".into()]), false);
        model.set_editability(target);
        let dict: HashMap<String, String> = CATEGORIES.iter().enumerate().map(|(i, n)| ((i + 1).to_string(), n.to_string())).collect();
        model.set_dictionary(3, dict);
        model.set_enum_values(7, ["Jakarta", "Bandung", "Surabaya", "Medan", "Denpasar", "Yogyakarta", "Makassar", "Semarang"].iter().map(|s| s.to_string()).collect());
        model.set_foreign_keys(vec![ForeignKeyLink {
            name: Some("big_category_fk".into()),
            columns: vec![3],
            ref_table: ObjectRef::table(Some("demo".into()), "categories"),
            ref_columns: vec!["id".into()],
        }]);
        let _ = model.add_virtual_column("amount × 2", "amount * 2");
        model.fetch.has_more = fetched < total_rows;
        model.fetch.elapsed = Some(Duration::from_millis(37));
    }

    /// Start appending rows in the background up to `until` rows.
    fn start_fetch(&mut self, until: usize, ctx: &egui::Context) {
        if self.fetch.is_some() {
            return;
        }
        let store = self.model.store().clone();
        let from = self.model.stored_row_count();
        let until = until.min(self.total_rows);
        if from >= until {
            return;
        }
        let done = Arc::new(AtomicBool::new(false));
        let fetched = Arc::new(AtomicUsize::new(from));
        let (d, f, ncols, ctx) = (done.clone(), fetched.clone(), self.ncols, ctx.clone());
        std::thread::spawn(move || {
            let mut rng = datagen::Rng::new(42 + from as u64);
            let mut batch = Vec::with_capacity(2048);
            for i in from..until {
                batch.push(datagen::row(i, ncols, &mut rng));
                if batch.len() == 2048 {
                    store.lock().extend(batch.drain(..));
                    f.store(i + 1, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            }
            store.lock().extend(batch.drain(..));
            f.store(until, Ordering::Relaxed);
            d.store(true, Ordering::Release);
            ctx.request_repaint();
        });
        self.model.fetch.fetching = true;
        self.fetch = Some(Fetch { done, fetched, started: Instant::now() });
    }

    fn poll_fetch(&mut self) {
        let Some(f) = &self.fetch else { return };
        if f.done.load(Ordering::Acquire) {
            self.model.fetch.fetching = false;
            self.model.fetch.elapsed = Some(f.started.elapsed());
            let n = f.fetched.load(Ordering::Relaxed);
            self.model.fetch.has_more = n < self.total_rows;
            self.fetch = None;
        }
    }

    fn refresh(&mut self) {
        let store = new_store();
        datagen::fill(&store, 1_000.min(self.total_rows), self.ncols, 42);
        self.model.reset(datagen::columns(self.ncols), store);
        self.model.fetch.has_more = self.model.stored_row_count() < self.total_rows;
        self.model.fetch.total_count = None;
        self.log("Refreshed".into());
    }

    fn log(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 50 {
            self.log.remove(0);
        }
    }

    fn handle(&mut self, action: GridAction, ctx: &egui::Context) {
        match action {
            GridAction::FetchMore => {
                let until = self.model.stored_row_count() + self.page;
                self.start_fetch(until, ctx);
            }
            GridAction::FetchAll => self.start_fetch(self.total_rows, ctx),
            GridAction::CountTotal => self.model.fetch.total_count = Some(self.total_rows as u64),
            GridAction::Refresh => self.refresh(),
            GridAction::SaveChanges => {
                let d = dbiji_core::dialect::GenericDialect::new();
                let sql = self.model.change_preview_sql(&d);
                self.save_preview = Some(sql);
            }
            GridAction::RequestReferences { relation, values } => {
                let (cols, rows) = if relation == 0 {
                    let id = values.first().and_then(Value::as_i64).unwrap_or(0);
                    (
                        vec![ColumnMeta::new("id", "int4", DataKind::Int), ColumnMeta::new("name", "text", DataKind::Text)],
                        vec![vec![Value::Int(id), Value::text(CATEGORIES.get((id - 1).max(0) as usize).copied().unwrap_or("?"))]],
                    )
                } else {
                    let cust = values.first().and_then(Value::as_i64).unwrap_or(0);
                    (
                        vec![
                            ColumnMeta::new("order_id", "int8", DataKind::Int),
                            ColumnMeta::new("customer_id", "int8", DataKind::Int),
                            ColumnMeta::new("total", "numeric", DataKind::Decimal),
                        ],
                        (0..3).map(|k| vec![Value::Int(cust * 10 + k), Value::Int(cust), Value::decimal(format!("{}.95", k * 17 + 3))]).collect(),
                    )
                };
                self.view.references.set_related_rows(relation, values, cols, rows);
            }
            GridAction::SelectionChanged => {}
            other => {
                let s = format!("{other:?}");
                let s = if s.len() > 160 { format!("{}…", &s[..160]) } else { s };
                self.log(s);
            }
        }
    }

    /// The whole UI.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll_fetch();
        if std::mem::take(&mut self.fetch_all_at_start) {
            let ctx = ui.ctx().clone();
            self.start_fetch(self.total_rows, &ctx);
        }
        let ctx = ui.ctx().clone();
        egui::Panel::top("demo_top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("DBiji grid demo");
                ui.weak(format!("{} rows × {} columns on the 'server'", self.total_rows, self.ncols));
                if ui.button(if self.dark { "☀ Light" } else { "🌙 Dark" }).clicked() {
                    self.dark = !self.dark;
                    ui.ctx().set_theme(if self.dark { egui::Theme::Dark } else { egui::Theme::Light });
                }
                ui.checkbox(&mut self.options.format.thousands_separator, "Thousands separator");
                let mut cb = self.options.format.bool_style == dbiji_core::value::BoolStyle::Checkbox;
                if ui.checkbox(&mut cb, "Checkbox booleans").changed() {
                    self.options.format.bool_style = if cb { dbiji_core::value::BoolStyle::Checkbox } else { dbiji_core::value::BoolStyle::TrueFalse };
                }
                ui.checkbox(&mut self.options.show_row_numbers, "Row numbers");
            });
        });
        egui::Panel::bottom("demo_log").resizable(true).default_size(70.0).show(ui, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l);
                }
                if self.log.is_empty() {
                    ui.weak("Actions emitted by the grid appear here.");
                }
            });
        });
        let actions = egui::CentralPanel::default().show(ui, |ui| ResultViewer::new(&mut self.model, &mut self.view, &self.options).show(ui).actions).inner;
        for a in actions {
            self.handle(a, &ctx);
        }
        if let Some(sql) = self.save_preview.clone() {
            let mut open = true;
            let mut apply = false;
            egui::Window::new("Save changes — preview SQL").open(&mut open).default_size([560.0, 300.0]).show(&ctx, |ui| {
                egui::ScrollArea::both().max_height(240.0).show(ui, |ui| {
                    let mut s = sql.as_str();
                    ui.add(egui::TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                });
                ui.horizontal(|ui| {
                    if ui.button("Apply (simulate commit)").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.save_preview = None;
                    }
                });
            });
            if apply {
                let n = self.model.pending_counts().total();
                self.model.apply_saved();
                self.log(format!("Saved {n} change(s)"));
                self.save_preview = None;
            } else if !open {
                self.save_preview = None;
            }
        }
    }
}
