//! Explain tree table (explain.depesz style): collapsible node tree with
//! cost, estimated and actual rows, misestimate factor, time, loops and the
//! exclusive share as a heat bar; the slowest nodes are flagged.

use super::analysis::{FlatPlanNode, PlanMetric, flatten_plan};
use super::diagram::{PlanResponse, PlanViewState};
use crate::common::colors;
use crate::common::format::{compact, duration_ms, grouped};
use dbiji_core::plan::QueryPlan;
use dbiji_core::{tr, trf};
use egui::{Align, Layout, RichText, Sense, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;
use std::collections::BTreeSet;

/// Number of slowest nodes that are flagged.
const HOT_NODES: usize = 3;

/// Shows the plan as a collapsible tree table. Selection is shared with
/// [`super::show_plan_diagram`] through `state`.
pub fn show_plan_tree(ui: &mut Ui, plan: &QueryPlan, state: &mut PlanViewState) -> PlanResponse {
    let flat = flatten_plan(plan);
    let metric = state.metric.resolve(plan);
    let before = state.selected.clone();
    let mut hottest: Vec<usize> = (0..flat.len()).filter(|&i| flat[i].share(metric).unwrap_or(0.0) > 0.0).collect();
    hottest.sort_by(|&a, &b| flat[b].share(metric).unwrap_or(0.0).total_cmp(&flat[a].share(metric).unwrap_or(0.0)));
    let hot: BTreeSet<usize> = hottest.into_iter().take(HOT_NODES).collect();

    // visible rows: skip descendants of collapsed nodes
    let mut visible = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for (i, f) in flat.iter().enumerate() {
        if let Some(d) = hidden_below {
            if f.depth > d {
                continue;
            }
            hidden_below = None;
        }
        visible.push(i);
        if state.collapsed.contains(&f.path) {
            hidden_below = Some(f.depth);
        }
    }

    ui.horizontal(|ui| {
        if ui.small_button(format!("{} {}", icon::CARET_DOWN, tr("Expand all"))).clicked() {
            state.collapsed.clear();
        }
        if ui.small_button(format!("{} {}", icon::CARET_RIGHT, tr("Collapse all"))).clicked() {
            state.collapsed = flat.iter().filter(|f| !f.children.is_empty()).map(|f| f.path.clone()).collect();
        }
        ui.separator();
        ui.label(tr("Color by"));
        egui::ComboBox::from_id_salt("dbiji_plan_tree_metric").selected_text(tr(state.metric.label())).show_ui(ui, |ui| {
            for m in [PlanMetric::Auto, PlanMetric::Time, PlanMetric::Cost] {
                ui.selectable_value(&mut state.metric, m, tr(m.label()));
            }
        });
    });
    ui.separator();

    let mut hovered = None;
    let mut toggles: Vec<Vec<usize>> = Vec::new();
    let mut clicked: Option<Vec<usize>> = None;
    let out = egui::ScrollArea::both().auto_shrink(false).id_salt("dbiji_plan_tree").show(ui, |ui| {
        egui::Grid::new("dbiji_plan_tree_grid").striped(true).num_columns(8).spacing(vec2(14.0, 3.0)).show(ui, |ui| {
            for h in ["Node", "Cost", "Rows (est.)", "Rows (actual)", "Rows ×", "Time", "Loops", "Exclusive"] {
                ui.label(RichText::new(tr(h)).strong());
            }
            ui.end_row();
            for &i in &visible {
                let Some(f) = flat.get(i) else { continue };
                let selected = state.selected.as_ref() == Some(&f.path);
                let row_resp = node_cell(ui, f, selected, hot.contains(&i), state.collapsed.contains(&f.path), &mut toggles);
                if row_resp.clicked() {
                    clicked = Some(f.path.clone());
                }
                if row_resp.hovered() {
                    hovered = Some(f.path.clone());
                }
                num(ui, f.node.total_cost.map(|c| compact(c, 2)));
                num(ui, f.node.plan_rows.map(|r| grouped(r.round())));
                num(ui, f.node.actual_rows.map(|r| grouped(r.round())));
                misestimate(ui, f);
                num(ui, f.inclusive_ms.map(duration_ms));
                num(ui, f.node.loops.map(grouped));
                share_bar(ui, f, metric, hot.contains(&i));
                ui.end_row();
            }
        })
    });
    for path in toggles {
        if !state.collapsed.remove(&path) {
            state.collapsed.insert(path);
        }
    }
    if let Some(p) = clicked {
        state.selected = if state.selected.as_ref() == Some(&p) { None } else { Some(p) };
    }
    if hovered.is_some() {
        state.hovered = hovered.clone();
    }
    PlanResponse {
        selection_changed: state.selected != before,
        selected: state.selected.clone(),
        hovered,
        response: Some(out.inner.response),
    }
}

fn node_cell(
    ui: &mut Ui,
    f: &FlatPlanNode<'_>,
    selected: bool,
    hot: bool,
    collapsed: bool,
    toggles: &mut Vec<Vec<usize>>,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_space(f.depth as f32 * 16.0);
        if f.children.is_empty() {
            ui.add_space(18.0);
        } else {
            let caret = if collapsed { icon::CARET_RIGHT } else { icon::CARET_DOWN };
            if ui.add(egui::Button::new(caret).frame(false).small()).clicked() {
                toggles.push(f.path.clone());
            }
        }
        if hot {
            ui.label(RichText::new(icon::FIRE).color(colors::STATUS_CRITICAL)).on_hover_text(tr("One of the slowest nodes"));
        }
        let mut title = RichText::new(f.node.title());
        if hot {
            title = title.strong();
        }
        let resp = ui.add(egui::Button::selectable(selected, title).frame_when_inactive(false));
        if let Some(c) = &f.node.condition {
            ui.label(RichText::new(c.as_str()).small().color(ui.visuals().weak_text_color()));
        }
        resp
    })
    .inner
}

