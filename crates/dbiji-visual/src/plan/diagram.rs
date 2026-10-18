//! Execution plan diagram: a tree of node boxes coloured by their share of
//! the time (or cost), edges as thick as the number of rows flowing through
//! them, a details panel for the selected node, pan/zoom and fit.

use super::analysis::{
    FlatPlanNode, PlanMetric, PlanNodePath, flatten_plan, plan_summary, summary_line,
};
use crate::common::colors;
use crate::common::format::{compact, duration_ms, grouped};
use crate::common::paint::{self, font_px};
use crate::common::text::text_width;
use crate::common::view::{ViewTransform, handle_pan_zoom};
use dbiji_core::plan::QueryPlan;
use dbiji_core::{tr, trf};
use egui::{
    Align2, CornerRadius, CursorIcon, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Direction of the plan tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanOrientation {
    /// Root at the top, children below.
    #[default]
    TopDown,
    /// Root on the left, children to the right.
    LeftRight,
}

/// View state shared by [`show_plan_diagram`] and [`super::show_plan_tree`]
/// (selection is synchronised between both views).
#[derive(Debug, Clone)]
pub struct PlanViewState {
    /// Zoom factor of the diagram.
    pub zoom: f32,
    /// Pan offset of the diagram.
    pub pan: Vec2,
    /// Selected node.
    pub selected: Option<PlanNodePath>,
    /// Node under the pointer.
    pub hovered: Option<PlanNodePath>,
    /// Tree direction of the diagram.
    pub orientation: PlanOrientation,
    /// Measure used for colours and percentages.
    pub metric: PlanMetric,
    /// Collapsed nodes of the tree view.
    pub collapsed: BTreeSet<PlanNodePath>,
    /// Show the details panel of the selected node.
    pub show_details: bool,
    /// Show the toolbar above the diagram.
    pub show_toolbar: bool,
    /// Zoom to fit on the next frame.
    pub fit_pending: bool,
    last_canvas: Rect,
    last_orientation: PlanOrientation,
}

impl Default for PlanViewState {
    fn default() -> Self {
        PlanViewState {
            zoom: 1.0,
            pan: vec2(20.0, 20.0),
            selected: None,
            hovered: None,
            orientation: PlanOrientation::TopDown,
            metric: PlanMetric::Auto,
            collapsed: BTreeSet::new(),
            show_details: true,
            show_toolbar: true,
            fit_pending: true,
            last_canvas: Rect::NOTHING,
            last_orientation: PlanOrientation::TopDown,
        }
    }
}

impl PlanViewState {
    /// Resets selection and view for a new plan.
    pub fn reset(&mut self) {
        self.selected = None;
        self.hovered = None;
        self.collapsed.clear();
        self.fit_pending = true;
    }
}

/// Result of the plan views.
#[derive(Debug, Clone, Default)]
pub struct PlanResponse {
    /// The selection changed this frame.
    pub selection_changed: bool,
    /// Currently selected node.
    pub selected: Option<PlanNodePath>,
    /// Node under the pointer.
    pub hovered: Option<PlanNodePath>,
    /// Response of the canvas / table.
    pub response: Option<Response>,
}

// ---------------------------------------------------------------- layout

const TITLE_FONT: f32 = 13.0;
const LINE_FONT: f32 = 11.5;
const TITLE_H: f32 = 24.0;
const LINE_H: f32 = 16.0;
const PAD: f32 = 10.0;
const ACCENT_W: f32 = 5.0;
const MIN_W: f32 = 150.0;
const MAX_W: f32 = 320.0;
const GAP_BREADTH: f32 = 26.0;
const GAP_DEPTH: f32 = 44.0;

/// A positioned node box.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanBox {
    /// Index into the flat node list.
    pub flat: usize,
    pub rect: Rect,
    /// Detail lines below the title.
    pub lines: Vec<String>,
}

