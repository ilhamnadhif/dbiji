//! Fast single-line cell text.
//!
//! Laying out fresh text with egui 0.36 runs a full shaper (harfrust) and
//! costs ~10 µs per cell, which is too slow when a whole page of new cells
//! scrolls in. Cells instead use a per-character glyph cache: every character
//! is laid out once by egui, and cell texts are composed from the cached glyph
//! quads into one mesh per frame (no shaping, cost bounded by the visible
//! width). Characters that need real shaping (combining marks, RTL and Indic
//! scripts, control characters ...) fall back to egui's normal layout.
//!
//! The composed mesh keeps texel UV coordinates inside a [`Galley`] so the
//! tessellator normalises them with the final font atlas size, and the cache
//! is dropped whenever egui recreates its fonts (detected with a canary galley).

use egui::epaint::text::{Galley, PlacedRow, RowVisuals};
use egui::epaint::{Mesh, Vertex};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Color32, FontId, Painter, Pos2, Rect, Shape, TextFormat, pos2};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
struct GlyphInfo {
    advance: f32,
    verts: Vec<Vertex>,
    indices: Vec<u32>,
}

/// Cache of single-character glyph meshes for one font.
#[derive(Debug, Clone, Default)]
pub(crate) struct GlyphCache {
    font: Option<FontId>,
    ppp: f32,
    canary: Option<Arc<Galley>>,
    template: Option<Arc<Galley>>,
    glyphs: HashMap<char, Option<GlyphInfo>>,
    row_height: f32,
}

/// True for characters whose rendering does not depend on neighbours.
fn is_simple_char(c: char) -> bool {
    let u = c as u32;
    match u {
        0x20..=0x7E => true,
        0xA0..=0x2FF => true,
        0x370..=0x58F => true, // Greek, Cyrillic, Armenian
        0x1E00..=0x1FFF => true,
        0x2010..=0x2027 | 0x2030..=0x205E => true,
        0x2070..=0x2BFF => true, // super/subscripts, currency, arrows, math, box drawing, shapes
        0x3000..=0x9FFF => true, // CJK
        0xAC00..=0xD7AF => true, // Hangul syllables
        0xE000..=0xF8FF => true, // private use (icon fonts)
        0xFF01..=0xFFEE => true, // fullwidth forms
        _ => false,
    }
}

/// Result of placing cell text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placed {
    /// The text was shortened with an ellipsis.
    pub elided: bool,
    /// Width of the drawn text.
    pub width: f32,
    /// Left edge of the drawn text.
    pub left: f32,
}

/// Glyph quads collected during a frame, painted as one shape.
#[derive(Debug, Default)]
pub(crate) struct TextBatch {
    mesh: Mesh,
    bounds: Option<Rect>,
}

impl TextBatch {
    pub fn new() -> Self {
        Self::default()
    }

    fn push_glyph(&mut self, g: &GlyphInfo, origin: Pos2, color: Color32) {
        if g.verts.is_empty() {
            return;
        }
        let base = self.mesh.vertices.len() as u32;
        let mut r = Rect::NOTHING;
        for v in &g.verts {
            let pos = origin + v.pos.to_vec2();
            r.extend_with(pos);
            self.mesh.vertices.push(Vertex { pos, uv: v.uv, color });
        }
        self.mesh.indices.extend(g.indices.iter().map(|i| base + i));
        self.bounds = Some(match self.bounds {
            Some(b) => b.union(r),
            None => r,
        });
    }

    /// Paint the collected text with `cache`'s template galley.
    pub fn paint(self, painter: &Painter, cache: &GlyphCache) {
        if let Some(shape) = self.into_shape(cache) {
            painter.add(shape);
        }
    }

    /// The collected text as one shape (`None` when empty).
    pub fn into_shape(self, cache: &GlyphCache) -> Option<Shape> {
        let (Some(template), Some(bounds)) = (&cache.template, self.bounds) else { return None };
        let first = template.rows.first()?;
        let mut row = (*first.row).clone();
        row.glyphs.clear();
        row.size = bounds.size();
        let n_verts = self.mesh.vertices.len();
        let n_idx = self.mesh.indices.len();
        row.visuals = RowVisuals { mesh: self.mesh, mesh_bounds: bounds, glyph_index_start: 0, glyph_vertex_range: 0..n_verts };
        let mut galley = (**template).clone();
        galley.rows = vec![PlacedRow { pos: Pos2::ZERO, row: Arc::new(row), ends_with_newline: false }];
        galley.elided = false;
        galley.rect = bounds;
        galley.mesh_bounds = bounds;
        galley.num_vertices = n_verts;
        galley.num_indices = n_idx;
        Some(Shape::galley(Pos2::ZERO, Arc::new(galley), Color32::WHITE))
    }
}

