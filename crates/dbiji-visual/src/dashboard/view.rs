//! Server health dashboard: a grid of small time series per metric group
//! with current values, pause/resume and window length controls.

use super::history::{DashboardState, MetricHistory};
use crate::common::colors;
use crate::common::format::with_unit;
use dbiji_core::{tr, trf};
use egui::{Align2, FontId, Id, Pos2, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

/// Visible time window of the dashboard charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowLength {
    /// Last minute.
    OneMinute,
    /// Last five minutes.
    #[default]
    FiveMinutes,
    /// Last fifteen minutes.
    FifteenMinutes,
    /// Last hour.
    OneHour,
}

impl WindowLength {
    /// All windows in menu order.
    pub const ALL: [WindowLength; 4] = [
        WindowLength::OneMinute,
        WindowLength::FiveMinutes,
        WindowLength::FifteenMinutes,
        WindowLength::OneHour,
    ];

    /// Length in seconds.
    pub fn secs(self) -> f64 {
        match self {
            WindowLength::OneMinute => 60.0,
            WindowLength::FiveMinutes => 300.0,
            WindowLength::FifteenMinutes => 900.0,
            WindowLength::OneHour => 3600.0,
        }
    }

    /// Short label ("1 min").
    pub fn label(self) -> &'static str {
        match self {
            WindowLength::OneMinute => "1 min",
            WindowLength::FiveMinutes => "5 min",
            WindowLength::FifteenMinutes => "15 min",
            WindowLength::OneHour => "60 min",
        }
    }
}

/// How the dashboard is arranged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardLayout {
    /// Unique id (several dashboards can be open at once).
    pub id: String,
    /// Title shown above the charts.
    pub title: Option<String>,
    /// Charts per row (`None`: as many as fit, at least 240 points wide).
    pub columns: Option<usize>,
    /// Height of one chart.
    pub chart_height: f32,
    /// Groups to show, in this order (`None`: all, in first-seen order).
    pub groups: Option<Vec<String>>,
    /// Window used until the user picks another one.
    pub default_window: WindowLength,
}

impl Default for DashboardLayout {
    fn default() -> Self {
        DashboardLayout {
            id: "server".into(),
            title: None,
            columns: None,
            chart_height: 90.0,
            groups: None,
            default_window: WindowLength::FiveMinutes,
        }
    }
}

/// Controls of a dashboard (kept in egui memory, returned to the app).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DashboardResponse {
    /// The user paused the display; the app may stop polling.
    pub paused: bool,
    /// Selected window length in seconds.
    pub window_secs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Controls {
    paused_at: Option<f64>,
    window: WindowLength,
}

/// Shows the server health dashboard.
pub fn show_dashboard(
    ui: &mut Ui,
    state: &DashboardState,
    layout: &DashboardLayout,
) -> DashboardResponse {
    let id = Id::new(("dbiji_dashboard", layout.id.as_str()));
    let mut controls = ui.data(|d| d.get_temp::<Controls>(id)).unwrap_or(Controls {
        paused_at: None,
        window: layout.default_window,
    });
    let latest = state.latest_time();

    ui.horizontal(|ui| {
        if let Some(t) = &layout.title {
            ui.heading(t.as_str());
            ui.separator();
        }
        let paused = controls.paused_at.is_some();
        let label = if paused {
            format!("{} {}", icon::PLAY, tr("Resume"))
        } else {
            format!("{} {}", icon::PAUSE, tr("Pause"))
        };
        if ui.button(label).clicked() {
            controls.paused_at = if paused {
                None
            } else {
                Some(latest.unwrap_or(0.0))
            };
        }
        ui.separator();
        ui.label(tr("Window"));
        for w in WindowLength::ALL {
            ui.selectable_value(&mut controls.window, w, tr(w.label()));
        }
        if paused {
            ui.separator();
            ui.colored_label(ui.visuals().warn_fg_color, tr("Paused"));
        }
    });
    ui.data_mut(|d| d.insert_temp(id, controls));
    let response = DashboardResponse {
        paused: controls.paused_at.is_some(),
        window_secs: controls.window.secs(),
    };

    let Some(now) = controls.paused_at.or(latest) else {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.weak(tr("Waiting for the first metrics…")));
        return response;
    };
    let window = controls.window.secs();
    let groups = state.groups();
    let ordered: Vec<(String, Vec<&MetricHistory>)> = match &layout.groups {
        Some(order) => order
            .iter()
            .filter_map(|g| groups.iter().find(|(name, _)| name == g).cloned())
            .collect(),
        None => groups,
    };
    egui::ScrollArea::vertical()
        .id_salt(id.with("scroll"))
        .auto_shrink(false)
        .show(ui, |ui| {
            for (group, metrics) in ordered {
                ui.add_space(6.0);
                ui.strong(group.as_str());
                ui.add_space(2.0);
                let avail = ui.available_width();
                let cols = layout
                    .columns
                    .unwrap_or(((avail + 8.0) / 248.0).floor().max(1.0) as usize)
                    .max(1);
                let card_w = ((avail - 8.0 * (cols as f32 - 1.0)) / cols as f32).max(120.0);
                for row in metrics.chunks(cols) {
                    ui.horizontal(|ui| {
                        for m in row {
                            metric_card(ui, m, now, window, card_w, layout.chart_height);
                        }
                    });
                    ui.add_space(8.0);
                }
            }
        });
    response
}