fn detail_lines(n: &FlatPlanNode<'_>) -> Vec<String> {
    let node = n.node;
    let mut lines = Vec::new();
    let mut on = String::new();
    if let Some(r) = &node.relation {
        on.push_str(r);
        if let Some(a) = node.alias.as_ref().filter(|a| *a != r) {
            on.push(' ');
            on.push_str(a);
        }
    }
    if let Some(i) = &node.index {
        if !on.is_empty() {
            on.push(' ');
        }
        on.push_str(&format!("({i})"));
    }
    if !on.is_empty() {
        lines.push(on);
    }
    match (node.startup_cost, node.total_cost) {
        (Some(s), Some(t)) => lines.push(trf("cost {}..{}", &[&compact(s, 2), &compact(t, 2)])),
        (None, Some(t)) => lines.push(trf("cost {}", &[&compact(t, 2)])),
        _ => {}
    }
    match (node.plan_rows, node.actual_rows) {
        (Some(p), Some(a)) => lines.push(trf(
            "rows {} est. / {} actual",
            &[&grouped(p.round()), &grouped(a.round())],
        )),
        (Some(p), None) => lines.push(trf("rows {} est.", &[&grouped(p.round())])),
        (None, Some(a)) => lines.push(trf("rows {} actual", &[&grouped(a.round())])),
        _ => {}
    }
    if let Some(t) = n.inclusive_ms {
        let loops = node.loops.unwrap_or(1.0);
        if loops > 1.0 {
            lines.push(trf(
                "{} total, {} loops",
                &[&duration_ms(t), &grouped(loops)],
            ));
        } else {
            lines.push(trf("{} total", &[&duration_ms(t)]));
        }
    }
    lines
}

fn box_size(n: &FlatPlanNode<'_>, lines: &[String]) -> Vec2 {
    let mut w =
        text_width(&n.node.node_type, TITLE_FONT, true) * 1.08 + 2.0 * PAD + ACCENT_W + 46.0;
    for l in lines {
        w = w.max(text_width(l, LINE_FONT, false) * 1.08 + 2.0 * PAD + ACCENT_W);
    }
    vec2(
        w.clamp(MIN_W, MAX_W).round(),
        TITLE_H + lines.len() as f32 * LINE_H + 6.0,
    )
}

