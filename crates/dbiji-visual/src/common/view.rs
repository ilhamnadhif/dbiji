//! Pan / zoom transform shared by the diagram canvases.
//!
//! The canvases do their own world -> screen transform (instead of using an
//! `egui::Scene` layer transform) so that text is laid out at the zoomed size
//! and stays crisp, and so that off-screen items can be culled cheaply.

use egui::{Pos2, Rect, Response, Ui, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

/// World -> screen mapping of a canvas: `screen = origin + pan + world * zoom`,
/// where `origin` is the top-left corner of the canvas on screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewTransform {
    /// Scale factor (1 = 100 %).
    pub zoom: f32,
    /// Offset of the world origin from the canvas origin, in screen points.
    #[serde(with = "crate::common::serde_helpers::vec2")]
    pub pan: Vec2,
}

impl Default for ViewTransform {
    fn default() -> Self {
        ViewTransform { zoom: 1.0, pan: vec2(20.0, 20.0) }
    }
}

impl ViewTransform {
    /// Creates a transform.
    pub fn new(zoom: f32, pan: Vec2) -> Self {
        ViewTransform { zoom: sanitize_zoom(zoom), pan }
    }

    /// World point to screen point.
    pub fn to_screen(&self, origin: Pos2, p: Pos2) -> Pos2 {
        origin + self.pan + p.to_vec2() * self.zoom
    }

    /// Screen point to world point.
    pub fn to_world(&self, origin: Pos2, p: Pos2) -> Pos2 {
        let z = sanitize_zoom(self.zoom);
        ((p - origin - self.pan) / z).to_pos2()
    }

    /// World rectangle to screen rectangle.
    pub fn rect_to_screen(&self, origin: Pos2, r: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(origin, r.min), self.to_screen(origin, r.max))
    }

    /// Screen rectangle to world rectangle.
    pub fn rect_to_world(&self, origin: Pos2, r: Rect) -> Rect {
        Rect::from_min_max(self.to_world(origin, r.min), self.to_world(origin, r.max))
    }

    /// Multiplies the zoom by `factor` keeping the world point under
    /// `anchor` (screen) fixed. The result is clamped to `min..=max`.
    pub fn zoom_around(&mut self, origin: Pos2, anchor: Pos2, factor: f32, min: f32, max: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let world = self.to_world(origin, anchor);
        let new_zoom = (sanitize_zoom(self.zoom) * factor).clamp(min, max);
        self.zoom = new_zoom;
        self.pan = anchor - origin - world.to_vec2() * new_zoom;
    }

    /// Zooms and pans so that `world` fits into a canvas of `canvas_size`
    /// with `margin` screen points around it.
    pub fn fit(&mut self, canvas_size: Vec2, world: Rect, margin: f32, min: f32, max: f32) {
        if !world.is_finite() || world.width() < 0.0 || world.height() < 0.0 {
            return;
        }
        let avail = (canvas_size - Vec2::splat(2.0 * margin)).max(Vec2::splat(10.0));
        let size = world.size().max(Vec2::splat(1.0));
        let zoom = (avail.x / size.x).min(avail.y / size.y).clamp(min, max);
        self.zoom = zoom;
        let center = world.center();
        self.pan = canvas_size * 0.5 - center.to_vec2() * zoom;
    }
}

fn sanitize_zoom(z: f32) -> f32 {
    if z.is_finite() && z > 1e-4 { z } else { 1.0 }
}

/// What the pan/zoom handler did this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct PanZoom {
    pub changed: bool,
}

/// Handles wheel/trackpad scrolling (pan), cmd/ctrl + scroll and pinch
/// (zoom around the pointer) and middle-button dragging (pan). When
/// `primary_pans` is true, dragging with the primary button pans as well.
pub(crate) fn handle_pan_zoom(
    ui: &Ui,
    response: &Response,
    view: &mut ViewTransform,
    origin: Pos2,
    zoom_range: (f32, f32),
    primary_pans: bool,
) -> PanZoom {
    let mut out = PanZoom::default();
    if response.dragged_by(egui::PointerButton::Middle) || (primary_pans && response.dragged_by(egui::PointerButton::Primary)) {
        let d = response.drag_delta();
        if d != Vec2::ZERO {
            view.pan += d;
            out.changed = true;
        }
    }
    if response.contains_pointer() {
        let (zoom_delta, scroll, pointer) =
            ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.latest_pos()));
        if zoom_delta != 1.0
            && let Some(p) = pointer {
                view.zoom_around(origin, p, zoom_delta, zoom_range.0, zoom_range.1);
                out.changed = true;
            }
        if scroll != Vec2::ZERO {
            view.pan += scroll;
            out.changed = true;
            // The canvas consumed the scroll; do not let an enclosing scroll area move too.
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
    }
    out
}

/// Distance from `p` to the segment `a`-`b`.
pub(crate) fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

/// Distance from `p` to a polyline.
pub(crate) fn distance_to_polyline(p: Pos2, points: &[Pos2]) -> f32 {
    points
        .windows(2)
        .map(|w| distance_to_segment(p, w[0], w[1]))
        .fold(f32::INFINITY, f32::min)
}

/// Bounding rectangle of points (`None` when empty).
pub(crate) fn bounds_of(points: impl IntoIterator<Item = Pos2>) -> Option<Rect> {
    let mut it = points.into_iter();
    let first = it.next()?;
    let mut r = Rect::from_min_max(first, first);
    for p in it {
        r.extend_with(p);
    }
    Some(r)
}

/// Rounds a point to whole units (used when dropping dragged items).
pub(crate) fn round_pos(p: Pos2) -> Pos2 {
    pos2(p.x.round(), p.y.round())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_zoom_anchor() {
        let origin = pos2(100.0, 50.0);
        let mut v = ViewTransform::new(1.5, vec2(10.0, -20.0));
        let w = pos2(33.0, 44.0);
        let s = v.to_screen(origin, w);
        let back = v.to_world(origin, s);
        assert!((back - w).length() < 1e-4);
        let anchor = pos2(400.0, 300.0);
        let under = v.to_world(origin, anchor);
        v.zoom_around(origin, anchor, 2.0, 0.1, 10.0);
        assert!((v.zoom - 3.0).abs() < 1e-6);
        assert!((v.to_world(origin, anchor) - under).length() < 1e-3);
        v.zoom_around(origin, anchor, 100.0, 0.1, 10.0);
        assert_eq!(v.zoom, 10.0);
    }

    #[test]
    fn fit_centers_world() {
        let mut v = ViewTransform::default();
        let world = Rect::from_min_size(pos2(1000.0, 1000.0), vec2(2000.0, 1000.0));
        v.fit(vec2(1000.0, 800.0), world, 20.0, 0.01, 4.0);
        let origin = Pos2::ZERO;
        let r = v.rect_to_screen(origin, world);
        assert!(r.min.x >= 19.0 && r.max.x <= 981.0, "{r:?}");
        assert!((r.center() - pos2(500.0, 400.0)).length() < 0.5);
    }

    #[test]
    fn segment_distance() {
        let d = distance_to_segment(pos2(5.0, 5.0), pos2(0.0, 0.0), pos2(10.0, 0.0));
        assert!((d - 5.0).abs() < 1e-6);
        let d = distance_to_segment(pos2(-3.0, 4.0), pos2(0.0, 0.0), pos2(10.0, 0.0));
        assert!((d - 5.0).abs() < 1e-6);
        assert_eq!(distance_to_polyline(pos2(0.0, 0.0), &[]), f32::INFINITY);
    }
}
