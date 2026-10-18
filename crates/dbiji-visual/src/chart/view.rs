//! Chart rendering (egui_plot for line/bar/area/scatter, a custom painter
//! for pies and donuts) and the chart configuration editor.

use super::config::{ChartAggregate, ChartConfig, ChartKind};
use super::data::{ChartData, PieSlice, XKind};
use crate::common::colors;
use crate::common::format::{compact, grouped};
use dbiji_core::value::ColumnMeta;
use dbiji_core::{tr, trf};
use egui::{Align2, Color32, FontId, Mesh, Pos2, RichText, Sense, Shape, Stroke, Ui, Vec2, vec2};
use egui_plot::{Bar, BarChart, Corner, FilledArea, GridInput, GridMark, HoverPosition, Legend, Line, Plot, PlotPoints, Points};
use std::f32::consts::TAU;
use std::sync::Arc;

/// Interactive state of a chart.
#[derive(Debug, Clone, Default)]
pub struct ChartState {
    /// Reset zoom/pan on the next frame (e.g. after the data changed).
    pub reset_view: bool,
    /// Pie slice under the pointer.
    pub hovered_slice: Option<usize>,
}

/// Cheap clonable x formatter for 'static plot callbacks.
#[derive(Clone)]
struct XFormat {
    kind: XKind,
    categories: Arc<Vec<String>>,
    has_time: bool,
}

impl XFormat {
    fn new(d: &ChartData) -> Self {
        XFormat { kind: d.x_kind, categories: Arc::new(d.categories.clone()), has_time: d.has_time }
    }

    fn data(&self) -> ChartData {
        ChartData { x_kind: self.kind, categories: (*self.categories).clone(), has_time: self.has_time, ..Default::default() }
    }

    fn full(&self, x: f64) -> String {
        match self.kind {
            XKind::Categorical => {
                let i = x.round();
                if i >= 0.0 && (i - x).abs() < 0.3 { self.categories.get(i as usize).cloned().unwrap_or_default() } else { String::new() }
            }
            _ => self.data().format_x(x),
        }
    }
}

fn series_color(i: usize, is_other: bool, dark: bool) -> Color32 {
    if is_other { colors::other(dark) } else { colors::categorical(i, dark) }
}

/// Shows a chart of `data` drawn as configured.
pub fn show_chart(ui: &mut Ui, data: &ChartData, config: &ChartConfig, state: &mut ChartState) {
    if !config.title.is_empty() {
        ui.vertical_centered(|ui| ui.strong(config.title.as_str()));
    }
    if let Some(e) = &data.error {
        ui.centered_and_justified(|ui| ui.weak(e.as_str()));
        return;
    }
    if data.is_empty() {
        ui.centered_and_justified(|ui| ui.weak(tr("Nothing to plot")));
        return;
    }
    notes(ui, data);
    match config.kind {
        ChartKind::Pie => show_pie(ui, data, config, state),
        _ => show_plot(ui, data, config, state),
    }
}

fn notes(ui: &mut Ui, data: &ChartData) {
    let mut notes = Vec::new();
    if data.implicit_aggregate.is_some() {
        notes.push(tr("Repeated X values are summed").to_string());
    }
    if data.downsampled {
        notes.push(tr("Long series are down-sampled").to_string());
    }
    if data.folded_series > 0 {
        notes.push(trf("{} series folded into Other or hidden", &[&data.folded_series]));
    }
    if data.rows_skipped > 0 {
        notes.push(trf("{} rows without values skipped", &[&data.rows_skipped]));
    }
    if !notes.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(egui_phosphor::regular::INFO).color(ui.visuals().weak_text_color()));
            ui.weak(notes.join(" · "));
        });
    }
}