/// Tidy tree layout: each subtree gets the breadth of its children (or its
/// own box), parents are centred over their children, depth levels are
/// spaced by their largest box.
pub(crate) fn layout_plan(flat: &[FlatPlanNode<'_>], orientation: PlanOrientation) -> Vec<PlanBox> {
    let n = flat.len();
    let lines: Vec<Vec<String>> = flat.iter().map(detail_lines).collect();
    let sizes: Vec<Vec2> = flat
        .iter()
        .zip(&lines)
        .map(|(f, l)| box_size(f, l))
        .collect();
    let horizontal = orientation == PlanOrientation::LeftRight;
    let breadth = |s: Vec2| if horizontal { s.y } else { s.x };
    let depth_size = |s: Vec2| if horizontal { s.x } else { s.y };

    // subtree breadth, post order (children come after parents in `flat`)
    let mut sub = vec![0.0f32; n];
    for i in (0..n).rev() {
        let kids: f32 = flat[i].children.iter().map(|&c| sub[c]).sum::<f32>()
            + GAP_BREADTH * flat[i].children.len().saturating_sub(1) as f32;
        sub[i] = breadth(sizes[i]).max(kids);
    }
    // depth offsets
    let max_depth = flat.iter().map(|f| f.depth).max().unwrap_or(0);
    let mut level = vec![0.0f32; max_depth + 2];
    for f in flat.iter().enumerate() {
        let (i, node) = f;
        level[node.depth + 1] = level[node.depth + 1].max(depth_size(sizes[i]));
    }
    let mut depth_off = vec![0.0f32; max_depth + 1];
    let mut acc = 0.0;
    for (d, off) in depth_off.iter_mut().enumerate() {
        *off = acc;
        acc += level[d + 1] + GAP_DEPTH;
    }
    // breadth positions: roots side by side
    let mut start = vec![0.0f32; n];
    let mut cursor = 0.0;
    for (i, f) in flat.iter().enumerate() {
        if f.parent.is_none() {
            start[i] = cursor;
            cursor += sub[i] + GAP_BREADTH * 2.0;
        }
    }
    let mut center = vec![0.0f32; n];
    for i in 0..n {
        let kids = &flat[i].children;
        let kids_w: f32 = kids.iter().map(|&c| sub[c]).sum::<f32>()
            + GAP_BREADTH * kids.len().saturating_sub(1) as f32;
        let mut x = start[i] + (sub[i] - kids_w) * 0.5;
        for &c in kids {
            start[c] = x;
            x += sub[c] + GAP_BREADTH;
        }
        center[i] = start[i] + sub[i] * 0.5;
    }
    (0..n)
        .map(|i| {
            let s = sizes[i];
            let b = center[i] - breadth(s) * 0.5;
            let d = depth_off.get(flat[i].depth).copied().unwrap_or(0.0);
            let min = if horizontal { pos2(d, b) } else { pos2(b, d) };
            PlanBox {
                flat: i,
                rect: Rect::from_min_size(min, s),
                lines: lines[i].clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- view

/// Shows the plan as a diagram. When a node is selected and
/// `state.show_details` is set, a details panel lists all its properties.
pub fn show_plan_diagram(ui: &mut Ui, plan: &QueryPlan, state: &mut PlanViewState) -> PlanResponse {
    let id = ui.id().with("dbiji_plan_diagram");
    let flat = flatten_plan(plan);
    let metric = state.metric.resolve(plan);
    let before = state.selected.clone();
    if state.show_toolbar {
        plan_toolbar(ui, plan, state);
    }
    if state.orientation != state.last_orientation {
        state.last_orientation = state.orientation;
        state.fit_pending = true;
    }
    let boxes = layout_plan(&flat, state.orientation);

    if state.show_details
        && let Some(sel) = state.selected.clone()
        && let Some(f) = flat.iter().find(|f| f.path == sel)
    {
        egui::Panel::right(id.with("details"))
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                details_panel(ui, f, metric, state);
            });
    }

    let rect = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(rect.min, rect.size().max(vec2(64.0, 64.0)));
    let response = ui.interact(rect, id.with("canvas"), Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);
    let stable = state.last_canvas.size() == rect.size();
    state.last_canvas = rect;
    let origin = rect.min;
    let painter = ui.painter_at(rect);
    let v = ui.visuals().clone();
    painter.rect_filled(rect, 0.0, v.extreme_bg_color);

    let bounds = boxes.iter().map(|b| b.rect).reduce(|a, b| a.union(b));
    if state.fit_pending {
        if !stable {
            ui.ctx().request_repaint();
            return PlanResponse {
                selection_changed: false,
                selected: state.selected.clone(),
                hovered: None,
                response: Some(response),
            };
        }
        let mut view = ViewTransform::new(state.zoom, state.pan);
        match bounds {
            Some(b) => view.fit(rect.size(), b, 24.0, 0.1, 1.0),
            None => view = ViewTransform::default(),
        }
        state.zoom = view.zoom;
        state.pan = view.pan;
        state.fit_pending = false;
    }
    let mut view = ViewTransform::new(state.zoom, state.pan);
    if handle_pan_zoom(ui, &response, &mut view, origin, (0.05, 3.0), true).changed {
        state.zoom = view.zoom;
        state.pan = view.pan;
    }

    // hover / select
    let pointer = ui
        .input(|i| i.pointer.latest_pos())
        .filter(|p| rect.contains(*p) && response.contains_pointer());
    let hovered = pointer.and_then(|p| {
        let w = view.to_world(origin, p);
        boxes
            .iter()
            .rev()
            .find(|b| b.rect.contains(w))
            .map(|b| b.flat)
    });
    state.hovered = hovered.and_then(|i| flat.get(i)).map(|f| f.path.clone());
    if response.clicked() {
        state.selected = state.hovered.clone();
    }
    if response.double_clicked()
        && let Some(b) = hovered.and_then(|i| boxes.iter().find(|b| b.flat == i))
    {
        let mut v2 = ViewTransform::new(state.zoom.max(1.0), state.pan);
        v2.pan = rect.size() * 0.5 - b.rect.center().to_vec2() * v2.zoom;
        state.zoom = v2.zoom;
        state.pan = v2.pan;
    }
    if hovered.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    // paint
    let view = ViewTransform::new(state.zoom, state.pan);
    let visible = view.rect_to_world(origin, rect).expand(40.0);
    let max_rows = flat
        .iter()
        .filter_map(|f| f.output_rows())
        .fold(1.0f64, f64::max);
    let line_color = v.text_color().gamma_multiply(0.45);
    for b in &boxes {
        let Some(f) = flat.get(b.flat) else { continue };
        let Some(p) = f.parent.and_then(|p| boxes.iter().find(|pb| pb.flat == p)) else {
            continue;
        };
        let rows = f.output_rows().unwrap_or(1.0).max(0.0);
        let t = ((rows + 1.0).log10() / (max_rows + 1.0).log10().max(1e-6)).clamp(0.0, 1.0) as f32;
        let width = (1.0 + 7.0 * t) * state.zoom.sqrt().clamp(0.5, 1.5);
        let (a, z) = if state.orientation == PlanOrientation::TopDown {
            (
                pos2(p.rect.center().x, p.rect.bottom()),
                pos2(b.rect.center().x, b.rect.top()),
            )
        } else {
            (
                pos2(p.rect.right(), p.rect.center().y),
                pos2(b.rect.left(), b.rect.center().y),
            )
        };
        let edge = Rect::from_two_pos(a, z);
        if !edge.intersects(visible) {
            continue;
        }
        let pts = if state.orientation == PlanOrientation::TopDown {
            let mid = (a.y + z.y) * 0.5;
            vec![a, pos2(a.x, mid), pos2(z.x, mid), z]
        } else {
            let mid = (a.x + z.x) * 0.5;
            vec![a, pos2(mid, a.y), pos2(mid, z.y), z]
        };
        let screen: Vec<Pos2> = pts.iter().map(|p| view.to_screen(origin, *p)).collect();
        painter.add(egui::Shape::line(screen, Stroke::new(width, line_color)));
        if state.zoom >= 0.5
            && let Some(px) = font_px(10.5, state.zoom)
        {
            let label_pos = view.to_screen(origin, z)
                + if state.orientation == PlanOrientation::TopDown {
                    vec2(6.0, -10.0)
                } else {
                    vec2(-6.0, -10.0)
                };
            let align = if state.orientation == PlanOrientation::TopDown {
                Align2::LEFT_CENTER
            } else {
                Align2::RIGHT_CENTER
            };
            paint::text(
                &painter,
                label_pos,
                align,
                &compact(rows, 1),
                px,
                v.weak_text_color(),
                false,
                f32::INFINITY,
            );
        }
    }
    for b in &boxes {
        if !b.rect.intersects(visible) {
            continue;
        }
        let Some(f) = flat.get(b.flat) else { continue };
        let srect = view.rect_to_screen(origin, b.rect);
        let selected = state.selected.as_ref() == Some(&f.path);
        let hover = state.hovered.as_ref() == Some(&f.path);
        paint_box(
            &painter, &v, f, b, srect, state.zoom, metric, selected, hover,
        );
    }
    if boxes.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            tr("No plan"),
            egui::FontId::proportional(14.0),
            v.weak_text_color(),
        );
    }

    if let Some(h) = hovered.and_then(|i| flat.get(i)) {
        let r = response.clone();
        r.on_hover_ui_at_pointer(|ui| node_tooltip(ui, h, metric));
    }
    PlanResponse {
        selection_changed: state.selected != before,
        selected: state.selected.clone(),
        hovered: state.hovered.clone(),
        response: Some(response),
    }
}

fn plan_toolbar(ui: &mut Ui, plan: &QueryPlan, state: &mut PlanViewState) {
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut state.orientation,
            PlanOrientation::TopDown,
            icon::ARROW_DOWN,
        )
        .on_hover_text(tr("Top down"));
        ui.selectable_value(
            &mut state.orientation,
            PlanOrientation::LeftRight,
            icon::ARROW_RIGHT,
        )
        .on_hover_text(tr("Left to right"));
        ui.separator();
        ui.label(tr("Color by"));
        egui::ComboBox::from_id_salt("dbiji_plan_metric")
            .selected_text(tr(state.metric.label()))
            .show_ui(ui, |ui| {
                for m in [PlanMetric::Auto, PlanMetric::Time, PlanMetric::Cost] {
                    ui.selectable_value(&mut state.metric, m, tr(m.label()));
                }
            });
        if ui
            .button(icon::CORNERS_OUT)
            .on_hover_text(tr("Zoom to fit"))
            .clicked()
        {
            state.fit_pending = true;
        }
        ui.toggle_value(&mut state.show_details, icon::SIDEBAR_SIMPLE)
            .on_hover_text(tr("Show node details"));
        ui.separator();
        ui.weak(summary_line(&plan_summary(plan)));
    });
}

