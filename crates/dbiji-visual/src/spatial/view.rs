//! Spatial viewer: pan/zoom canvas with grid (lat/lon graticule for SRID
//! 4326), filled polygons (triangulated once per layer), lines, points,
//! hover tooltips and selection. No base map tiles are loaded (privacy).

use super::geometry::{Bounds, Coord, Geom};
use super::layer::SpatialLayer;
use super::triangulate::{Triangulation, triangulate};
use crate::common::colors;
use crate::common::text::truncate_chars;
use crate::common::ticks::nice_step;
use dbiji_core::{tr, trf};
use egui::{Align2, Color32, CursorIcon, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};
use egui_phosphor::regular as icon;

/// One polygon prepared for drawing.
#[derive(Debug, Clone)]
struct PreparedPolygon {
    rings: Vec<Vec<[f64; 2]>>,
    tri: Option<Triangulation>,
}

/// Geometry of one item flattened into drawable parts.
#[derive(Debug, Clone)]
struct PreparedItem {
    item: usize,
    row: usize,
    bounds: Bounds,
    points: Vec<[f64; 2]>,
    lines: Vec<Vec<[f64; 2]>>,
    polygons: Vec<PreparedPolygon>,
}

#[derive(Debug, Clone)]
struct PreparedLayer {
    key: (usize, usize),
    items: Vec<PreparedItem>,
}

fn xy(cs: &[Coord]) -> Vec<[f64; 2]> {
    cs.iter().filter(|c| c.x.is_finite() && c.y.is_finite()).map(|c| [c.x, c.y]).collect()
}

fn collect(g: &Geom, item: &mut PreparedItem) {
    match g {
        Geom::Point(Some(c)) => item.points.push([c.x, c.y]),
        Geom::Point(None) => {}
        Geom::MultiPoint(cs) => item.points.extend(xy(cs)),
        Geom::LineString(cs) => item.lines.push(xy(cs)),
        Geom::MultiLineString(ls) => item.lines.extend(ls.iter().map(|l| xy(l))),
        Geom::Polygon(rings) => push_polygon(rings, item),
        Geom::MultiPolygon(ps) => ps.iter().for_each(|p| push_polygon(p, item)),
        Geom::GeometryCollection(gs) => gs.iter().for_each(|g| collect(g, item)),
    }
}

fn push_polygon(rings: &[Vec<Coord>], item: &mut PreparedItem) {
    let rings: Vec<Vec<[f64; 2]>> = rings.iter().map(|r| xy(r)).filter(|r| r.len() >= 2).collect();
    let Some(outer) = rings.first() else { return };
    let tri = triangulate(outer, &rings[1..]);
    item.polygons.push(PreparedPolygon { rings, tri });
}

fn prepare(layer: &SpatialLayer) -> PreparedLayer {
    let items = layer
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let bounds = it.geometry.bounds()?;
            let mut p =
                PreparedItem { item: i, row: it.row, bounds, points: Vec::new(), lines: Vec::new(), polygons: Vec::new() };
            collect(&it.geometry.geom, &mut p);
            Some(p)
        })
        .collect();
    PreparedLayer { key: (layer.items.as_ptr() as usize, layer.items.len()), items }
}

/// View state of the spatial viewer.
#[derive(Debug, Clone)]
pub struct SpatialState {
    /// World coordinate at the centre of the canvas.
    pub center: [f64; 2],
    /// Screen points per world unit.
    pub scale: f64,
    /// Selected row.
    pub selected_row: Option<usize>,
    /// Row under the pointer.
    pub hovered_row: Option<usize>,
    /// Fit all layers on the next frame.
    pub fit_pending: bool,
    /// Draw the grid / graticule.
    pub show_grid: bool,
    /// One color per row instead of per layer.
    pub color_by_row: bool,
    /// Show the toolbar.
    pub show_toolbar: bool,
    cache: Vec<PreparedLayer>,
    last_canvas: Rect,
    pointer_world: Option<[f64; 2]>,
}