fn show_plot(ui: &mut Ui, data: &ChartData, config: &ChartConfig, state: &mut ChartState) {
    let dark = ui.visuals().dark_mode;
    let xf = XFormat::new(data);
    let id = ui.id().with("dbiji_chart_plot");
    let multi = data.series.len() > 1;
    let mut plot = Plot::new(id)
        .x_axis_label(data.x_label.clone())
        .y_axis_label(data.y_label.clone())
        .allow_scroll(false)
        .y_axis_formatter(|m: GridMark, _r: &std::ops::RangeInclusive<f64>| compact(m.value, 2));
    if multi {
        plot = plot.legend(Legend::default().position(Corner::LeftTop).background_alpha(0.85));
    }
    if state.reset_view {
        plot = plot.reset();
        state.reset_view = false;
    }
    {
        let xf = xf.clone();
        let span_data = data.x_range().map(|(a, b)| (b - a).abs()).unwrap_or(1.0);
        let d = data.clone_axis_info();
        plot = plot.x_axis_formatter(move |m: GridMark, r: &std::ops::RangeInclusive<f64>| {
            let span = (r.end() - r.start()).abs().max(span_data * 1e-6);
            match xf.kind {
                XKind::Categorical => xf.full(m.value),
                XKind::Index => {
                    if m.value.fract().abs() < 1e-9 && m.value >= 0.0 { format!("{}", m.value as i64 + 1) } else { String::new() }
                }
                _ => d.axis_label_x(m.value, span),
            }
        });
    }
    if data.x_kind == XKind::Categorical {
        let n = data.categories.len().max(1);
        plot = plot.x_grid_spacer(move |input: GridInput| {
            let (lo, hi) = input.bounds;
            let visible = ((hi - lo).max(1.0)).min(n as f64);
            let step = (visible / 12.0).ceil().max(1.0);
            let mut marks = Vec::new();
            let mut v = (lo.max(0.0) / step).ceil() * step;
            while v <= hi.min(n as f64 - 1.0) {
                marks.push(GridMark { value: v, step_size: step });
                v += step;
            }
            marks
        });
    }
    {
        let xf = xf.clone();
        plot = plot.label_formatter(move |pos: &HoverPosition<'_>| match pos {
            HoverPosition::NearDataPoint { plot_name, position, .. } => {
                let x = xf.full(position.x);
                Some(if plot_name.is_empty() {
                    format!("{x}\n{}", grouped(position.y))
                } else {
                    format!("{plot_name}\n{x}: {}", grouped(position.y))
                })
            }
            HoverPosition::Elsewhere { .. } => None,
        });
    }

    // bar width in plot units, capped at ~24 px per bar
    let n_series = data.series.len().max(1) as f64;
    let slot = bar_slot(data);
    let px_per_unit = {
        let range = data.x_range().map(|(a, b)| (b - a).abs() + slot).unwrap_or(1.0).max(slot);
        (ui.available_width() as f64 / range).max(1e-9)
    };
    let group_w = slot * 0.8;
    let bar_w = if config.stacked { group_w.min(28.0 / px_per_unit) } else { (group_w / n_series).min(24.0 / px_per_unit) };

    let stacked = config.stacked && config.kind.can_stack() && multi;
    let layers = if stacked { data.stacked() } else { Vec::new() };
    plot.show(ui, |plot_ui| {
        for (i, s) in data.series.iter().enumerate() {
            let color = series_color(i, s.is_other, dark);
            let name = s.name.clone();
            match config.kind {
                ChartKind::Line => {
                    plot_ui.line(Line::new(name.clone(), PlotPoints::new(s.points.clone())).color(color).width(2.0));
                    if s.points.len() <= 60 {
                        plot_ui.points(Points::new(name, PlotPoints::new(s.points.clone())).color(color).radius(4.0).filled(true));
                    }
                }
                ChartKind::Area => {
                    if let Some((xs, lower, upper)) = layers.get(i) {
                        plot_ui.add(FilledArea::new(name.clone(), xs, lower, upper).fill_color(color.gamma_multiply(0.35)).stroke(Stroke::NONE));
                        let pts: Vec<[f64; 2]> = xs.iter().zip(upper).map(|(x, y)| [*x, *y]).collect();
                        plot_ui.line(Line::new(name, PlotPoints::new(pts)).color(color).width(2.0));
                    } else {
                        plot_ui.line(Line::new(name, PlotPoints::new(s.points.clone())).color(color).width(2.0).fill(0.0).fill_alpha(0.12));
                    }
                }
                ChartKind::Scatter => {
                    plot_ui.points(Points::new(name, PlotPoints::new(s.points.clone())).color(color).radius(4.0).filled(true));
                }
                ChartKind::Bar | ChartKind::Pie => {
                    let bars: Vec<Bar> = if let Some((xs, lower, upper)) = layers.get(i) {
                        xs.iter()
                            .zip(lower.iter().zip(upper))
                            .filter(|(_, (l, u))| (*u - *l).abs() > 0.0)
                            .map(|(x, (l, u))| Bar::new(*x, u - l).base_offset(*l).width(bar_w).fill(color).stroke(Stroke::NONE))
                            .collect()
                    } else {
                        let offset = if multi { (i as f64 - (n_series - 1.0) * 0.5) * bar_w } else { 0.0 };
                        s.points.iter().map(|p| Bar::new(p[0] + offset, p[1]).width(bar_w * 0.92).fill(color).stroke(Stroke::NONE)).collect()
                    };
                    let xf = xf.clone();
                    let series_name = s.name.clone();
                    let offset = if multi && !stacked { (i as f64 - (n_series - 1.0) * 0.5) * bar_w } else { 0.0 };
                    let chart = BarChart::new(name, bars).color(color).element_formatter(Box::new(move |bar: &Bar, _c: &BarChart| {
                        format!("{series_name}\n{}: {}", xf.full(bar.argument - offset), grouped(bar.value))
                    }));
                    plot_ui.bar_chart(chart);
                }
            }
        }
    });
}

