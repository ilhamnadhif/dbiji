//! Custom dashboards: a grid of charts, each fed by a SQL query that the
//! application runs (on demand or every `refresh_secs`).

use crate::chart::{ChartConfig, ChartData, ChartState, chart_config_ui, show_chart};
use dbiji_core::value::{ColumnMeta, Row};
use dbiji_core::{tr, trf};
use egui::{RichText, Ui, vec2};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// One chart of a custom dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardItem {
    /// Unique id within the dashboard.
    pub id: u64,
    /// Card title.
    pub title: String,
    /// Query producing the chart data.
    pub sql: String,
    /// How the result is charted.
    pub chart: ChartConfig,
    /// Refresh interval in seconds (0: manual refresh only).
    pub refresh_secs: u32,
}

impl Default for DashboardItem {
    fn default() -> Self {
        DashboardItem {
            id: 0,
            title: String::new(),
            sql: String::new(),
            chart: ChartConfig::default(),
            refresh_secs: 60,
        }
    }
}

/// A saved custom dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomDashboard {
    /// Dashboard name.
    pub name: String,
    /// Charts in display order.
    pub items: Vec<DashboardItem>,
    /// Charts per row.
    pub columns: usize,
}

impl Default for CustomDashboard {
    fn default() -> Self {
        CustomDashboard {
            name: String::new(),
            items: Vec::new(),
            columns: 2,
        }
    }
}

impl CustomDashboard {
    /// Adds an item with a fresh id and returns the id.
    pub fn add_item(&mut self, mut item: DashboardItem) -> u64 {
        let id = self.items.iter().map(|i| i.id).max().map_or(1, |m| m + 1);
        item.id = id;
        self.items.push(item);
        id
    }

    /// Item by id.
    pub fn item(&self, id: u64) -> Option<&DashboardItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

/// Last result of an item.
#[derive(Debug, Clone, Default)]
pub struct ItemResult {
    /// Result columns.
    pub columns: Vec<ColumnMeta>,
    /// Result rows.
    pub rows: Vec<Row>,
    /// Chart data built from the rows with the item's chart configuration.
    pub data: ChartData,
    /// When the result arrived (app clock, seconds).
    pub fetched_at: f64,
    /// Error message of the last refresh.
    pub error: Option<String>,
}

/// Runtime state of a custom dashboard (results, pending refreshes, editor).
#[derive(Debug, Clone, Default)]
pub struct CustomDashboardState {
    results: HashMap<u64, ItemResult>,
    pending: BTreeSet<u64>,
    requested_at: HashMap<u64, f64>,
    charts: HashMap<u64, ChartState>,
    editing: Option<u64>,
}

/// Something the application has to do for a custom dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomDashboardEvent {
    /// Run the SQL of this item and call
    /// [`CustomDashboardState::set_result`] / [`CustomDashboardState::set_error`].
    Refresh(u64),
    /// The dashboard definition changed (save it).
    Changed,
}

impl CustomDashboardState {
    /// Stores the result of an item's query (`now`: app clock in seconds).
    pub fn set_result(
        &mut self,
        item: &DashboardItem,
        columns: Vec<ColumnMeta>,
        rows: Vec<Row>,
        now: f64,
    ) {
        let data = ChartData::build(&columns, &rows, &item.chart);
        self.pending.remove(&item.id);
        self.results.insert(
            item.id,
            ItemResult {
                columns,
                rows,
                data,
                fetched_at: now,
                error: None,
            },
        );
        self.charts.entry(item.id).or_default().reset_view = true;
    }

    /// Stores a failed refresh.
    pub fn set_error(&mut self, item_id: u64, error: impl Into<String>, now: f64) {
        self.pending.remove(&item_id);
        let r = self.results.entry(item_id).or_default();
        r.error = Some(error.into());
        r.fetched_at = now;
    }

    /// Result of an item.
    pub fn result(&self, item_id: u64) -> Option<&ItemResult> {
        self.results.get(&item_id)
    }

    /// Items whose refresh is due at `now` (never requested yet, or their
    /// interval elapsed) and that are not already running. Marks them pending.
    pub fn take_due(&mut self, dashboard: &CustomDashboard, now: f64) -> Vec<u64> {
        let mut due = Vec::new();
        for item in &dashboard.items {
            if self.pending.contains(&item.id) || item.sql.trim().is_empty() {
                continue;
            }
            let last = self.requested_at.get(&item.id).copied();
            let is_due = match last {
                None => true,
                Some(t) => item.refresh_secs > 0 && now - t >= item.refresh_secs as f64,
            };
            if is_due {
                due.push(item.id);
            }
        }
        for id in &due {
            self.pending.insert(*id);
            self.requested_at.insert(*id, now);
        }
        due
    }