#[allow(clippy::too_many_arguments)]
fn paint_box(
    painter: &Painter,
    v: &egui::Visuals,
    f: &FlatPlanNode<'_>,
    b: &PlanBox,
    srect: Rect,
    zoom: f32,
    metric: PlanMetric,
    selected: bool,
    hover: bool,
) {
    let share = f.share(metric);
    let heat = colors::heat(share.map(|s| (s * 2.0) as f32).unwrap_or(0.0));
    let radius = (5.0 * zoom).clamp(0.0, 10.0);
    let fill = match share {
        Some(s) if s >= 0.02 => colors::tint(v, heat, if v.dark_mode { 0.16 } else { 0.12 }),
        _ => v.window_fill,
    };
    painter.rect_filled(srect, radius, fill);
    let r8 = radius.round() as u8;
    let accent = Rect::from_min_size(srect.min, vec2((ACCENT_W * zoom).max(2.0), srect.height()));
    painter.rect_filled(
        accent,
        CornerRadius {
            nw: r8,
            sw: r8,
            ne: 0,
            se: 0,
        },
        if share.is_some() {
            heat
        } else {
            v.weak_text_color()
        },
    );
    let stroke = if selected {
        Stroke::new(2.0, v.selection.stroke.color)
    } else if hover {
        Stroke::new(1.5, v.selection.stroke.color.gamma_multiply(0.7))
    } else {
        v.widgets.noninteractive.bg_stroke
    };
    painter.rect_stroke(srect, radius, stroke, StrokeKind::Inside);

    let left = srect.left() + (PAD + ACCENT_W) * zoom;
    if let Some(px) = font_px(TITLE_FONT, zoom) {
        let ty = srect.top() + TITLE_H * 0.5 * zoom;
        let mut right = srect.right() - PAD * zoom;
        if let Some(s) = share {
            let label = format!("{:.0}%", s * 100.0);
            let galley =
                paint::single_line(painter, &label, (px - 1.5).max(5.0), false, f32::INFINITY);
            let pill = Rect::from_min_size(
                pos2(
                    right - galley.size().x - 10.0,
                    ty - galley.size().y * 0.5 - 1.0,
                ),
                galley.size() + vec2(10.0, 2.0),
            );
            painter.rect_filled(pill, pill.height() * 0.5, heat);
            painter.galley(pill.min + vec2(5.0, 1.0), galley, colors::text_on(heat));
            right = pill.left() - 4.0;
        }
        paint::text(
            painter,
            pos2(left, ty),
            Align2::LEFT_CENTER,
            &f.node.node_type,
            px,
            v.strong_text_color(),
            false,
            (right - left).max(10.0),
        );
    }
    if let Some(px) = font_px(LINE_FONT, zoom) {
        for (i, line) in b.lines.iter().enumerate() {
            let y = srect.top() + (TITLE_H + LINE_H * (i as f32 + 0.5)) * zoom;
            paint::text(
                painter,
                pos2(left, y),
                Align2::LEFT_CENTER,
                line,
                px,
                if i == 0 && f.node.relation.is_some() {
                    v.text_color()
                } else {
                    v.weak_text_color()
                },
                false,
                (srect.right() - PAD * zoom - left).max(10.0),
            );
        }
    }
}