/// Width of one x slot: 1 for categories/rows, the smallest gap between x
/// values otherwise.
fn bar_slot(data: &ChartData) -> f64 {
    match data.x_kind {
        XKind::Categorical | XKind::Index => 1.0,
        _ => {
            let mut xs: Vec<f64> = data.series.iter().flat_map(|s| s.points.iter().map(|p| p[0])).collect();
            xs.sort_by(f64::total_cmp);
            xs.dedup();
            let min_gap = xs.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.0).fold(f64::INFINITY, f64::min);
            if min_gap.is_finite() { min_gap } else { 1.0 }
        }
    }
}

impl ChartData {
    /// Copy of the information needed to format axis labels.
    pub(crate) fn clone_axis_info(&self) -> ChartData {
        ChartData { x_kind: self.x_kind, categories: self.categories.clone(), has_time: self.has_time, ..Default::default() }
    }
}

// ---------------------------------------------------------------- pie

/// Geometry of a pie chart.
pub(crate) struct PieGeometry {
    pub center: Pos2,
    pub radius: f32,
    pub inner: f32,
    /// (start angle, end angle) per slice, radians clockwise from 12 o'clock.
    pub angles: Vec<(f32, f32)>,
}

pub(crate) fn pie_geometry(slices: &[PieSlice], center: Pos2, radius: f32, donut: bool) -> PieGeometry {
    let total: f64 = slices.iter().map(|s| s.value).sum();
    let mut angles = Vec::with_capacity(slices.len());
    let mut a = 0.0f32;
    for s in slices {
        let sweep = if total > 0.0 { (s.value / total) as f32 * TAU } else { 0.0 };
        angles.push((a, a + sweep));
        a += sweep;
    }
    PieGeometry { center, radius, inner: if donut { radius * 0.58 } else { 0.0 }, angles }
}

/// Point at `angle` (clockwise from 12 o'clock) and distance `r`.
pub(crate) fn polar(center: Pos2, angle: f32, r: f32) -> Pos2 {
    center + vec2(angle.sin(), -angle.cos()) * r
}