impl Default for SpatialState {
    fn default() -> Self {
        SpatialState {
            center: [0.0, 0.0],
            scale: 1.0,
            selected_row: None,
            hovered_row: None,
            fit_pending: true,
            show_grid: true,
            color_by_row: false,
            show_toolbar: true,
            cache: Vec::new(),
            last_canvas: Rect::NOTHING,
            pointer_world: None,
        }
    }
}

/// Result of [`show_spatial`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpatialResponse {
    /// Row clicked this frame.
    pub clicked_row: Option<usize>,
    /// Row under the pointer.
    pub hovered_row: Option<usize>,
}

struct Xf {
    center: [f64; 2],
    scale: f64,
    origin: Pos2,
}

impl Xf {
    fn to_screen(&self, p: [f64; 2]) -> Pos2 {
        pos2(
            self.origin.x + ((p[0] - self.center[0]) * self.scale) as f32,
            self.origin.y - ((p[1] - self.center[1]) * self.scale) as f32,
        )
    }

    fn to_world(&self, p: Pos2) -> [f64; 2] {
        [
            self.center[0] + (p.x - self.origin.x) as f64 / self.scale,
            self.center[1] - (p.y - self.origin.y) as f64 / self.scale,
        ]
    }
}

fn layer_color(layer: &SpatialLayer, index: usize, dark: bool) -> Color32 {
    layer.color.unwrap_or_else(|| colors::categorical(index, dark))
}

