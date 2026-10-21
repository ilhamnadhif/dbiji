//! Building blocks of the side panels: cards, stat tiles, key/value rows.

use dbiji_core::tr;
use egui::{Color32, CornerRadius, Frame, Margin, RichText, Sense, Stroke, Ui};

/// Background of a card: the data area's colour, a step away from the panel.
pub(crate) fn card_fill(ui: &Ui) -> Color32 {
    ui.visuals().extreme_bg_color
}

fn card_frame(ui: &Ui) -> Frame {
    Frame::new()
        .fill(card_fill(ui))
        .stroke(Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
}

/// A rounded card filling the width.
pub(crate) fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame(ui)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Small upper-case title above a group.
pub(crate) fn section_title(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text.to_uppercase()).small().strong().color(ui.visuals().weak_text_color()));
    ui.add_space(2.0);
}

/// A row of equally wide tiles: a big value over its label. A click copies the value.
pub(crate) fn stat_tiles(ui: &mut Ui, items: &[(&str, String)]) {
    if items.is_empty() {
        return;
    }
    let gap = 6.0;
    let n = items.len() as f32;
    let width = ((ui.available_width() - gap * (n - 1.0)) / n).max(40.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (label, value) in items {
            let r = card_frame(ui)
                .inner_margin(Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(width - 18.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add(egui::Label::new(RichText::new(value).monospace().size(ui.text_style_height(&egui::TextStyle::Body) + 3.0).strong()).truncate());
                        ui.label(RichText::new(*label).small().color(ui.visuals().weak_text_color()));
                    });
                })
                .response
                .interact(Sense::click());
            if r.on_hover_text(format!("{value}\n{}", tr("Click to copy"))).clicked() {
                ui.ctx().copy_text(value.clone());
            }
        }
    });
}

/// A full-width row: the label on the left, the value (monospace) on the right.
/// Hovering highlights it; a click copies the value.
pub(crate) fn kv_row(ui: &mut Ui, label: &str, value: &str) {
    let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(4), ui.visuals().widgets.hovered.weak_bg_fill.gamma_multiply(0.6));
    }
    let v = ui.visuals();
    let mid = rect.center().y;
    let label_font = egui::TextStyle::Body.resolve(ui.style());
    let value_font = egui::TextStyle::Monospace.resolve(ui.style());
    let label_galley = ui.painter().layout_no_wrap(label.to_string(), label_font, v.weak_text_color());
    let max_value = (rect.width() - label_galley.size().x - 24.0).max(20.0);
    let shown: String = if value.chars().count() > 120 { value.chars().take(120).collect::<String>() + "…" } else { value.to_string() };
    let mut job = egui::text::LayoutJob::simple_singleline(shown, value_font, v.strong_text_color());
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_value);
    let value_galley = ui.painter().layout_job(job);
    ui.painter().galley(egui::pos2(rect.left() + 6.0, mid - label_galley.size().y / 2.0), label_galley, v.weak_text_color());
    ui.painter().galley(egui::pos2(rect.right() - 6.0 - value_galley.size().x, mid - value_galley.size().y / 2.0), value_galley, v.strong_text_color());
    if resp.on_hover_text(format!("{value}\n{}", tr("Click to copy"))).clicked() {
        ui.ctx().copy_text(value.to_string());
    }
}
