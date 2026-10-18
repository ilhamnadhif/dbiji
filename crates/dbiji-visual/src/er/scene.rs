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
        Route {
            points,
            start_dir,
            end_dir,
            bbox,
        }
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
        return Route::new(
            z_points(pos2(b, y1), mid, pos2(a, y2)),
            vec2(-1.0, 0.0),
            vec2(1.0, 0.0),
        );
    }
    if parent.left() >= child.right() + stub2 {
        let (a, b) = (child.right(), parent.left());
        let mid = ((a + b) * 0.5 + lane).clamp(a + 8.0, b - 8.0);
        return Route::new(
            z_points(pos2(a, y1), mid, pos2(b, y2)),
            vec2(1.0, 0.0),
            vec2(-1.0, 0.0),
        );
    }
    // Horizontally overlapping: go around the side with the shorter detour.
    let out = STUB + lane.abs();
    let right_x = child.right().max(parent.right()) + out;
    let left_x = child.left().min(parent.left()) - out;
    let right_cost = (right_x - child.right()) + (right_x - parent.right());
    let left_cost = (child.left() - left_x) + (parent.left() - left_x);
    if right_cost <= left_cost {
        let pts = vec![
            pos2(child.right(), y1),
            pos2(right_x, y1),
            pos2(right_x, y2),
            pos2(parent.right(), y2),
        ];
        Route::new(pts, vec2(1.0, 0.0), vec2(1.0, 0.0))
    } else {
        let pts = vec![
            pos2(child.left(), y1),
            pos2(left_x, y1),
            pos2(left_x, y2),
            pos2(parent.left(), y2),
        ];
        Route::new(pts, vec2(-1.0, 0.0), vec2(-1.0, 0.0))
    }
}

/// Loop on the right side of a self-referencing table.
pub(crate) fn self_route(rect: Rect, y1: f32, y2: f32, lane: f32) -> Route {
    let y2 = if (y1 - y2).abs() < 6.0 { y1 + 10.0 } else { y2 };
    let x = rect.right() + STUB + lane.abs() + 8.0;
    let pts = vec![
        pos2(rect.right(), y1),
        pos2(x, y1),
        pos2(x, y2),
        pos2(rect.right(), y2),
    ];
    Route::new(pts, vec2(1.0, 0.0), vec2(1.0, 0.0))
}

fn z_points(start: Pos2, mid_x: f32, end: Pos2) -> Vec<Pos2> {
    if (start.y - end.y).abs() < 0.5 {
        vec![start, pos2(end.x, start.y)]
    } else {
        vec![start, pos2(mid_x, start.y), pos2(mid_x, end.y), end]
    }
}

