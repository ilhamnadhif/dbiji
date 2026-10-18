//! Painting helpers for zoomable canvases.

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, FontFamily, FontId, Galley, Painter, Pos2, Rect, Stroke};
use std::sync::Arc;

/// Smallest on-screen font size that is still drawn; smaller text is skipped
/// (level of detail).
pub(crate) const MIN_FONT_PX: f32 = 5.5;

/// On-screen font size for a world font size at `zoom`, quantised to half
/// points so that the galley cache stays warm while panning and during slow
/// zooms. `None` when too small to be legible.
pub(crate) fn font_px(world_size: f32, zoom: f32) -> Option<f32> {
    let px = world_size * zoom;
    if !px.is_finite() || px < MIN_FONT_PX {
        return None;
    }
    Some(((px * 2.0).round() / 2.0).min(96.0))
}

/// Lays out a single line of text, eliding it with `…` when it is wider than
/// `max_width` (pass `f32::INFINITY` for no limit). The galley uses the
/// placeholder color so the paint color can change without re-layout.
pub(crate) fn single_line(painter: &Painter, text: &str, size: f32, monospace: bool, max_width: f32) -> Arc<Galley> {
    let family = if monospace { FontFamily::Monospace } else { FontFamily::Proportional };
    let mut job = LayoutJob::simple_singleline(text.to_owned(), FontId::new(size, family), Color32::PLACEHOLDER);
    if max_width.is_finite() {
        job.wrap = TextWrapping::truncate_at_width(max_width.max(1.0));
    }
    painter.layout_job(job)
}

/// Paints `galley` anchored at `pos` with `align`.
pub(crate) fn paint_galley(painter: &Painter, pos: Pos2, align: Align2, galley: Arc<Galley>, color: Color32) -> Rect {
    let rect = align.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
    rect
}

/// Lays out and paints one line of text; returns the painted rectangle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn text(
    painter: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    size: f32,
    color: Color32,
    monospace: bool,
    max_width: f32,
) -> Rect {
    let galley = single_line(painter, text, size, monospace, max_width);
    paint_galley(painter, pos, align, galley, color)
}

/// Draws a polyline as dashes (`dash` and `gap` in screen points).
pub(crate) fn dashed_polyline(painter: &Painter, points: &[Pos2], stroke: Stroke, dash: f32, gap: f32) {
    if points.len() < 2 {
        return;
    }
    let mut shapes = Vec::new();
    egui::Shape::dashed_line_many(points, stroke, dash, gap, &mut shapes);
    painter.extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_quantisation() {
        assert_eq!(font_px(12.0, 1.0), Some(12.0));
        assert_eq!(font_px(12.0, 1.01), Some(12.0));
        assert_eq!(font_px(12.0, 1.03), Some(12.5));
        assert_eq!(font_px(12.0, 0.3), None);
        assert_eq!(font_px(12.0, f32::NAN), None);
    }
}