fn metric_card(ui: &mut Ui, m: &MetricHistory, now: f64, window: f64, width: f32, chart_h: f32) {
    let v = ui.visuals().clone();
    let header_h = 40.0;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(width, header_h + chart_h + 18.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, v.faint_bg_color);
    painter.rect_stroke(
        rect,
        6.0,
        v.widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let rate = m.is_rate();
    painter.text(
        rect.min + vec2(10.0, 8.0),
        Align2::LEFT_TOP,
        m.label.as_str(),
        FontId::proportional(12.5),
        v.text_color(),
    );
    let current = m.window(now - window, now).last().map(|(_, v)| v);
    let current_text = current
        .map(|c| with_unit(c, &m.unit, rate))
        .unwrap_or_else(|| "–".into());
    painter.text(
        rect.right_top() + vec2(-10.0, 6.0),
        Align2::RIGHT_TOP,
        current_text,
        FontId::proportional(17.0),
        v.strong_text_color(),
    );

    let chart = Rect::from_min_max(
        pos2(rect.left() + 10.0, rect.top() + header_h),
        pos2(rect.right() - 10.0, rect.bottom() - 18.0),
    );
    let points: Vec<(f64, f64)> = m.window(now - window, now).collect();
    let color = colors::categorical(0, v.dark_mode);
    let stats = m.stats(now - window, now);
    let (lo, hi) = match stats {
        Some(s) => {
            let lo = if s.min >= 0.0 { 0.0 } else { s.min };
            let hi = if s.max <= lo { lo + 1.0 } else { s.max * 1.1 };
            (lo, hi)
        }
        None => (0.0, 1.0),
    };
    let sx = |t: f64| chart.left() + ((t - (now - window)) / window) as f32 * chart.width();
    let sy = |y: f64| chart.bottom() - ((y - lo) / (hi - lo)) as f32 * chart.height();
    // baseline and max hairlines
    painter.hline(
        chart.x_range(),
        chart.bottom(),
        Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color),
    );
    painter.hline(
        chart.x_range(),
        chart.top(),
        Stroke::new(
            1.0,
            v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.5),
        ),
    );
    painter.text(
        chart.left_top() + vec2(2.0, 1.0),
        Align2::LEFT_TOP,
        with_unit(hi, &m.unit, rate),
        FontId::proportional(9.5),
        v.weak_text_color(),
    );
    painter.text(
        rect.left_bottom() + vec2(10.0, -4.0),
        Align2::LEFT_BOTTOM,
        format!("-{}", short_duration(window)),
        FontId::proportional(9.5),
        v.weak_text_color(),
    );
    painter.text(
        rect.right_bottom() + vec2(-10.0, -4.0),
        Align2::RIGHT_BOTTOM,
        tr("now"),
        FontId::proportional(9.5),
        v.weak_text_color(),
    );
    if points.len() >= 2 {
        let line: Vec<Pos2> = points.iter().map(|&(t, y)| pos2(sx(t), sy(y))).collect();
        // area wash (~10 %) under the line
        let mut mesh = egui::Mesh::default();
        let fill = color.gamma_multiply(0.12);
        for p in &line {
            mesh.colored_vertex(*p, fill);
            mesh.colored_vertex(pos2(p.x, chart.bottom()), fill);
        }
        for k in 0..(line.len() as u32 - 1) {
            let (a, b, c, d) = (2 * k, 2 * k + 1, 2 * k + 2, 2 * k + 3);
            mesh.add_triangle(a, b, d);
            mesh.add_triangle(a, d, c);
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::line(line, Stroke::new(2.0, color)));
    } else if let Some(&(t, y)) = points.first() {
        painter.circle_filled(pos2(sx(t), sy(y)), 3.0, color);
    } else {
        painter.text(
            chart.center(),
            Align2::CENTER_CENTER,
            tr("no data"),
            FontId::proportional(11.0),
            v.weak_text_color(),
        );
    }
    // hover crosshair
    if let Some(p) = resp
        .hover_pos()
        .filter(|p| chart.expand2(vec2(0.0, 8.0)).contains(*p))
        && let Some(&(t, y)) = points
            .iter()
            .min_by(|a, b| (sx(a.0) - p.x).abs().total_cmp(&(sx(b.0) - p.x).abs()))
    {
        let x = sx(t);
        painter.vline(x, chart.y_range(), Stroke::new(1.0, v.weak_text_color()));
        painter.circle(
            pos2(x, sy(y)),
            4.0,
            color,
            Stroke::new(2.0, v.faint_bg_color),
        );
        let ago = now - t;
        let text = trf(
            "{} · {} ago",
            &[&with_unit(y, &m.unit, rate), &short_duration(ago)],
        );
        resp.clone().on_hover_ui_at_pointer(|ui| {
            ui.strong(m.label.as_str());
            ui.label(text);
            if let Some(s) = stats {
                ui.weak(trf(
                    "min {} · avg {} · max {}",
                    &[
                        &with_unit(s.min, &m.unit, rate),
                        &with_unit(s.avg, &m.unit, rate),
                        &with_unit(s.max, &m.unit, rate),
                    ],
                ));
            }
            if m.resets() > 0 {
                ui.weak(trf("{} counter resets", &[&m.resets()]));
            }
        });
    }
}