impl PieGeometry {
    /// Slice under a point.
    pub fn slice_at(&self, p: Pos2) -> Option<usize> {
        let d = p - self.center;
        let dist = d.length();
        if dist > self.radius || dist < self.inner {
            return None;
        }
        let mut angle = d.x.atan2(-d.y);
        if angle < 0.0 {
            angle += TAU;
        }
        self.angles.iter().position(|(a, b)| angle >= *a && angle < *b)
    }
}

fn show_pie(ui: &mut Ui, data: &ChartData, config: &ChartConfig, state: &mut ChartState) {
    let slices = data.pie_slices();
    if slices.is_empty() {
        ui.centered_and_justified(|ui| ui.weak(tr("No positive values to show")));
        return;
    }
    let total: f64 = slices.iter().map(|s| s.value).sum();
    let dark = ui.visuals().dark_mode;
    let avail = ui.available_size();
    let legend_w = (avail.x * 0.35).clamp(140.0, 280.0);
    let d = (avail.x - legend_w - 16.0).min(avail.y).clamp(120.0, 640.0);
    ui.horizontal_top(|ui| {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::hover());
        let geo = pie_geometry(&slices, rect.center(), d * 0.5 - 10.0, config.donut);
        state.hovered_slice = resp.hover_pos().and_then(|p| geo.slice_at(p));
        let painter = ui.painter_at(rect);
        let surface = ui.visuals().panel_fill;
        for (i, s) in slices.iter().enumerate() {
            let (a0, a1) = geo.angles[i];
            let color = series_color(i, s.is_other, dark);
            let hovered = state.hovered_slice == Some(i);
            let shift = if hovered { polar(Pos2::ZERO, (a0 + a1) * 0.5, 6.0).to_vec2() } else { Vec2::ZERO };
            painter.add(Shape::mesh(slice_mesh(geo.center + shift, geo.inner, geo.radius, a0, a1, color)));
        }
        // 2 px surface gaps between slices
        if slices.len() > 1 {
            for (a0, _) in &geo.angles {
                painter.line_segment([polar(geo.center, *a0, geo.inner), polar(geo.center, *a0, geo.radius + 8.0)], Stroke::new(2.0, surface));
            }
        }
        // labels inside large slices
        for (i, s) in slices.iter().enumerate() {
            let (a0, a1) = geo.angles[i];
            if a1 - a0 < 0.32 {
                continue;
            }
            let mid = (a0 + a1) * 0.5;
            let r = if geo.inner > 0.0 { (geo.inner + geo.radius) * 0.5 } else { geo.radius * 0.64 };
            let pct = s.value / total * 100.0;
            let fill = series_color(i, s.is_other, dark);
            painter.text(polar(geo.center, mid, r), Align2::CENTER_CENTER, format!("{pct:.0}%"), FontId::proportional(12.0), colors::text_on(fill));
        }
        if geo.inner > 0.0 {
            let v = ui.visuals();
            painter.text(geo.center - vec2(0.0, 8.0), Align2::CENTER_CENTER, compact(total, 2), FontId::proportional(20.0), v.strong_text_color());
            painter.text(geo.center + vec2(0.0, 12.0), Align2::CENTER_CENTER, tr("total"), FontId::proportional(11.0), v.weak_text_color());
        }
        if let Some(i) = state.hovered_slice
            && let Some(s) = slices.get(i) {
                resp.on_hover_ui_at_pointer(|ui| {
                    ui.strong(s.label.as_str());
                    ui.label(format!("{} ({:.1}%)", grouped(s.value), s.value / total * 100.0));
                });
            }
        ui.add_space(8.0);
        ui.vertical(|ui| {
            ui.set_max_width(legend_w);
            egui::ScrollArea::vertical().id_salt("dbiji_pie_legend").max_height(d).show(ui, |ui| {
                for (i, s) in slices.iter().enumerate() {
                    let color = series_color(i, s.is_other, dark);
                    let row = ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        ui.painter().rect_filled(r, 3.0, color);
                        let text = format!("{} · {} ({:.1}%)", s.label, compact(s.value, 2), s.value / total * 100.0);
                        let mut rt = RichText::new(text);
                        if state.hovered_slice == Some(i) {
                            rt = rt.strong();
                        }
                        ui.label(rt);
                    });
                    if row.response.hovered() {
                        state.hovered_slice = Some(i);
                    }
                }
            });
        });
    });
}

