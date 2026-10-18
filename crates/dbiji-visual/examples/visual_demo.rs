//! Interactive demo of every dbiji-visual component.
//!
//! `cargo run -p dbiji-visual --example visual_demo`
//!
//! The demo plays the role of the application: it answers the events of the
//! components (adding tables to the query builder, applying ER edit-mode
//! changes to the model, executing custom dashboard queries with fake data,
//! feeding fake server metrics to the health dashboard).

#[path = "common/demo_data.rs"]
mod demo_data;

use dbiji_core::ddl::TableDef;
use dbiji_core::i18n::{Locale, set_locale};
use dbiji_core::plan::QueryPlan;
use dbiji_core::value::{ColumnMeta, DataKind, Row, Value};
use dbiji_core::{admin::Metric, tr};
use dbiji_visual::builder::{QbEvent, QbViewState, QueryModel, show_query_builder};
use dbiji_visual::chart::{ChartConfig, ChartData, ChartKind, ChartState, chart_config_ui, export_chart_svg, show_chart};
use dbiji_visual::dashboard::{
    CustomDashboard, CustomDashboardEvent, CustomDashboardState, DashboardItem, DashboardLayout, DashboardState,
    show_custom_dashboard, show_dashboard,
};
use dbiji_visual::er::{
    ErColumn, ErDiagram, ErEvent, ErModel, ErRelation, ErTable, ErTableKind, ErViewState, export_png, export_svg,
    show_er,
};
use dbiji_visual::plan::{PlanViewState, plan_summary, show_plan_diagram, show_plan_text, show_plan_tree};
use dbiji_visual::spatial::{SpatialLayer, SpatialState, show_spatial};
use eframe::egui;
use std::collections::BTreeSet;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Er,
    Plan,
    Builder,
    Charts,
    Dashboard,
    Custom,
    Spatial,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanView {
    Diagram,
    Tree,
    Text,
    Summary,
}

struct DemoApp {
    tab: Tab,
    log: Vec<String>,
    // ER
    er: ErDiagram,
    er_state: ErViewState,
    // plan
    plan: QueryPlan,
    plan_state: PlanViewState,
    plan_view: PlanView,
    // builder
    qb: QueryModel,
    qb_state: QbViewState,
    qb_tables: Vec<TableDef>,
    qb_picker: bool,
    // charts
    chart_columns: Vec<ColumnMeta>,
    chart_rows: Vec<Row>,
    chart_config: ChartConfig,
    chart_data: ChartData,
    chart_state: ChartState,
    // dashboards
    started: Instant,
    last_push: f64,
    dash: DashboardState,
    dash_layout: DashboardLayout,
    counters: [f64; 5],
    rng: demo_data::Rng,
    custom: CustomDashboard,
    custom_state: CustomDashboardState,
    // spatial
    layers: Vec<SpatialLayer>,
    spatial_state: SpatialState,
    indonesian: bool,
}

