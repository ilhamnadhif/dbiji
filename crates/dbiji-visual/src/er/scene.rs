//! Resolved geometry of a diagram: box rectangles and relation routes in
//! diagram coordinates. Shared by the canvas, hit testing and the exports.
//!
//! Relations are routed orthogonally between the side of the FK column row
//! in the child box and the side of the referenced key row in the parent
//! box: side by side boxes get an S/Z shaped route through the gap between
//! them, overlapping boxes a C shaped route around the nearer side; parallel
//! relations between the same pair of boxes use separate lanes.

use super::diagram::ErDiagram;
use super::geometry::{TableGeometry, table_geometry};
use egui::{Pos2, Rect, Vec2, pos2, vec2};
use std::collections::{BTreeMap, HashMap};

/// Length of the horizontal stub leaving a box side.
pub(crate) const STUB: f32 = 22.0;
/// Distance between parallel lanes.
pub(crate) const LANE: f32 = 8.0;

/// One routed relation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Route {
    /// Polyline from the child (FK side) to the parent (referenced side).
    pub points: Vec<Pos2>,
    /// Unit vector pointing out of the child box at the first point.
    pub start_dir: Vec2,
    /// Unit vector pointing out of the parent box at the last point.
    pub end_dir: Vec2,
    /// Bounding box (for culling).
    pub bbox: Rect,
}

impl Route {
    fn new(points: Vec<Pos2>, start_dir: Vec2, end_dir: Vec2) -> Route {
        let bbox = crate::common::view::bounds_of(points.iter().copied())
            .unwrap_or(Rect::NOTHING)
            .expand(STUB);
        Route { points, start_dir, end_dir, bbox }
    }
}

/// Routes a relation between two different boxes. `child_y` / `parent_y`
/// are absolute y coordinates of the attachment rows, `lane` an offset of
/// the vertical segment.
pub(crate) fn route(child: Rect, child_y: f32, parent: Rect, parent_y: f32, lane: f32) -> Route {
    let (y1, y2) = (child_y, parent_y);
    let stub2 = STUB * 2.0;
    if child.left() >= parent.right() + stub2 {
        // child right of parent: leave left, enter parent's right side
        let (a, b) = (parent.right(), child.left());
        let mid = ((a + b) * 0.5 + lane).clamp(a + 8.0, b - 8.0);
        return Route::new(z_points(pos2(b, y1), mid, pos2(a, y2)), vec2(-1.0, 0.0), vec2(1.0, 0.0));
    }
    if parent.left() >= child.right() + stub2 {
        let (a, b) = (child.right(), parent.left());
        let mid = ((a + b) * 0.5 + lane).clamp(a + 8.0, b - 8.0);
        return Route::new(z_points(pos2(a, y1), mid, pos2(b, y2)), vec2(1.0, 0.0), vec2(-1.0, 0.0));
    }
    // Horizontally overlapping: go around the side with the shorter detour.
    let out = STUB + lane.abs();
    let right_x = child.right().max(parent.right()) + out;
    let left_x = child.left().min(parent.left()) - out;
    let right_cost = (right_x - child.right()) + (right_x - parent.right());
    let left_cost = (child.left() - left_x) + (parent.left() - left_x);
    if right_cost <= left_cost {
        let pts = vec![pos2(child.right(), y1), pos2(right_x, y1), pos2(right_x, y2), pos2(parent.right(), y2)];
        Route::new(pts, vec2(1.0, 0.0), vec2(1.0, 0.0))
    } else {
        let pts = vec![pos2(child.left(), y1), pos2(left_x, y1), pos2(left_x, y2), pos2(parent.left(), y2)];
        Route::new(pts, vec2(-1.0, 0.0), vec2(-1.0, 0.0))
    }
}

/// Loop on the right side of a self-referencing table.
pub(crate) fn self_route(rect: Rect, y1: f32, y2: f32, lane: f32) -> Route {
    let y2 = if (y1 - y2).abs() < 6.0 { y1 + 10.0 } else { y2 };
    let x = rect.right() + STUB + lane.abs() + 8.0;
    let pts = vec![pos2(rect.right(), y1), pos2(x, y1), pos2(x, y2), pos2(rect.right(), y2)];
    Route::new(pts, vec2(1.0, 0.0), vec2(1.0, 0.0))
}

fn z_points(start: Pos2, mid_x: f32, end: Pos2) -> Vec<Pos2> {
    if (start.y - end.y).abs() < 0.5 {
        vec![start, pos2(end.x, start.y)]
    } else {
        vec![start, pos2(mid_x, start.y), pos2(mid_x, end.y), end]
    }
}

/// A box of the scene.
#[derive(Debug, Clone)]
pub(crate) struct SceneTable {
    /// Index into `ErModel::tables`.
    pub table: usize,
    pub rect: Rect,
    pub geom: TableGeometry,
}

/// A routed relation of the scene.
#[derive(Debug, Clone)]
pub(crate) struct SceneRelation {
    /// Index into `ErModel::relations`.
    pub relation: usize,
    pub route: Route,
}