fn metric_lines(ui: &mut Ui, f: &FlatPlanNode<'_>, metric: PlanMetric) {
    if let Some(e) = f.exclusive_ms {
        ui.label(trf("Exclusive time: {}", &[&duration_ms(e)]));
    }
    if let Some(c) = f.exclusive_cost {
        ui.label(trf("Exclusive cost: {}", &[&compact(c, 2)]));
    }
    if let Some(s) = f.share(metric) {
        ui.label(trf("Share of plan: {}%", &[&format!("{:.1}", s * 100.0)]));
    }
    if let Some(rf) = f.row_factor
        && !(0.1..=10.0).contains(&rf)
    {
        let (dir, k) = if rf > 1.0 {
            (tr("under-estimated"), rf)
        } else {
            (tr("over-estimated"), 1.0 / rf)
        };
        ui.colored_label(
            ui.visuals().warn_fg_color,
            trf("Rows {} by {}x", &[&dir, &format!("{k:.0}")]),
        );
    }
}

fn node_tooltip(ui: &mut Ui, f: &FlatPlanNode<'_>, metric: PlanMetric) {
    ui.label(egui::RichText::new(f.node.title()).strong());
    if let Some(c) = &f.node.condition {
        ui.label(egui::RichText::new(c.as_str()).monospace());
    }
    metric_lines(ui, f, metric);
}