impl DemoApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        cc.egui_ctx.set_fonts(fonts);

        let snapshot = demo_data::schema(200, 42);
        let er = ErDiagram::new("Generated schema (200 tables)", ErModel::from_snapshot(&snapshot, None));

        let qb_tables: Vec<TableDef> = snapshot.tables.iter().take(40).cloned().collect();
        let mut qb = QueryModel::default();
        if let (Some(a), Some(b)) = (qb_tables.get(1), qb_tables.first()) {
            qb.add_table(b);
            let alias = qb.add_table(a);
            if let Some(c) = a.columns.get(1) {
                qb.set_column_selected(&alias, &c.name, true);
            }
        }

        let (chart_columns, chart_rows) = demo_data::chart_rows(90, 3);
        let chart_config = ChartConfig {
            kind: ChartKind::Line,
            x_column: Some(0),
            y_columns: vec![3],
            series_column: Some(1),
            aggregate: dbiji_visual::chart::ChartAggregate::Sum,
            ..Default::default()
        };
        let chart_data = ChartData::build(&chart_columns, &chart_rows, &chart_config);

        let mut custom = CustomDashboard { name: "Sales overview".into(), columns: 2, ..Default::default() };
        custom.add_item(DashboardItem {
            title: "Revenue by region".into(),
            sql: "SELECT region, sum(revenue) FROM sales GROUP BY 1".into(),
            chart: ChartConfig { kind: ChartKind::Bar, x_column: Some(0), y_columns: vec![1], ..Default::default() },
            refresh_secs: 10,
            ..Default::default()
        });
        custom.add_item(DashboardItem {
            title: "Orders per channel".into(),
            sql: "SELECT channel, count(*) FROM orders GROUP BY 1".into(),
            chart: ChartConfig { kind: ChartKind::Pie, x_column: Some(0), y_columns: vec![1], ..Default::default() },
            refresh_secs: 0,
            ..Default::default()
        });

        let (sp_cols, sp_rows) = demo_data::spatial_rows();
        let geom_col = sp_cols.iter().position(|c| c.kind == DataKind::Geometry).unwrap_or(2);
        let layers = vec![SpatialLayer::from_column("Demo geometries", &sp_rows, geom_col, Some(1))];

        DemoApp {
            tab: Tab::Er,
            log: Vec::new(),
            er,
            er_state: ErViewState::default(),
            plan: demo_data::sample_plan(),
            plan_state: PlanViewState::default(),
            plan_view: PlanView::Diagram,
            qb,
            qb_state: QbViewState::default(),
            qb_tables,
            qb_picker: false,
            chart_columns,
            chart_rows,
            chart_config,
            chart_data,
            chart_state: ChartState::default(),
            started: Instant::now(),
            last_push: -1.0,
            dash: DashboardState::new(),
            dash_layout: DashboardLayout { id: "demo".into(), title: Some("PostgreSQL demo server".into()), ..Default::default() },
            counters: [0.0; 5],
            rng: demo_data::Rng::new(11),
            custom,
            custom_state: CustomDashboardState::default(),
            layers,
            spatial_state: SpatialState::default(),
            indonesian: false,
        }
    }

    fn log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Fake server metrics, one sample per second.
    fn poll_metrics(&mut self) {
        let t = self.now();
        if t - self.last_push < 1.0 {
            return;
        }
        self.last_push = t;
        let wave = (t / 20.0).sin();
        let active = (12.0 + wave * 6.0 + self.rng.f64() * 4.0).round();
        self.counters[0] += 40.0 + self.rng.f64() * 30.0 + wave * 15.0; // commits
        self.counters[1] += self.rng.f64() * 2.0; // rollbacks
        self.counters[2] += 8192.0 * (50.0 + self.rng.f64() * 200.0); // bytes read
        self.counters[3] += 8192.0 * (500.0 + self.rng.f64() * 900.0); // bytes hit
        self.counters[4] += 1000.0 + self.rng.f64() * 4000.0; // rows returned
        if t > 45.0 && t < 47.0 {
            self.counters[0] = 0.0; // simulated statistics reset
        }
        let metrics = vec![
            Metric::gauge("Connections", "conn.active", "Active", active, ""),
            Metric::gauge("Connections", "conn.idle", "Idle", 30.0 - active, ""),
            Metric::gauge("Connections", "conn.waiting", "Waiting for locks", (self.rng.f64() * 2.0).floor(), ""),
            Metric::counter("Transactions", "xact.commit", "Commits", self.counters[0], ""),
            Metric::counter("Transactions", "xact.rollback", "Rollbacks", self.counters[1], ""),
            Metric::counter("I/O", "blks.read", "Read from disk", self.counters[2], "bytes"),
            Metric::counter("I/O", "blks.hit", "Read from cache", self.counters[3], "bytes"),
            Metric::gauge("I/O", "cache.ratio", "Cache hit ratio", 92.0 + self.rng.f64() * 7.0, "%"),
            Metric::counter("Rows", "tup.returned", "Rows returned", self.counters[4], "rows"),
        ];
        self.dash.push(t, &metrics);
    }

    fn er_tab(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("er_log").resizable(true).default_size(110.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(tr("Events"));
                ui.weak("(the demo applies edit-mode events to the model, like the app would after a DDL preview)");
            });
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l.as_str());
                }
            });
        });
        let resp = show_er(ui, &mut self.er, &mut self.er_state);
        for e in resp.events {
            self.handle_er_event(e);
        }
    }

    fn handle_er_event(&mut self, e: ErEvent) {
        self.log(format!("{e:?}"));
        match e {
            ErEvent::CreateTable { pos } => {
                let n = self.er.model.tables.len();
                let t = ErTable::new(Some("public"), format!("new_table_{n}"), ErTableKind::Table)
                    .with_column(ErColumn::new("id", "bigint").primary_key());
                self.er.positions.insert(t.key.clone(), pos);
                self.er.model.tables.push(t);
            }
            ErEvent::AddColumn(key) => {
                if let Some(t) = self.er.model.table_mut(&key) {
                    let n = t.columns.len();
                    t.columns.push(ErColumn::new(format!("new_column_{n}"), "text"));
                }
            }
            ErEvent::DeleteTables(keys) => {
                let set: BTreeSet<String> = keys.into_iter().collect();
                self.er.remove_tables(&set);
            }
            ErEvent::CreateRelation { from_table, from_columns, to_table, to_columns } => {
                let name = format!("fk_{}_{}", from_table.replace('.', "_"), from_columns.join("_"));
                self.er.model.relations.push(ErRelation {
                    name,
                    from_table,
                    from_columns,
                    to_table,
                    to_columns,
                    identifying: false,
                    optional: true,
                });
                self.er.model.refresh_fk_flags();
            }
            ErEvent::DeleteRelation(r) => {
                self.er.model.relations.retain(|x| !(x.name == r.name && x.from_table == r.from_table));
                self.er.model.refresh_fk_flags();
            }
            ErEvent::ExportRequested => {
                let dir = std::env::temp_dir();
                let svg = dir.join("dbiji_er_demo.svg");
                let png = dir.join("dbiji_er_demo.png");
                let r1 = std::fs::write(&svg, export_svg(&self.er));
                let r2 = match export_png(&self.er, 1.0) {
                    Ok(bytes) => std::fs::write(&png, bytes).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                self.log(format!("export: {} {:?} / {} {:?}", svg.display(), r1.err(), png.display(), r2.err()));
            }
            _ => {}
        }
    }

    fn plan_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.plan_view, PlanView::Diagram, tr("Diagram"));
            ui.selectable_value(&mut self.plan_view, PlanView::Tree, tr("Tree"));
            ui.selectable_value(&mut self.plan_view, PlanView::Text, tr("Text"));
            ui.selectable_value(&mut self.plan_view, PlanView::Summary, tr("Summary"));
        });
        ui.separator();
        match self.plan_view {
            PlanView::Diagram => {
                show_plan_diagram(ui, &self.plan, &mut self.plan_state);
            }
            PlanView::Tree => {
                show_plan_tree(ui, &self.plan, &mut self.plan_state);
            }
            PlanView::Text => show_plan_text(ui, &self.plan),
            PlanView::Summary => {
                let s = plan_summary(&self.plan);
                ui.label(dbiji_visual::plan::summary_line(&s));
                ui.heading(tr("Most expensive nodes"));
                for h in &s.hotspots {
                    ui.label(format!("{:5.1}%  {}", h.share * 100.0, h.title));
                }
                ui.heading(tr("Warnings"));
                for w in &s.warnings {
                    ui.label(format!("⚠ {}", w.message));
                }
            }
        }
    }

    fn builder_tab(&mut self, ui: &mut egui::Ui) {
        let resp = show_query_builder(ui, &mut self.qb, &mut self.qb_state);
        for e in resp.events {
            match e {
                QbEvent::AddTableRequested => self.qb_picker = true,
                QbEvent::Changed => {}
                other => self.log(format!("{other:?}")),
            }
        }
        if self.qb_picker {
            let mut open = true;
            let mut chosen = None;
            egui::Window::new(tr("Add table")).open(&mut open).default_height(400.0).show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, t) in self.qb_tables.iter().enumerate() {
                        if ui.button(t.name.as_str()).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            });
            if let Some(i) = chosen {
                if let Some(t) = self.qb_tables.get(i).cloned() {
                    self.qb.add_table(&t);
                }
                open = false;
            }
            self.qb_picker = open;
        }
    }

    fn charts_tab(&mut self, ui: &mut egui::Ui) {
        if chart_config_ui(ui, &self.chart_columns, &mut self.chart_config) {
            self.chart_data = ChartData::build(&self.chart_columns, &self.chart_rows, &self.chart_config);
            self.chart_state.reset_view = true;
        }
        ui.horizontal(|ui| {
            if ui.button(tr("Export SVG")).clicked() {
                let svg = export_chart_svg(&self.chart_data, &self.chart_config, egui::vec2(900.0, 500.0));
                let path = std::env::temp_dir().join("dbiji_chart_demo.svg");
                let r = std::fs::write(&path, svg);
                self.log.push(format!("chart export {}: {:?}", path.display(), r.err()));
            }
            ui.weak(format!("{} rows", self.chart_rows.len()));
        });
        ui.separator();
        show_chart(ui, &self.chart_data, &self.chart_config, &mut self.chart_state);
    }

    fn custom_tab(&mut self, ui: &mut egui::Ui) {
        let now = self.now();
        let events = show_custom_dashboard(ui, &mut self.custom, &mut self.custom_state, now);
        for e in events {
            if let CustomDashboardEvent::Refresh(id) = e {
                // "execute" the query: fake two-column result
                let Some(item) = self.custom.item(id).cloned() else { continue };
                let columns = vec![ColumnMeta::new("label", "text", DataKind::Text), ColumnMeta::new("value", "numeric", DataKind::Decimal)];
                let labels = if item.sql.contains("channel") { vec!["web", "store", "partner", "phone"] } else { vec!["North", "South", "East", "West"] };
                let rows: Vec<Row> = labels
                    .into_iter()
                    .map(|l| vec![Value::text(l), Value::decimal(format!("{:.2}", 100.0 + self.rng.f64() * 900.0))])
                    .collect();
                self.custom_state.set_result(&item, columns, rows, now);
            }
        }
    }
}