/// Shows the layers on a pan/zoom canvas.
pub fn show_spatial(ui: &mut Ui, layers: &[SpatialLayer], state: &mut SpatialState) -> SpatialResponse {
    let id = ui.id().with("dbiji_spatial");
    // keep the prepared cache in sync with the layers
    state.cache.truncate(layers.len());
    for (i, l) in layers.iter().enumerate() {
        let key = (l.items.as_ptr() as usize, l.items.len());
        match state.cache.get(i) {
            Some(c) if c.key == key => {}
            _ => {
                let p = prepare(l);
                if i < state.cache.len() {
                    state.cache[i] = p;
                } else {
                    state.cache.push(p);
                }
            }
        }
    }
    let srid = layers.iter().find_map(|l| l.srid());
    let geographic = srid == Some(4326) || srid == Some(4258) || srid == Some(4269);
    if state.show_toolbar {
        toolbar(ui, layers, state, srid);
    }
    let rect = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(rect.min, rect.size().max(vec2(64.0, 64.0)));
    let response = ui.interact(rect, id.with("canvas"), Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);
    let stable = state.last_canvas.size() == rect.size();
    state.last_canvas = rect;
    let painter = ui.painter_at(rect);
    let v = ui.visuals().clone();
    painter.rect_filled(rect, 0.0, v.extreme_bg_color);

    if state.fit_pending {
        if !stable {
            ui.ctx().request_repaint();
            return SpatialResponse::default();
        }
        fit(state, layers, rect, geographic);
        state.fit_pending = false;
    }

    // navigation
    if response.dragged_by(egui::PointerButton::Primary) || response.dragged_by(egui::PointerButton::Middle) {
        let d = response.drag_delta();
        state.center[0] -= d.x as f64 / state.scale;
        state.center[1] += d.y as f64 / state.scale;
    }
    if response.contains_pointer() {
        let (zoom, scroll, pointer) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.latest_pos()));
        if scroll != Vec2::ZERO {
            state.center[0] -= scroll.x as f64 / state.scale;
            state.center[1] += scroll.y as f64 / state.scale;
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
        if zoom != 1.0
            && let Some(p) = pointer {
                let xf = Xf { center: state.center, scale: state.scale, origin: rect.center() };
                let anchor = xf.to_world(p);
                let new_scale = (state.scale * zoom as f64).clamp(1e-12, 1e12);
                // keep the world point under the pointer fixed
                state.center[0] = anchor[0] - (p.x - rect.center().x) as f64 / new_scale;
                state.center[1] = anchor[1] + (p.y - rect.center().y) as f64 / new_scale;
                state.scale = new_scale;
            }
    }
    let xf = Xf { center: state.center, scale: state.scale, origin: rect.center() };
    let pointer = ui.input(|i| i.pointer.latest_pos()).filter(|p| rect.contains(*p) && response.contains_pointer());
    state.pointer_world = pointer.map(|p| xf.to_world(p));
    let hovered = pointer.and_then(|p| hit_test(&state.cache, layers, &xf, p));
    state.hovered_row = hovered.map(|(li, ii)| layers.get(li).and_then(|l| l.items.get(ii)).map(|it| it.row).unwrap_or(0));
    let mut clicked_row = None;
    if response.clicked() {
        clicked_row = state.hovered_row;
        state.selected_row = state.hovered_row;
    }

    let min = xf.to_world(rect.left_bottom());
    let max = xf.to_world(rect.right_top());
    let visible = Bounds { min_x: min[0], min_y: min[1], max_x: max[0], max_y: max[1] };
    if state.show_grid {
        grid(&painter, rect, &xf, visible, geographic, &v);
    }

    let dark = v.dark_mode;
    let surface = v.extreme_bg_color;
    let mut highlight: Vec<Shape> = Vec::new();
    for (li, (layer, prep)) in layers.iter().zip(&state.cache).enumerate() {
        let base = layer_color(layer, li, dark);
        let mut fill = Mesh::default();
        let mut strokes: Vec<Shape> = Vec::new();
        let mut dots: Vec<Shape> = Vec::new();
        for it in &prep.items {
            if !it.bounds.intersects(&visible) {
                continue;
            }
            let color = if state.color_by_row { colors::categorical(it.row, dark) } else { base };
            let fill_color = color.gamma_multiply(0.28);
            let is_sel = state.selected_row == Some(it.row);
            let is_hover = state.hovered_row == Some(it.row);
            for poly in &it.polygons {
                if let Some(t) = &poly.tri {
                    let start = fill.vertices.len() as u32;
                    for p in &t.vertices {
                        fill.colored_vertex(xf.to_screen(*p), fill_color);
                    }
                    for tri in &t.triangles {
                        fill.add_triangle(start + tri[0], start + tri[1], start + tri[2]);
                    }
                }
                for ring in &poly.rings {
                    let pts = screen_path(ring, &xf);
                    if pts.len() >= 2 {
                        strokes.push(Shape::closed_line(pts.clone(), Stroke::new(1.2, color)));
                        if is_sel || is_hover {
                            highlight.push(Shape::closed_line(pts, Stroke::new(if is_sel { 3.0 } else { 2.0 }, v.selection.stroke.color)));
                        }
                    }
                }
            }
            for line in &it.lines {
                let pts = screen_path(line, &xf);
                if pts.len() >= 2 {
                    strokes.push(Shape::line(pts.clone(), Stroke::new(2.0, color)));
                    if is_sel || is_hover {
                        highlight.push(Shape::line(pts, Stroke::new(if is_sel { 4.0 } else { 3.0 }, v.selection.stroke.color)));
                    }
                }
            }
            for p in &it.points {
                let s = xf.to_screen(*p);
                dots.push(Shape::circle_filled(s, 6.0, surface));
                dots.push(Shape::circle_filled(s, 4.5, color));
                if is_sel || is_hover {
                    highlight.push(Shape::circle_stroke(s, if is_sel { 8.0 } else { 7.0 }, Stroke::new(2.0, v.selection.stroke.color)));
                }
            }
        }
        if !fill.is_empty() {
            painter.add(Shape::mesh(fill));
        }
        painter.extend(strokes);
        painter.extend(dots);
    }
    painter.extend(highlight);

    if layers.iter().all(|l| l.items.is_empty()) {
        painter.text(rect.center(), Align2::CENTER_CENTER, tr("No geometries to show"), FontId::proportional(14.0), v.weak_text_color());
    }
    if let Some((li, ii)) = hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        if let (Some(layer), Some(item)) = (layers.get(li), layers.get(li).and_then(|l| l.items.get(ii))) {
            let r = response.clone();
            r.on_hover_ui_at_pointer(|ui| {
                ui.strong(item.label.clone().unwrap_or_else(|| trf("Row {}", &[&(item.row + 1)])));
                ui.weak(trf("{} · row {} · {} points", &[&item.geometry.type_name(), &(item.row + 1), &item.geometry.num_points()]));
                if !layer.name.is_empty() {
                    ui.weak(layer.name.as_str());
                }
                ui.label(egui::RichText::new(truncate_chars(&item.geometry.to_wkt(), 200)).monospace().small());
            });
        }
    }
    SpatialResponse { clicked_row, hovered_row: state.hovered_row }
}