/// Triangle mesh of a pie slice (or ring segment when `inner` > 0).
pub(crate) fn slice_mesh(center: Pos2, inner: f32, outer: f32, a0: f32, a1: f32, color: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    let steps = (((a1 - a0) / (TAU / 180.0)).ceil() as usize).clamp(1, 360);
    if inner <= 0.0 {
        mesh.colored_vertex(center, color);
        for k in 0..=steps {
            let a = a0 + (a1 - a0) * k as f32 / steps as f32;
            mesh.colored_vertex(polar(center, a, outer), color);
        }
        for k in 0..steps as u32 {
            mesh.add_triangle(0, k + 1, k + 2);
        }
    } else {
        for k in 0..=steps {
            let a = a0 + (a1 - a0) * k as f32 / steps as f32;
            mesh.colored_vertex(polar(center, a, inner), color);
            mesh.colored_vertex(polar(center, a, outer), color);
        }
        for k in 0..steps as u32 {
            let (i0, o0, i1, o1) = (2 * k, 2 * k + 1, 2 * k + 2, 2 * k + 3);
            mesh.add_triangle(i0, o0, o1);
            mesh.add_triangle(i0, o1, i1);
        }
    }
    mesh
}

// ---------------------------------------------------------------- editor

fn column_name(columns: &[ColumnMeta], i: Option<usize>, none: &str) -> String {
    match i.and_then(|i| columns.get(i)) {
        Some(c) => c.name.clone(),
        None => none.to_string(),
    }
}