impl eframe::App for DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_metrics();
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Er, tr("ER diagram"));
                ui.selectable_value(&mut self.tab, Tab::Plan, tr("Execution plan"));
                ui.selectable_value(&mut self.tab, Tab::Builder, tr("Query builder"));
                ui.selectable_value(&mut self.tab, Tab::Charts, tr("Charts"));
                ui.selectable_value(&mut self.tab, Tab::Dashboard, tr("Server dashboard"));
                ui.selectable_value(&mut self.tab, Tab::Custom, tr("Custom dashboard"));
                ui.selectable_value(&mut self.tab, Tab::Spatial, tr("Spatial"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::widgets::global_theme_preference_switch(ui);
                    if ui.checkbox(&mut self.indonesian, "Bahasa Indonesia").changed() {
                        set_locale(if self.indonesian { Locale::Id } else { Locale::En });
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Er => self.er_tab(ui),
            Tab::Plan => self.plan_tab(ui),
            Tab::Builder => self.builder_tab(ui),
            Tab::Charts => self.charts_tab(ui),
            Tab::Dashboard => {
                let r = show_dashboard(ui, &self.dash, &self.dash_layout);
                if !r.paused {
                    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                }
            }
            Tab::Custom => self.custom_tab(ui),
            Tab::Spatial => {
                let r = show_spatial(ui, &self.layers, &mut self.spatial_state);
                if let Some(row) = r.clicked_row {
                    self.log(format!("spatial click: row {row}"));
                }
            }
        });
        // keep the fake metrics flowing while another tab is shown
        if self.tab != Tab::Dashboard {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 900.0]).with_title("DBiji visual components"),
        ..Default::default()
    };
    eframe::run_native("dbiji-visual-demo", options, Box::new(|cc| Ok(Box::new(DemoApp::new(cc)))))
}