fn details_panel(ui: &mut Ui, f: &FlatPlanNode<'_>, metric: PlanMetric, state: &mut PlanViewState) {
    ui.horizontal(|ui| {
        ui.strong(f.node.node_type.as_str());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(icon::X)
                .on_hover_text(tr("Close"))
                .clicked()
            {
                state.selected = None;
            }
        });
    });
    ui.separator();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            metric_lines(ui, f, metric);
            ui.add_space(4.0);
            let node = f.node;
            let mut rows: Vec<(String, String)> = Vec::new();
            let mut push = |k: &str, v: Option<String>| {
                if let Some(v) = v {
                    rows.push((tr(k).to_string(), v));
                }
            };
            push("Relation", node.relation.clone());
            push("Alias", node.alias.clone());
            push("Index", node.index.clone());
            push("Condition", node.condition.clone());
            push("Startup cost", node.startup_cost.map(|c| format!("{c:.2}")));
            push("Total cost", node.total_cost.map(|c| format!("{c:.2}")));
            push("Plan rows", node.plan_rows.map(grouped));
            push("Plan width", node.plan_width.map(grouped));
            push("Actual rows", node.actual_rows.map(grouped));
            push(
                "Actual startup time",
                node.actual_startup_ms.map(duration_ms),
            );
            push("Actual total time", node.actual_total_ms.map(duration_ms));
            push("Loops", node.loops.map(grouped));
            for (k, v) in &node.properties {
                rows.push((k.clone(), v.clone()));
            }
            egui::Grid::new("dbiji_plan_props")
                .num_columns(2)
                .striped(true)
                .spacing(vec2(12.0, 4.0))
                .show(ui, |ui| {
                    for (k, v) in rows {
                        ui.label(egui::RichText::new(k).color(ui.visuals().weak_text_color()));
                        ui.add(egui::Label::new(v).wrap());
                        ui.end_row();
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::analysis::tests::sample_plan;

    #[test]
    fn layout_has_no_overlaps_and_parents_above_children() {
        let plan = sample_plan();
        let flat = flatten_plan(&plan);
        for orientation in [PlanOrientation::TopDown, PlanOrientation::LeftRight] {
            let boxes = layout_plan(&flat, orientation);
            assert_eq!(boxes.len(), flat.len());
            for i in 0..boxes.len() {
                for j in i + 1..boxes.len() {
                    assert!(
                        !boxes[i].rect.intersects(boxes[j].rect),
                        "{orientation:?}: {:?} vs {:?}",
                        boxes[i],
                        boxes[j]
                    );
                }
            }
            for b in &boxes {
                if let Some(p) = flat[b.flat].parent {
                    let pr = boxes.iter().find(|x| x.flat == p).unwrap().rect;
                    match orientation {
                        PlanOrientation::TopDown => assert!(pr.bottom() < b.rect.top()),
                        PlanOrientation::LeftRight => assert!(pr.right() < b.rect.left()),
                    }
                }
            }
            // deterministic
            assert_eq!(layout_plan(&flat, orientation), boxes);
        }
    }

    #[test]
    fn wide_plan_layout() {
        // root with many children with subtrees of different sizes
        use dbiji_core::plan::PlanNode;
        let mut root = PlanNode::new("Append");
        for i in 0..12 {
            let mut c = PlanNode::new("Seq Scan");
            c.relation = Some(format!("part_{i}"));
            if i % 3 == 0 {
                c.children.push(PlanNode::new("Materialize"));
                c.children.push(PlanNode::new("Index Only Scan"));
            }
            root.children.push(c);
        }
        let plan = QueryPlan {
            roots: vec![root, PlanNode::new("Result")],
            ..Default::default()
        };
        let flat = flatten_plan(&plan);
        let boxes = layout_plan(&flat, PlanOrientation::TopDown);
        for i in 0..boxes.len() {
            for j in i + 1..boxes.len() {
                assert!(!boxes[i].rect.intersects(boxes[j].rect));
            }
        }
        assert!(
            layout_plan(
                &flatten_plan(&QueryPlan::default()),
                PlanOrientation::TopDown
            )
            .is_empty()
        );
    }

    #[test]
    fn diagram_and_tree_selection() {
        use crate::test_harness::Harness;
        let plan = sample_plan();
        let mut state = PlanViewState::default();
        let mut h = Harness::new(1200.0, 800.0);
        let mut last = PlanResponse::default();
        let mut ui_fn = |ui: &mut Ui| last = show_plan_diagram(ui, &plan, &mut state);
        h.idle(3, &mut ui_fn);
        drop(ui_fn);
        assert!(!state.fit_pending);
        // click the box of the index scan
        let flat = flatten_plan(&plan);
        let boxes = layout_plan(&flat, state.orientation);
        let target = boxes
            .iter()
            .find(|b| flat[b.flat].path == vec![0, 0, 1])
            .unwrap();
        let p = ViewTransform::new(state.zoom, state.pan)
            .to_screen(state.last_canvas.min, target.rect.center());
        let mut changed = false;
        let mut ui_fn = |ui: &mut Ui| {
            let r = show_plan_diagram(ui, &plan, &mut state);
            changed |= r.selection_changed;
        };
        h.click(p, &mut ui_fn);
        h.idle(2, &mut ui_fn); // details panel shown
        drop(ui_fn);
        assert!(changed);
        assert_eq!(state.selected, Some(vec![0, 0, 1]));
        // orientation change refits, tree / text views render with the selection
        state.orientation = PlanOrientation::LeftRight;
        state.collapsed.insert(vec![0, 0]);
        let mut ui_fn = |ui: &mut Ui| {
            show_plan_diagram(ui, &plan, &mut state);
            super::super::tree::show_plan_tree(ui, &plan, &mut state);
            super::super::tree::show_plan_text(ui, &plan);
        };
        h.idle(4, &mut ui_fn);
        drop(ui_fn);
        let empty = QueryPlan::default();
        let mut s2 = PlanViewState::default();
        let mut ui_fn = |ui: &mut Ui| {
            show_plan_diagram(ui, &empty, &mut s2);
        };
        h.idle(3, &mut ui_fn);
        let _ = last;
    }
}