impl GlyphCache {
    /// Validate the cache for this frame (font, scale, egui font atlas).
    pub fn begin_frame(&mut self, painter: &Painter, font: &FontId) {
        let ppp = painter.pixels_per_point();
        let canary = painter.layout_no_wrap("M".to_string(), font.clone(), Color32::WHITE);
        let valid = self.font.as_ref() == Some(font) && self.ppp == ppp && self.canary.as_ref().is_some_and(|c| Arc::ptr_eq(c, &canary));
        if !valid {
            self.glyphs.clear();
            self.font = Some(font.clone());
            self.ppp = ppp;
            self.row_height = canary.rect.height();
            self.template = Some(canary.clone());
        }
        self.canary = Some(canary);
    }

    /// Height of one text line.
    pub fn row_height(&self) -> f32 {
        self.row_height
    }

    fn glyph(&mut self, painter: &Painter, c: char) -> Option<&GlyphInfo> {
        if !self.glyphs.contains_key(&c) {
            let info = if is_simple_char(c) { self.layout_glyph(painter, c) } else { None };
            self.glyphs.insert(c, info);
        }
        self.glyphs.get(&c).and_then(Option::as_ref)
    }

    fn layout_glyph(&self, painter: &Painter, c: char) -> Option<GlyphInfo> {
        let font = self.font.clone()?;
        let g = painter.layout_no_wrap(c.to_string(), font, Color32::WHITE);
        let row = g.rows.first()?;
        let glyph = row.glyphs.first()?;
        if row.glyphs.len() != 1 {
            return None;
        }
        let offset = row.pos.to_vec2();
        let verts = row.visuals.mesh.vertices.iter().map(|v| Vertex { pos: v.pos + offset, uv: v.uv, color: v.color }).collect();
        Some(GlyphInfo { advance: glyph.advance_width, verts, indices: row.visuals.mesh.indices.clone() })
    }

    /// Width of `text` if it can use the fast path.
    pub fn measure(&mut self, painter: &Painter, text: &str) -> Option<f32> {
        let mut w = 0.0;
        for c in text.chars() {
            w += self.glyph(painter, c)?.advance;
        }
        Some(w)
    }

    /// Place `text` in `rect` (one line, vertically centred, elided with `…`
    /// when too wide). Returns `None` when the text needs the slow path.
    pub fn place(&mut self, painter: &Painter, batch: &mut TextBatch, text: &str, rect: Rect, align: Align, color: Color32) -> Option<Placed> {
        let max_w = rect.width();
        if max_w <= 1.0 || text.is_empty() {
            return Some(Placed { elided: !text.is_empty(), width: 0.0, left: rect.left() });
        }
        let ellipsis_w = self.glyph(painter, '…').map_or(0.0, |g| g.advance);
        // Find how many chars fit.
        let mut width = 0.0f32;
        let mut fit_chars = 0usize; // chars that fit together with an ellipsis
        let mut fit_width = 0.0f32;
        let mut elided = false;
        for c in text.chars() {
            let adv = self.glyph(painter, c)?.advance;
            if width + adv > max_w + 0.01 {
                elided = true;
                break;
            }
            width += adv;
            if width + ellipsis_w <= max_w + 0.01 {
                fit_chars += 1;
                fit_width = width;
            }
        }
        let (draw_chars, draw_width) = if elided { (fit_chars, fit_width + ellipsis_w) } else { (usize::MAX, width) };
        let ppp = self.ppp.max(0.1);
        let snap = |x: f32| (x * ppp).round() / ppp;
        let left = match align {
            Align::Min => rect.left(),
            Align::Center => rect.center().x - draw_width / 2.0,
            Align::Max => rect.right() - draw_width,
        };
        let top = snap(rect.center().y - self.row_height / 2.0);
        let mut pen = left;
        for (i, c) in text.chars().enumerate() {
            if i >= draw_chars {
                break;
            }
            let Some(g) = self.glyphs.get(&c).and_then(Option::as_ref) else { break };
            batch.push_glyph(g, pos2(snap(pen), top), color);
            pen += g.advance;
        }
        if elided && let Some(g) = self.glyphs.get(&'…').and_then(Option::as_ref) {
            batch.push_glyph(g, pos2(snap(pen), top), color);
        }
        Some(Placed { elided, width: draw_width, left })
    }
}