/// Chart configuration editor (type, X, Y, series, aggregate, options).
/// Returns true when something changed.
pub fn chart_config_ui(ui: &mut Ui, columns: &[ColumnMeta], config: &mut ChartConfig) -> bool {
    let before = config.clone();
    ui.horizontal_wrapped(|ui| {
        for k in ChartKind::ALL {
            ui.selectable_value(&mut config.kind, k, format!("{} {}", k.icon(), tr(k.label())));
        }
        ui.separator();
        ui.label(tr("X"));
        egui::ComboBox::from_id_salt("dbiji_chart_x")
            .selected_text(column_name(columns, config.x_column, tr("(row number)")))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut config.x_column, None, tr("(row number)"));
                for (i, c) in columns.iter().enumerate() {
                    ui.selectable_value(&mut config.x_column, Some(i), c.name.as_str());
                }
            });
        ui.label(tr("Y"));
        let y_text = if config.y_columns.is_empty() {
            tr("(none)").to_string()
        } else {
            config.y_columns.iter().filter_map(|&i| columns.get(i)).map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
        };
        ui.menu_button(format!("{y_text} {}", egui_phosphor::regular::CARET_DOWN), |ui| {
            for (i, c) in columns.iter().enumerate() {
                let mut on = config.y_columns.contains(&i);
                let label = if c.kind.is_numeric() { RichText::new(c.name.as_str()) } else { RichText::new(c.name.as_str()).weak() };
                if ui.checkbox(&mut on, label).changed() {
                    if on {
                        config.y_columns.push(i);
                        config.y_columns.sort_unstable();
                    } else {
                        config.y_columns.retain(|&y| y != i);
                    }
                }
            }
        });
        ui.label(tr("Series"));
        egui::ComboBox::from_id_salt("dbiji_chart_series")
            .selected_text(column_name(columns, config.series_column, tr("(none)")))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut config.series_column, None, tr("(none)"));
                for (i, c) in columns.iter().enumerate() {
                    ui.selectable_value(&mut config.series_column, Some(i), c.name.as_str());
                }
            });
        ui.label(tr("Aggregate"));
        egui::ComboBox::from_id_salt("dbiji_chart_agg").selected_text(tr(config.aggregate.label())).show_ui(ui, |ui| {
            for a in ChartAggregate::ALL {
                ui.selectable_value(&mut config.aggregate, a, tr(a.label()));
            }
        });
        ui.checkbox(&mut config.sort_x, tr("Sort X"));
        if config.kind.can_stack() {
            ui.checkbox(&mut config.stacked, tr("Stacked"));
        }
        if config.kind == ChartKind::Pie {
            ui.checkbox(&mut config.donut, tr("Donut"));
        }
        ui.label(tr("Max points"));
        ui.add(egui::DragValue::new(&mut config.max_points).range(10..=1_000_000).speed(10.0));
    });
    *config != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pie_geometry_and_hit_testing() {
        let slices = vec![
            PieSlice { label: "a".into(), value: 3.0, is_other: false },
            PieSlice { label: "b".into(), value: 1.0, is_other: false },
        ];
        let g = pie_geometry(&slices, Pos2::new(100.0, 100.0), 50.0, true);
        assert!((g.angles[0].1 - TAU * 0.75).abs() < 1e-5);
        // 3 o'clock is inside slice a (0..270 degrees), 9 o'clock in slice b
        assert_eq!(g.slice_at(Pos2::new(140.0, 100.0)), Some(0));
        assert_eq!(g.slice_at(Pos2::new(60.0, 100.0)), Some(1));
        assert_eq!(g.slice_at(Pos2::new(100.0, 100.0)), None, "donut hole");
        assert_eq!(g.slice_at(Pos2::new(300.0, 100.0)), None);
        let m = slice_mesh(Pos2::ZERO, 0.0, 10.0, 0.0, TAU / 4.0, Color32::RED);
        assert!(m.is_valid());
        let ring = slice_mesh(Pos2::ZERO, 5.0, 10.0, 0.0, TAU, Color32::RED);
        assert!(ring.is_valid());
        assert_eq!(ring.indices.len() % 3, 0);
    }

    #[test]
    fn every_chart_kind_renders() {
        use crate::chart::ChartAggregate;
        use crate::test_harness::Harness;
        use dbiji_core::value::{DataKind, Value};
        let columns = vec![
            ColumnMeta::new("day", "date", DataKind::Date),
            ColumnMeta::new("region", "text", DataKind::Text),
            ColumnMeta::new("v", "int", DataKind::Int),
        ];
        let rows: Vec<Vec<Value>> = (0..40)
            .map(|i| {
                vec![
                    Value::Date(chrono::NaiveDate::from_ymd_opt(2026, 1, 1 + (i % 20) as u32).unwrap()),
                    Value::text(["N", "S", "E"][i % 3]),
                    Value::Int(i as i64 * 3 - 20),
                ]
            })
            .collect();
        let mut h = Harness::new(900.0, 600.0);
        for kind in ChartKind::ALL {
            for (x, series, stacked) in [(Some(0), Some(1), false), (Some(1), None, false), (Some(0), Some(1), true), (None, None, false)] {
                let config = ChartConfig {
                    kind,
                    x_column: x,
                    y_columns: vec![2],
                    series_column: series,
                    stacked,
                    aggregate: ChartAggregate::Sum,
                    title: "t".into(),
                    ..Default::default()
                };
                let data = ChartData::build(&columns, &rows, &config);
                let mut state = ChartState { reset_view: true, ..Default::default() };
                let mut cfg = config.clone();
                let mut ui_fn = |ui: &mut Ui| {
                    chart_config_ui(ui, &columns, &mut cfg);
                    show_chart(ui, &data, &config, &mut state);
                };
                h.idle(2, &mut ui_fn);
                h.hover(egui::pos2(300.0, 300.0), &mut ui_fn);
            }
        }
        // error state renders a message
        let bad = ChartConfig::default();
        let data = ChartData::build(&columns, &rows, &bad);
        let mut state = ChartState::default();
        let mut ui_fn = |ui: &mut Ui| show_chart(ui, &data, &bad, &mut state);
        h.idle(2, &mut ui_fn);
    }
}