    /// Seconds until the next item is due (for scheduling a repaint).
    pub fn next_due_in(&self, dashboard: &CustomDashboard, now: f64) -> Option<f64> {
        dashboard
            .items
            .iter()
            .filter(|i| i.refresh_secs > 0 && !self.pending.contains(&i.id))
            .filter_map(|i| {
                self.requested_at
                    .get(&i.id)
                    .map(|t| (t + i.refresh_secs as f64 - now).max(0.0))
            })
            .reduce(f64::min)
    }

    fn request(&mut self, id: u64, now: f64) {
        self.pending.insert(id);
        self.requested_at.insert(id, now);
    }

    fn rebuild(&mut self, item: &DashboardItem) {
        if let Some(r) = self.results.get_mut(&item.id) {
            r.data = ChartData::build(&r.columns, &r.rows, &item.chart);
        }
    }
}

/// Shows a custom dashboard and its editor. `now` is the app clock in
/// seconds (used for refresh intervals). Returns refresh requests and
/// change notifications.
pub fn show_custom_dashboard(
    ui: &mut Ui,
    dashboard: &mut CustomDashboard,
    state: &mut CustomDashboardState,
    now: f64,
) -> Vec<CustomDashboardEvent> {
    let before = dashboard.clone();
    let mut events: Vec<CustomDashboardEvent> = Vec::new();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut dashboard.name)
                .hint_text(tr("Dashboard name"))
                .desired_width(220.0),
        );
        if ui
            .button((dbiji_icons::ic(icon::PLUS), tr("Add chart")))
            .clicked()
        {
            let id = dashboard.add_item(DashboardItem {
                title: tr("New chart").to_string(),
                ..Default::default()
            });
            state.editing = Some(id);
        }
        if ui
            .button((dbiji_icons::ic(icon::ARROWS_CLOCKWISE), tr("Refresh all")))
            .clicked()
        {
            for item in &dashboard.items {
                if !item.sql.trim().is_empty() && !state.pending.contains(&item.id) {
                    state.request(item.id, now);
                    events.push(CustomDashboardEvent::Refresh(item.id));
                }
            }
        }
        ui.separator();
        ui.label(tr("Columns"));
        ui.add(egui::DragValue::new(&mut dashboard.columns).range(1..=4));
    });
    ui.separator();
    for id in state.take_due(dashboard, now) {
        events.push(CustomDashboardEvent::Refresh(id));
    }

    let cols = dashboard.columns.clamp(1, 4);
    let mut delete: Option<u64> = None;
    let mut move_up: Option<u64> = None;
    egui::ScrollArea::vertical()
        .id_salt("dbiji_custom_dashboard")
        .auto_shrink(false)
        .show(ui, |ui| {
            if dashboard.items.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.weak(tr("Add a chart and give it a query to build a dashboard."))
                });
            }
            let avail = ui.available_width();
            let card_w = ((avail - 10.0 * (cols as f32 - 1.0)) / cols as f32).max(200.0);
            let ids: Vec<u64> = dashboard.items.iter().map(|i| i.id).collect();
            for row in ids.chunks(cols) {
                ui.horizontal_top(|ui| {
                    for &id in row {
                        let Some(idx) = dashboard.items.iter().position(|i| i.id == id) else {
                            continue;
                        };
                        ui.allocate_ui(vec2(card_w, 320.0), |ui| {
                            ui.set_width(card_w);
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_width(card_w - 14.0);
                                let item = &mut dashboard.items[idx];
                                card(ui, item, state, now, &mut events, &mut delete, &mut move_up);
                            });
                        });
                    }
                });
                ui.add_space(10.0);
            }
        });
    if let Some(id) = delete {
        dashboard.items.retain(|i| i.id != id);
        state.results.remove(&id);
        state.pending.remove(&id);
        state.requested_at.remove(&id);
        if state.editing == Some(id) {
            state.editing = None;
        }
    }
    if let Some(id) = move_up
        && let Some(i) = dashboard.items.iter().position(|x| x.id == id)
        && i > 0
    {
        dashboard.items.swap(i, i - 1);
    }
    if let Some(wait) = state.next_due_in(dashboard, now) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(wait.clamp(0.05, 3600.0)));
    }
    if *dashboard != before {
        events.push(CustomDashboardEvent::Changed);
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    item: &mut crate::dashboard::custom::DashboardItem,
    state: &mut CustomDashboardState,
    now: f64,
    events: &mut Vec<CustomDashboardEvent>,
    delete: &mut Option<u64>,
    move_up: &mut Option<u64>,
) {
    let id = item.id;
    let editing = state.editing == Some(id);
    ui.horizontal(|ui| {
        ui.strong(if item.title.is_empty() {
            tr("Untitled")
        } else {
            item.title.as_str()
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(dbiji_icons::ic(icon::TRASH))
                .on_hover_text(tr("Delete chart"))
                .clicked()
            {
                *delete = Some(id);
            }
            if ui
                .small_button(dbiji_icons::ic(icon::ARROW_LEFT))
                .on_hover_text(tr("Move earlier"))
                .clicked()
            {
                *move_up = Some(id);
            }
            if ui
                .selectable_label(editing, dbiji_icons::ic(icon::PENCIL_SIMPLE))
                .on_hover_text(tr("Edit chart"))
                .clicked()
            {
                state.editing = if editing { None } else { Some(id) };
            }
            let pending = state.pending.contains(&id);
            if pending {
                ui.spinner();
            } else if ui
                .small_button(dbiji_icons::ic(icon::ARROWS_CLOCKWISE))
                .on_hover_text(tr("Refresh"))
                .clicked()
                && !item.sql.trim().is_empty()
            {
                state.request(id, now);
                events.push(CustomDashboardEvent::Refresh(id));
            }
            if let Some(r) = state.results.get(&id) {
                ui.weak(trf(
                    "{}s ago",
                    &[&((now - r.fetched_at).max(0.0).round() as i64)],
                ));
            }
        });
    });
    if editing {
        let before = item.chart.clone();
        egui::Grid::new(("dbiji_dash_item", id))
            .num_columns(2)
            .spacing(vec2(8.0, 4.0))
            .show(ui, |ui| {
                ui.label(tr("Title"));
                ui.text_edit_singleline(&mut item.title);
                ui.end_row();
                ui.label(tr("Refresh (s)"));
                ui.add(egui::DragValue::new(&mut item.refresh_secs).range(0..=86_400))
                    .on_hover_text(tr("0 = refresh manually"));
                ui.end_row();
            });
        ui.label(tr("SQL"));
        ui.add(
            egui::TextEdit::multiline(&mut item.sql)
                .code_editor()
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        let columns = state
            .results
            .get(&id)
            .map(|r| r.columns.clone())
            .unwrap_or_default();
        if columns.is_empty() {
            ui.weak(tr("Run the query once to choose the chart columns."));
        } else {
            chart_config_ui(ui, &columns, &mut item.chart);
        }
        if item.chart != before {
            state.rebuild(item);
        }
        ui.horizontal(|ui| {
            if ui
                .button((dbiji_icons::ic(icon::PLAY), tr("Run")))
                .clicked()
                && !item.sql.trim().is_empty()
            {
                state.request(id, now);
                events.push(CustomDashboardEvent::Refresh(id));
            }
            if ui.button(tr("Done")).clicked() {
                state.editing = None;
            }
        });
        ui.separator();
    }
    match state.results.get(&id) {
        Some(r) if r.error.is_some() => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                RichText::new(r.error.clone().unwrap_or_default()).small(),
            );
        }
        Some(r) => {
            let chart_state = state.charts.entry(id).or_default();
            ui.allocate_ui(vec2(ui.available_width(), 240.0), |ui| {
                ui.set_height(240.0);
                show_chart(ui, &r.data, &item.chart, chart_state);
            });
        }
        None if item.sql.trim().is_empty() => {
            ui.weak(tr("No query yet"));
        }
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Loading…"));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::ChartKind;
    use dbiji_core::value::{DataKind, Value};

    #[test]
    fn refresh_scheduling() {
        let mut d = CustomDashboard::default();
        let a = d.add_item(DashboardItem {
            title: "a".into(),
            sql: "select 1".into(),
            refresh_secs: 10,
            ..Default::default()
        });
        let b = d.add_item(DashboardItem {
            title: "b".into(),
            sql: "select 2".into(),
            refresh_secs: 0,
            ..Default::default()
        });
        let _empty = d.add_item(DashboardItem {
            title: "c".into(),
            ..Default::default()
        });
        assert_eq!((a, b), (1, 2));
        let mut s = CustomDashboardState::default();
        assert_eq!(s.take_due(&d, 0.0), vec![a, b]);
        assert!(
            s.take_due(&d, 1.0).is_empty(),
            "pending items are not requested twice"
        );
        let item_a = d.item(a).unwrap().clone();
        s.set_result(
            &DashboardItem {
                chart: ChartConfig {
                    kind: ChartKind::Bar,
                    x_column: Some(0),
                    y_columns: vec![1],
                    ..Default::default()
                },
                ..item_a
            },
            vec![
                ColumnMeta::new("k", "text", DataKind::Text),
                ColumnMeta::new("v", "int", DataKind::Int),
            ],
            vec![vec![Value::text("x"), Value::Int(3)]],
            2.0,
        );
        s.set_error(b, "boom", 2.0);
        assert!(s.result(a).unwrap().data.error.is_none());
        assert_eq!(s.result(b).unwrap().error.as_deref(), Some("boom"));
        assert_eq!(s.next_due_in(&d, 5.0), Some(5.0));
        assert!(s.take_due(&d, 9.0).is_empty());
        assert_eq!(
            s.take_due(&d, 10.0),
            vec![a],
            "manual items are not refreshed automatically"
        );
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<CustomDashboard>(&json).unwrap(), d);
    }
}