/// Screen path with points closer than half a pixel merged (level of detail).
fn screen_path(points: &[[f64; 2]], xf: &Xf) -> Vec<Pos2> {
    let mut out: Vec<Pos2> = Vec::with_capacity(points.len());
    for p in points {
        let s = xf.to_screen(*p);
        if out.last().is_none_or(|l| (*l - s).length_sq() >= 0.25) {
            out.push(s);
        }
    }
    out
}

fn fit(state: &mut SpatialState, layers: &[SpatialLayer], rect: Rect, geographic: bool) {
    let Some(b) = layers.iter().filter_map(SpatialLayer::bounds).reduce(Bounds::union) else {
        state.center = [0.0, 0.0];
        state.scale = 1.0;
        return;
    };
    let min_span = if geographic { 0.02 } else { (b.center().0.abs().max(b.center().1.abs()) * 1e-3).max(10.0) };
    let w = b.width().max(min_span);
    let h = b.height().max(min_span);
    state.center = [b.center().0, b.center().1];
    state.scale = ((rect.width() as f64 / w).min(rect.height() as f64 / h) * 0.9).clamp(1e-12, 1e12);
}

fn dist_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
    ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()
}

/// Even-odd point in polygon over all rings.
pub(crate) fn point_in_rings(p: [f64; 2], rings: &[Vec<[f64; 2]>]) -> bool {
    let mut inside = false;
    for ring in rings {
        let n = ring.len();
        if n < 3 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (a, b) = (ring[i], ring[j]);
            if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

fn hit_test(cache: &[PreparedLayer], layers: &[SpatialLayer], xf: &Xf, pointer: Pos2) -> Option<(usize, usize)> {
    let w = xf.to_world(pointer);
    let tol = 6.0 / xf.scale;
    let layer_count = cache.len().min(layers.len());
    // Same order as painting: points and lines are drawn above polygon
    // fills, later layers and items above earlier ones.
    for pass in 0..2 {
        for (li, prep) in cache.iter().enumerate().take(layer_count).rev() {
            for it in prep.items.iter().rev() {
                if !it.bounds.expand(tol).contains(w[0], w[1]) {
                    continue;
                }
                let hit = if pass == 0 {
                    it.points.iter().any(|p| ((p[0] - w[0]).powi(2) + (p[1] - w[1]).powi(2)).sqrt() <= tol)
                        || it.lines.iter().any(|l| l.windows(2).any(|s| dist_segment(w, s[0], s[1]) <= tol * 0.8))
                } else {
                    it.polygons.iter().any(|p| {
                        point_in_rings(w, &p.rings)
                            || p.rings.iter().any(|r| r.windows(2).any(|s| dist_segment(w, s[0], s[1]) <= tol * 0.5))
                    })
                };
                if hit {
                    return Some((li, it.item));
                }
            }
        }
    }
    None
}

fn toolbar(ui: &mut Ui, layers: &[SpatialLayer], state: &mut SpatialState, srid: Option<i32>) {
    ui.horizontal(|ui| {
        if ui.button(icon::CORNERS_OUT).on_hover_text(tr("Zoom to fit")).clicked() {
            state.fit_pending = true;
        }
        ui.toggle_value(&mut state.show_grid, icon::GRID_FOUR).on_hover_text(tr("Show grid"));
        ui.toggle_value(&mut state.color_by_row, icon::PALETTE).on_hover_text(tr("One color per row"));
        ui.separator();
        let items: usize = layers.iter().map(|l| l.items.len()).sum();
        ui.weak(trf("{} geometries", &[&items]));
        let errors: usize = layers.iter().map(|l| l.errors.len()).sum();
        if errors > 0 {
            let detail = layers
                .iter()
                .flat_map(|l| l.errors.iter())
                .take(5)
                .map(|(row, e)| trf("row {}: {}", &[&(row + 1), e]))
                .collect::<Vec<_>>()
                .join("\n");
            ui.colored_label(ui.visuals().warn_fg_color, format!("{} {}", icon::WARNING, trf("{} not readable", &[&errors])))
                .on_hover_text(detail);
        }
        if let Some(s) = srid {
            ui.weak(format!("SRID {s}"));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(tr("No base map: geometries only"));
            if let Some([x, y]) = state.pointer_world {
                ui.monospace(format!("{x:.6}, {y:.6}"));
            }
        });
    });
}

fn grid(painter: &egui::Painter, rect: Rect, xf: &Xf, visible: Bounds, geographic: bool, v: &egui::Visuals) {
    let span = visible.width().max(visible.height());
    if !span.is_finite() || span <= 0.0 {
        return;
    }
    let mut step = nice_step(visible.width().max(1e-12), 8);
    if geographic {
        step = step.min(30.0);
    }
    let stroke = Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6));
    let label_color = v.weak_text_color();
    let font = FontId::proportional(10.5);
    let decimals = if step >= 1.0 { 0 } else { (-step.log10()).ceil().clamp(0.0, 10.0) as usize };
    let fmt = |val: f64, axis_x: bool| -> String {
        if geographic {
            let hemi = match (axis_x, val >= 0.0) {
                (true, true) => "E",
                (true, false) => "W",
                (false, true) => "N",
                (false, false) => "S",
            };
            if val == 0.0 { "0°".to_string() } else { format!("{:.*}°{hemi}", decimals, val.abs()) }
        } else {
            format!("{val:.decimals$}")
        }
    };
    let mut lines = 0;
    let mut x = (visible.min_x / step).ceil() * step;
    while x <= visible.max_x && lines < 200 {
        if !geographic || (-180.0..=180.0).contains(&x) {
            let sx = xf.to_screen([x, 0.0]).x;
            painter.vline(sx, rect.y_range(), stroke);
            painter.text(pos2(sx + 3.0, rect.bottom() - 3.0), Align2::LEFT_BOTTOM, fmt(x, true), font.clone(), label_color);
        }
        x += step;
        lines += 1;
    }
    let mut y = (visible.min_y / step).ceil() * step;
    while y <= visible.max_y && lines < 400 {
        if !geographic || (-90.0..=90.0).contains(&y) {
            let sy = xf.to_screen([0.0, y]).y;
            painter.hline(rect.x_range(), sy, stroke);
            painter.text(pos2(rect.left() + 3.0, sy - 2.0), Align2::LEFT_BOTTOM, fmt(y, false), font.clone(), label_color);
        }
        y += step;
        lines += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::layer::SpatialItem;
    use crate::spatial::wkt::parse_wkt;

    fn layer(wkts: &[&str]) -> SpatialLayer {
        SpatialLayer {
            items: wkts
                .iter()
                .enumerate()
                .map(|(i, w)| SpatialItem { row: i, geometry: parse_wkt(w).unwrap(), label: None })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn prepare_and_hit_test() {
        let l = layer(&[
            "POLYGON((0 0,10 0,10 10,0 10,0 0),(4 4,6 4,6 6,4 6,4 4))",
            "LINESTRING(20 0,30 10)",
            "MULTIPOINT((40 5),(45 5))",
            "GEOMETRYCOLLECTION(POINT(50 50),POLYGON((60 60,61 60,61 61,60 60)))",
        ]);
        let p = prepare(&l);
        assert_eq!(p.items.len(), 4);
        assert!(p.items[0].polygons[0].tri.is_some());
        assert_eq!(p.items[2].points.len(), 2);
        assert_eq!(p.items[3].points.len(), 1);
        assert_eq!(p.items[3].polygons.len(), 1);
        let xf = Xf { center: [25.0, 25.0], scale: 10.0, origin: pos2(500.0, 500.0) };
        let layers = vec![l];
        let cache = vec![p];
        let at = |x: f64, y: f64| hit_test(&cache, &layers, &xf, xf.to_screen([x, y]));
        assert_eq!(at(2.0, 2.0), Some((0, 0)), "inside polygon");
        assert_eq!(at(5.0, 5.0), None, "inside the hole");
        assert_eq!(at(25.0, 5.0), Some((0, 1)), "on the line");
        assert_eq!(at(45.2, 5.1), Some((0, 2)), "near a point");
        assert_eq!(at(35.0, 35.0), None);
        // screen transform roundtrip
        let w = xf.to_world(xf.to_screen([12.5, -3.25]));
        assert!((w[0] - 12.5).abs() < 1e-3 && (w[1] + 3.25).abs() < 1e-3);
    }

    #[test]
    fn point_in_polygon_even_odd() {
        let outer = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let hole = vec![[4.0, 4.0], [6.0, 4.0], [6.0, 6.0], [4.0, 6.0]];
        assert!(point_in_rings([1.0, 1.0], &[outer.clone(), hole.clone()]));
        assert!(!point_in_rings([5.0, 5.0], &[outer.clone(), hole]));
        assert!(!point_in_rings([11.0, 5.0], &[outer]));
    }

    #[test]
    fn viewer_click_selects_a_row() {
        use crate::test_harness::Harness;
        let mut l = layer(&["SRID=4326;POINT(106.8 -6.2)", "SRID=4326;POLYGON((100 -10,110 -10,110 0,100 0,100 -10))"]);
        l.name = "demo".into();
        let layers = vec![l];
        let mut state = SpatialState::default();
        let mut h = Harness::new(1000.0, 700.0);
        let mut resp = SpatialResponse::default();
        let mut ui_fn = |ui: &mut Ui| resp = show_spatial(ui, &layers, &mut state);
        h.idle(3, &mut ui_fn);
        drop(ui_fn);
        assert!(!state.fit_pending);
        // the point is drawn above the polygon: clicking it selects row 0
        let xf = Xf { center: state.center, scale: state.scale, origin: state.last_canvas.center() };
        let p = xf.to_screen([106.8, -6.2]);
        let mut clicked = None;
        let mut ui_fn = |ui: &mut Ui| {
            let r = show_spatial(ui, &layers, &mut state);
            if r.clicked_row.is_some() {
                clicked = r.clicked_row;
            }
        };
        h.click(p, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(clicked, Some(0));
        assert_eq!(state.selected_row, Some(0));
        // inside the polygon away from the point
        let p = xf.to_screen([102.0, -8.0]);
        let mut ui_fn = |ui: &mut Ui| {
            resp = show_spatial(ui, &layers, &mut state);
        };
        h.hover(p, &mut ui_fn);
        drop(ui_fn);
        assert_eq!(resp.hovered_row, Some(1));
        // zooming in keeps rendering, empty layers too
        let scale = state.scale;
        let mut ui_fn = |ui: &mut Ui| {
            show_spatial(ui, &layers, &mut state);
        };
        h.frame(vec![egui::Event::Zoom(2.0)], &mut ui_fn);
        h.idle(1, &mut ui_fn);
        drop(ui_fn);
        assert!(state.scale > scale);
        let none: Vec<SpatialLayer> = vec![SpatialLayer::default()];
        let mut s2 = SpatialState::default();
        let mut ui_fn = |ui: &mut Ui| {
            show_spatial(ui, &none, &mut s2);
        };
        h.idle(3, &mut ui_fn);
    }
}