/// Slow path: lay out with egui (elided to one line) and paint immediately.
pub(crate) fn place_slow(painter: &Painter, font: &FontId, text: &str, rect: Rect, align: Align, color: Color32) -> Placed {
    let (shape, placed) = layout_slow(painter, font, text, rect, align, color);
    painter.add(shape);
    placed
}

/// Slow path returning the shape instead of painting it.
pub(crate) fn layout_slow(painter: &Painter, font: &FontId, text: &str, rect: Rect, align: Align, color: Color32) -> (Shape, Placed) {
    // Bound the shaping work: no more characters than could possibly fit.
    let max_chars = ((rect.width() / (font.size * 0.2)).ceil() as usize).saturating_add(2);
    let shown: String = if text.chars().count() > max_chars { text.chars().take(max_chars).collect() } else { text.to_string() };
    let mut job = LayoutJob::single_section(shown, TextFormat::simple(font.clone(), color));
    job.wrap = TextWrapping { max_width: rect.width().max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    job.halign = align;
    let galley = painter.layout_job(job);
    let w = galley.rect.width();
    let x = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x,
        Align::Max => rect.right(),
    };
    let left = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x - w / 2.0,
        Align::Max => rect.right() - w,
    };
    let y = rect.center().y - galley.rect.height() / 2.0;
    let elided = galley.elided || shown_was_cut(text, max_chars);
    (Shape::galley(pos2(x, y), galley, color), Placed { elided, width: w, left })
}

fn shown_was_cut(text: &str, max_chars: usize) -> bool {
    text.chars().nth(max_chars).is_some()
}

/// Measure text with egui's layout (slow path).
pub(crate) fn measure_slow(painter: &Painter, font: &FontId, text: &str) -> f32 {
    let shown: String = text.chars().take(512).collect();
    painter.layout_no_wrap(shown, font.clone(), Color32::WHITE).rect.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_chars() {
        assert!(is_simple_char('a'));
        assert!(is_simple_char('é'));
        assert!(is_simple_char('Ж'));
        assert!(is_simple_char('…'));
        assert!(is_simple_char('中'));
        assert!(!is_simple_char('\u{0301}'));
        assert!(!is_simple_char('\t'));
        assert!(!is_simple_char('ש'));
        assert!(!is_simple_char('\u{200D}'));
    }

    #[test]
    fn fast_text_matches_egui_widths() {
        let ctx = egui::Context::default();
        let mut cache = GlyphCache::default();
        let font = FontId::proportional(13.0);
        let mut results = Vec::new();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            cache.begin_frame(&painter, &font);
            let text = "Hello World 12345";
            let fast = cache.measure(&painter, text).unwrap();
            let slow = measure_slow(&painter, &font, text);
            results.push((fast, slow));
            let mut batch = TextBatch::new();
            let rect = Rect::from_min_size(pos2(10.0, 10.0), egui::vec2(40.0, 20.0));
            let placed = cache.place(&painter, &mut batch, text, rect, Align::Min, Color32::WHITE).unwrap();
            assert!(placed.elided);
            assert!(placed.width <= 40.01, "{}", placed.width);
            let placed = cache.place(&painter, &mut batch, "ab", rect, Align::Max, Color32::WHITE).unwrap();
            assert!(!placed.elided);
            assert!((placed.left + placed.width - 50.0).abs() < 0.01);
            assert!(cache.place(&painter, &mut batch, "a\u{0301}", rect, Align::Min, Color32::WHITE).is_none());
            batch.paint(&painter, &cache);
        });
        out.textures_delta.clear();
        let (fast, slow) = results[0];
        // No kerning in the fast path; widths stay close.
        assert!((fast - slow).abs() / slow < 0.08, "fast {fast} slow {slow}");
    }
}