/// Resolved geometry of a whole diagram.
#[derive(Debug, Clone, Default)]
pub(crate) struct ErScene {
    pub tables: Vec<SceneTable>,
    pub relations: Vec<SceneRelation>,
    /// Table key -> index into `tables`.
    pub index: HashMap<String, usize>,
}

impl ErScene {
    /// Builds the scene; tables without a position are skipped.
    pub fn build(diagram: &ErDiagram) -> ErScene {
        let mut scene = ErScene::default();
        for (i, t) in diagram.model.tables.iter().enumerate() {
            let Some(pos) = diagram.positions.get(&t.key) else { continue };
            if scene.index.contains_key(&t.key) {
                continue;
            }
            let geom = table_geometry(t, &diagram.settings);
            scene.index.insert(t.key.clone(), scene.tables.len());
            scene.tables.push(SceneTable { table: i, rect: Rect::from_min_size(*pos, geom.size), geom });
        }
        scene.route_all(diagram);
        scene
    }

    /// (Re)computes every route from the current table rectangles.
    pub fn route_all(&mut self, diagram: &ErDiagram) {
        self.relations.clear();
        // lanes: relations sharing the same pair of boxes
        let mut groups: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
        let mut ends = Vec::new();
        for (ri, r) in diagram.model.relations.iter().enumerate() {
            let (Some(&c), Some(&p)) = (self.index.get(&r.from_table), self.index.get(&r.to_table)) else { continue };
            groups.entry((c.min(p), c.max(p))).or_default().push(ends.len());
            ends.push((ri, c, p));
        }
        let mut lane = vec![0.0f32; ends.len()];
        for (&(a, b), members) in &groups {
            let n = members.len() as f32;
            for (i, &m) in members.iter().enumerate() {
                // self loops nest outwards, other relations spread around the middle
                lane[m] = if a == b { i as f32 * LANE } else { (i as f32 - (n - 1.0) * 0.5) * LANE };
            }
        }
        for (k, &(ri, c, p)) in ends.iter().enumerate() {
            let (Some(rel), Some(ct), Some(pt)) = (diagram.model.relations.get(ri), self.tables.get(c), self.tables.get(p))
            else {
                continue;
            };
            let (Some(child_t), Some(parent_t)) = (diagram.model.tables.get(ct.table), diagram.model.tables.get(pt.table)) else {
                continue;
            };
            let y1 = ct.rect.top() + ct.geom.anchor_y(child_t, rel.from_columns.first().map(String::as_str));
            let y2 = pt.rect.top() + pt.geom.anchor_y(parent_t, rel.to_columns.first().map(String::as_str));
            let lane_k = lane.get(k).copied().unwrap_or(0.0);
            let route = if c == p { self_route(ct.rect, y1, y2, lane_k) } else { route(ct.rect, y1, pt.rect, y2, lane_k) };
            self.relations.push(SceneRelation { relation: ri, route });
        }
    }

    /// Bounding box of all boxes (`None` when empty).
    pub fn bounds(&self) -> Option<Rect> {
        self.tables.iter().map(|t| t.rect).reduce(|a, b| a.union(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(x, y), vec2(w, h))
    }

    #[test]
    fn side_by_side_routes_through_gap() {
        let parent = r(0.0, 0.0, 100.0, 80.0);
        let child = r(300.0, 50.0, 100.0, 80.0);
        let rt = route(child, 70.0, parent, 20.0, 0.0);
        assert_eq!(rt.points.first(), Some(&pos2(300.0, 70.0)));
        assert_eq!(rt.points.last(), Some(&pos2(100.0, 20.0)));
        assert_eq!(rt.points.len(), 4);
        for w in rt.points.windows(2) {
            assert!(w[0].x == w[1].x || w[0].y == w[1].y, "orthogonal segments");
        }
        assert!(rt.points.iter().all(|p| p.x >= 100.0 && p.x <= 300.0));
        assert_eq!(rt.start_dir, vec2(-1.0, 0.0));
        assert_eq!(rt.end_dir, vec2(1.0, 0.0));
        // same height: straight line
        let rt = route(child, 20.0, parent, 20.0, 0.0);
        assert_eq!(rt.points.len(), 2);
        // mirrored
        let rt = route(parent, 20.0, child, 70.0, 0.0);
        assert_eq!(rt.start_dir, vec2(1.0, 0.0));
    }

    #[test]
    fn stacked_boxes_route_around() {
        let parent = r(0.0, 0.0, 100.0, 80.0);
        let child = r(20.0, 200.0, 100.0, 80.0);
        let rt = route(child, 230.0, parent, 20.0, 0.0);
        assert_eq!(rt.points.len(), 4);
        let x = rt.points[1].x;
        assert!(x > 120.0 || x < 0.0, "outside both boxes: {x}");
        let rt = self_route(parent, 30.0, 30.0, 0.0);
        assert_eq!(rt.points.len(), 4);
        assert!(rt.points[1].x > parent.right());
        assert_ne!(rt.points[0].y, rt.points[3].y);
    }
}