fn num(ui: &mut Ui, v: Option<String>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match v {
        Some(s) => ui.monospace(s),
        None => ui.weak("–"),
    });
}

fn misestimate(ui: &mut Ui, f: &FlatPlanNode<'_>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match f.row_factor {
        Some(rf) => {
            let (arrow, k) = if rf >= 1.0 { (icon::ARROW_DOWN, rf) } else { (icon::ARROW_UP, 1.0 / rf) };
            let text = format!("{arrow}{}", compact(k, 1));
            if k >= 10.0 {
                ui.colored_label(ui.visuals().warn_fg_color, RichText::new(text).monospace()).on_hover_text(if rf >= 1.0 {
                    trf("{}x more rows than estimated", &[&compact(k, 1)])
                } else {
                    trf("{}x fewer rows than estimated", &[&compact(k, 1)])
                })
            } else {
                ui.weak(RichText::new(text).monospace())
            }
        }
        None => ui.weak("–"),
    });
}

fn share_bar(ui: &mut Ui, f: &FlatPlanNode<'_>, metric: PlanMetric, hot: bool) {
    let share = f.share(metric);
    let (rect, resp) = ui.allocate_exact_size(vec2(110.0, ui.spacing().interact_size.y * 0.8), Sense::hover());
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 3.0, v.faint_bg_color);
    if let Some(s) = share {
        let w = (rect.width() * s as f32).max(if s > 0.0 { 2.0 } else { 0.0 });
        let fill = colors::heat((s * 2.0) as f32);
        painter.rect_filled(egui::Rect::from_min_size(rect.min, vec2(w, rect.height())), 3.0, fill);
        let text_color = if w > rect.width() * 0.55 { colors::text_on(fill) } else { v.text_color() };
        painter.text(
            rect.right_center() - vec2(4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{:.1}%", s * 100.0),
            egui::FontId::monospace(11.0),
            text_color,
        );
        if hot {
            painter.rect_stroke(rect, 3.0, Stroke::new(1.0, colors::STATUS_CRITICAL), egui::StrokeKind::Inside);
        }
    } else {
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "–", egui::FontId::monospace(11.0), v.weak_text_color());
    }
    let _ = resp.on_hover_text(match (f.exclusive_ms, f.exclusive_cost) {
        (Some(ms), _) => trf("Exclusive time {}", &[&duration_ms(ms)]),
        (None, Some(c)) => trf("Exclusive cost {}", &[&compact(c, 2)]),
        _ => tr("No measurements").to_string(),
    });
}

/// Shows the textual plan (as printed by the database, or rendered from the
/// node tree when the server gave no text form) in a scrollable monospace view.
pub fn show_plan_text(ui: &mut Ui, plan: &QueryPlan) {
    let text = if plan.text.trim().is_empty() { plan.to_text_tree() } else { plan.text.clone() };
    ui.horizontal(|ui| {
        if ui.button(format!("{} {}", icon::COPY, tr("Copy"))).clicked() {
            ui.ctx().copy_text(text.clone());
        }
    });
    egui::ScrollArea::both().auto_shrink(false).id_salt("dbiji_plan_text").show(ui, |ui| {
        let mut t = text.as_str();
        ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
    });
}