/// Removes repeated and collinear interior points of an orthogonal polyline.
fn simplify(points: Vec<Pos2>) -> Vec<Pos2> {
    let mut out: Vec<Pos2> = Vec::with_capacity(points.len());
    for p in points {
        if out.last().is_some_and(|l| (*l - p).length_sq() < 0.01) {
            continue;
        }
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            let collinear = ((a.x - b.x).abs() < 0.01 && (b.x - p.x).abs() < 0.01)
                || ((a.y - b.y).abs() < 0.01 && (b.y - p.y).abs() < 0.01);
            if collinear {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

/// Uniform grid over the table rectangles for obstacle queries.
pub(crate) struct RectIndex {
    cell: f32,
    rects: Vec<Rect>,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

impl RectIndex {
    pub(crate) fn new(rects: Vec<Rect>) -> Self {
        let mut index = RectIndex {
            cell: 256.0,
            rects: Vec::new(),
            cells: HashMap::new(),
        };
        for r in rects {
            let id = index.rects.len();
            let (x0, y0, x1, y1) = index.range(r);
            for cx in x0..=x1 {
                for cy in y0..=y1 {
                    index.cells.entry((cx, cy)).or_default().push(id);
                }
            }
            index.rects.push(r);
        }
        index
    }

    fn range(&self, r: Rect) -> (i32, i32, i32, i32) {
        let f = |v: f32| (v / self.cell).floor().clamp(-1e5, 1e5) as i32;
        (f(r.min.x), f(r.min.y), f(r.max.x), f(r.max.y))
    }

    /// Is any rectangle other than `exclude` intersecting `area`?
    fn any_in(&self, area: Rect, exclude: [usize; 2]) -> bool {
        let hit = |i: usize| {
            !exclude.contains(&i) && self.rects.get(i).is_some_and(|r| r.intersects(area))
        };
        let (x0, y0, x1, y1) = self.range(area);
        if (x1 - x0 + 1) as i64 * (y1 - y0 + 1) as i64 > 64 {
            return (0..self.rects.len()).any(hit);
        }
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(ids) = self.cells.get(&(cx, cy))
                    && ids.iter().any(|&i| hit(i))
                {
                    return true;
                }
            }
        }
        false
    }

    /// Does the polyline cross a rectangle other than `exclude`?
    fn blocked(&self, points: &[Pos2], exclude: [usize; 2]) -> bool {
        points
            .windows(2)
            .any(|w| self.any_in(Rect::from_two_pos(w[0], w[1]).expand(1.5), exclude))
    }
}

/// Re-routes a side by side relation whose direct route crosses other
/// boxes: vertical runs in the gaps next to the child and the parent and a
/// horizontal run along the nearest free corridor.
fn avoid_obstacles(
    base: Route,
    child: Rect,
    parent: Rect,
    lane: f32,
    spread: f32,
    index: &RectIndex,
    exclude: [usize; 2],
) -> Route {
    if !index.blocked(&base.points, exclude) {
        return base;
    }
    let (Some(&start), Some(&end)) = (base.points.first(), base.points.last()) else {
        return base;
    };
    let child_right_of_parent = child.left() >= parent.right();
    let gap_in = 12.0 + lane.abs() * 0.5 + spread;
    let (xc, xp) = if child_right_of_parent {
        (child.left() - gap_in, parent.right() + gap_in)
    } else {
        (child.right() + gap_in, parent.left() - gap_in)
    };
    let mut candidates: Vec<Vec<Pos2>> = vec![
        vec![start, pos2(xc, start.y), pos2(xc, end.y), end],
        vec![start, pos2(xp, start.y), pos2(xp, end.y), end],
    ];
    let (band_lo, band_hi) = (xp.min(xc), xp.max(xc));
    let spacing = 8.0 + lane.abs() + spread;
    let mut ys: Vec<f32> = vec![start.y, end.y];
    for (i, r) in index.rects.iter().enumerate() {
        if exclude.contains(&i) || r.right() < band_lo || r.left() > band_hi {
            continue;
        }
        ys.push(r.top() - spacing);
        ys.push(r.bottom() + spacing);
    }
    let ideal = (start.y + end.y) * 0.5;
    ys.sort_by(|a, b| {
        let ca = (a - start.y).abs() + (a - end.y).abs() + (a - ideal).abs() * 0.01;
        let cb = (b - start.y).abs() + (b - end.y).abs() + (b - ideal).abs() * 0.01;
        ca.total_cmp(&cb)
    });
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    for y in ys.into_iter().take(48) {
        candidates.push(vec![
            start,
            pos2(xc, start.y),
            pos2(xc, y),
            pos2(xp, y),
            pos2(xp, end.y),
            end,
        ]);
    }
    for pts in candidates {
        let pts = simplify(pts);
        if !index.blocked(&pts, exclude) {
            return Route::new(pts, base.start_dir, base.end_dir);
        }
    }
    base
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
    /// Indices into `ErScene::tables`.
    child: usize,
    parent: usize,
    /// Lane offset among relations of the same pair of boxes.
    lane: f32,
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
            let Some(pos) = diagram.positions.get(&t.key) else {
                continue;
            };
            if scene.index.contains_key(&t.key) {
                continue;
            }
            let geom = table_geometry(t, &diagram.settings);
            scene.index.insert(t.key.clone(), scene.tables.len());
            scene.tables.push(SceneTable {
                table: i,
                rect: Rect::from_min_size(*pos, geom.size),
                geom,
            });
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
            let (Some(&c), Some(&p)) = (self.index.get(&r.from_table), self.index.get(&r.to_table))
            else {
                continue;
            };
            groups
                .entry((c.min(p), c.max(p)))
                .or_default()
                .push(ends.len());
            ends.push((ri, c, p));
        }
        let mut lane = vec![0.0f32; ends.len()];
        for (&(a, b), members) in &groups {
            let n = members.len() as f32;
            for (i, &m) in members.iter().enumerate() {
                // self loops nest outwards, other relations spread around the middle
                lane[m] = if a == b {
                    i as f32 * LANE
                } else {
                    (i as f32 - (n - 1.0) * 0.5) * LANE
                };
            }
        }
        let index = RectIndex::new(self.tables.iter().map(|t| t.rect).collect());
        for (k, &(ri, c, p)) in ends.iter().enumerate() {
            let lane_k = lane.get(k).copied().unwrap_or(0.0);
            if let Some(route) = self.route_one(diagram, ri, c, p, lane_k, &index) {
                self.relations.push(SceneRelation {
                    relation: ri,
                    child: c,
                    parent: p,
                    lane: lane_k,
                    route,
                });
            }
        }
    }

    fn route_one(
        &self,
        diagram: &ErDiagram,
        ri: usize,
        c: usize,
        p: usize,
        lane: f32,
        index: &RectIndex,
    ) -> Option<Route> {
        let rel = diagram.model.relations.get(ri)?;
        let (ct, pt) = (self.tables.get(c)?, self.tables.get(p)?);
        let (child_t, parent_t) = (
            diagram.model.tables.get(ct.table)?,
            diagram.model.tables.get(pt.table)?,
        );
        let y1 = ct.rect.top()
            + ct.geom
                .anchor_y(child_t, rel.from_columns.first().map(String::as_str));
        let y2 = pt.rect.top()
            + pt.geom
                .anchor_y(parent_t, rel.to_columns.first().map(String::as_str));
        Some(if c == p {
            self_route(ct.rect, y1, y2, lane)
        } else {
            let base = route(ct.rect, y1, pt.rect, y2, lane);
            let side_by_side = ct.rect.left() >= pt.rect.right() + STUB * 2.0
                || pt.rect.left() >= ct.rect.right() + STUB * 2.0;
            // deterministic spread so detours of different relations do not coincide
            let spread = (ri % 7) as f32 * 3.0;
            if side_by_side {
                avoid_obstacles(base, ct.rect, pt.rect, lane, spread, index, [c, p])
            } else {
                base
            }
        })
    }

    /// Fast update while boxes are dragged: moves the given tables to their
    /// current positions and re-routes only the relations attached to them.
    /// Other routes keep their shape until the next full [`Self::build`].
    pub fn update_moved(&mut self, diagram: &ErDiagram, keys: &[String]) {
        let mut moved = Vec::new();
        for k in keys {
            if let (Some(&i), Some(pos)) = (self.index.get(k), diagram.positions.get(k))
                && let Some(st) = self.tables.get_mut(i)
            {
                st.rect = Rect::from_min_size(*pos, st.geom.size);
                moved.push(i);
            }
        }
        if moved.is_empty() {
            return;
        }
        let index = RectIndex::new(self.tables.iter().map(|t| t.rect).collect());
        let updates: Vec<(usize, Route)> = self
            .relations
            .iter()
            .enumerate()
            .filter(|(_, sr)| moved.contains(&sr.child) || moved.contains(&sr.parent))
            .filter_map(|(k, sr)| {
                self.route_one(diagram, sr.relation, sr.child, sr.parent, sr.lane, &index)
                    .map(|r| (k, r))
            })
            .collect();
        for (k, r) in updates {
            if let Some(sr) = self.relations.get_mut(k) {
                sr.route = r;
            }
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

    #[test]
    fn long_routes_avoid_boxes_in_between() {
        // parent | obstacle column | child; the direct route crosses the obstacle
        let parent = r(0.0, 0.0, 100.0, 300.0);
        let obstacle = r(250.0, 0.0, 120.0, 300.0);
        let below = r(250.0, 340.0, 120.0, 100.0);
        let child = r(500.0, 0.0, 100.0, 300.0);
        let index = RectIndex::new(vec![parent, child, obstacle, below]);
        let base = route(child, 150.0, parent, 150.0, 0.0);
        assert!(
            index.blocked(&base.points, [0, 1]),
            "test setup: direct route is blocked"
        );
        let routed = avoid_obstacles(base, child, parent, 0.0, 0.0, &index, [0, 1]);
        assert!(
            !index.blocked(&routed.points, [0, 1]),
            "{:?}",
            routed.points
        );
        assert_eq!(routed.points.first(), Some(&pos2(500.0, 150.0)));
        assert_eq!(routed.points.last(), Some(&pos2(100.0, 150.0)));
        for w in routed.points.windows(2) {
            assert!(
                w[0].x == w[1].x || w[0].y == w[1].y,
                "orthogonal: {:?}",
                routed.points
            );
        }
        // corridor between the two obstacles (y 300..340) or around them
        let corridor_y = routed
            .points
            .iter()
            .map(|p| p.y)
            .find(|y| *y != 150.0)
            .unwrap();
        assert!(
            corridor_y < 0.0 || (300.0..=340.0).contains(&corridor_y) || corridor_y > 440.0,
            "{corridor_y}"
        );
        // unobstructed routes are kept as they are
        let free = RectIndex::new(vec![parent, child]);
        let base = route(child, 150.0, parent, 150.0, 0.0);
        assert_eq!(
            avoid_obstacles(base.clone(), child, parent, 0.0, 0.0, &free, [0, 1]),
            base
        );
        assert_eq!(
            simplify(vec![
                pos2(0.0, 0.0),
                pos2(0.0, 0.0),
                pos2(5.0, 0.0),
                pos2(9.0, 0.0),
                pos2(9.0, 4.0)
            ])
            .len(),
            3
        );
    }
}