fn short_duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as i64;
    if s < 60 {
        trf("{}s", &[&s])
    } else if s < 3600 {
        if s % 60 == 0 {
            trf("{}m", &[&(s / 60)])
        } else {
            trf("{}m{}s", &[&(s / 60), &(s % 60)])
        }
    } else {
        trf("{}h{}m", &[&(s / 3600), &((s % 3600) / 60)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(short_duration(5.2), "5s");
        assert_eq!(short_duration(300.0), "5m");
        assert_eq!(short_duration(95.0), "1m35s");
        assert_eq!(short_duration(3700.0), "1h1m");
        assert_eq!(WindowLength::FifteenMinutes.secs(), 900.0);
    }

    #[test]
    fn dashboards_render() {
        use crate::chart::{ChartConfig, ChartKind};
        use crate::dashboard::{
            CustomDashboard, CustomDashboardEvent, CustomDashboardState, DashboardItem,
            show_custom_dashboard,
        };
        use crate::test_harness::Harness;
        use dbiji_core::admin::Metric;
        let mut state = DashboardState::new();
        let layout = DashboardLayout {
            title: Some("server".into()),
            ..Default::default()
        };
        let mut h = Harness::new(1000.0, 800.0);
        let mut resp = None;
        let mut ui_fn = |ui: &mut Ui| resp = Some(show_dashboard(ui, &state, &layout));
        h.idle(2, &mut ui_fn); // empty state
        drop(ui_fn);
        for t in 0..120 {
            state.push(
                t as f64,
                &[
                    Metric::gauge("Connections", "c", "Active", (t % 7) as f64, ""),
                    Metric::counter("I/O", "r", "Read", t as f64 * 4096.0, "bytes"),
                    Metric::gauge("I/O", "h", "Hit ratio", 97.5, "%"),
                ],
            );
        }
        let mut ui_fn = |ui: &mut Ui| resp = Some(show_dashboard(ui, &state, &layout));
        h.idle(2, &mut ui_fn);
        h.hover(egui::pos2(150.0, 120.0), &mut ui_fn);
        drop(ui_fn);
        let r = resp.expect("response");
        assert!(!r.paused);
        assert_eq!(r.window_secs, 300.0);

        let mut custom = CustomDashboard::default();
        custom.add_item(DashboardItem {
            title: "a".into(),
            sql: "select 1".into(),
            chart: ChartConfig {
                kind: ChartKind::Bar,
                x_column: Some(0),
                y_columns: vec![1],
                ..Default::default()
            },
            ..Default::default()
        });
        let mut cs = CustomDashboardState::default();
        let mut events = Vec::new();
        let mut ui_fn =
            |ui: &mut Ui| events.extend(show_custom_dashboard(ui, &mut custom, &mut cs, 1.0));
        h.idle(2, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(events, vec![CustomDashboardEvent::Refresh(1)]);
    }
}
