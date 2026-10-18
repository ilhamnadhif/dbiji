//! Polygon triangulation by ear clipping. Holes are first merged into the
//! outer ring with bridge edges (the hole's rightmost vertex is connected to
//! a mutually visible vertex found by casting a ray to +x), then ears are
//! clipped from the resulting simple polygon. Collinear vertices are
//! dropped; self-intersecting input that cannot be clipped returns `None`
//! (callers then draw the outline only).

/// A 2D point.
pub type Point = [f64; 2];

/// Triangles over a vertex list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Triangulation {
    /// Vertices (outer ring followed by the hole rings).
    pub vertices: Vec<Point>,
    /// Counter-clockwise triangles as indices into `vertices`.
    pub triangles: Vec<[u32; 3]>,
}

impl Triangulation {
    /// Total area of the triangles.
    pub fn area(&self) -> f64 {
        self.triangles
            .iter()
            .map(|t| {
                let p = |i: u32| self.vertices.get(i as usize).copied().unwrap_or([0.0, 0.0]);
                cross(p(t[0]), p(t[1]), p(t[2])).abs() * 0.5
            })
            .sum()
    }
}

/// Polygons with more vertices than this are not triangulated.
pub const MAX_VERTICES: usize = 50_000;

/// Twice the signed area of triangle a, b, c (> 0 when counter-clockwise).
fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0])
}

/// Signed area of a ring (> 0 when counter-clockwise with y up).
pub fn signed_area(ring: &[Point]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        s += a[0] * b[1] - b[0] * a[1];
    }
    s * 0.5
}

/// Drops non-finite points, consecutive duplicates and the closing point.
fn clean_ring(ring: &[Point]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(ring.len());
    for p in ring {
        if !p[0].is_finite() || !p[1].is_finite() {
            continue;
        }
        if out.last() != Some(p) {
            out.push(*p);
        }
    }
    while out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

fn inside_triangle(p: Point, a: Point, b: Point, c: Point, strict: bool) -> bool {
    let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    if strict {
        d1 > 0.0 && d2 > 0.0 && d3 > 0.0
    } else {
        d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0
    }
}

/// Triangulates a polygon given by its outer ring and holes (any
/// orientation, closing point optional).
pub fn triangulate(outer: &[Point], holes: &[Vec<Point>]) -> Option<Triangulation> {
    let mut outer = clean_ring(outer);
    if outer.len() < 3 {
        return None;
    }
    let area = signed_area(&outer);
    if area == 0.0 || !area.is_finite() {
        return None;
    }
    if area < 0.0 {
        outer.reverse();
    }
    let mut rings: Vec<Vec<Point>> = Vec::new();
    for h in holes {
        let mut h = clean_ring(h);
        if h.len() < 3 {
            continue;
        }
        let a = signed_area(&h);
        if a == 0.0 || !a.is_finite() {
            continue;
        }
        if a > 0.0 {
            h.reverse();
        }
        rings.push(h);
    }
    let total: usize = outer.len() + rings.iter().map(Vec::len).sum::<usize>();
    if total > MAX_VERTICES {
        return None;
    }
    let mut vertices: Vec<Point> = outer.clone();
    let mut poly: Vec<usize> = (0..outer.len()).collect();
    // rightmost holes first
    let mut hole_order: Vec<usize> = (0..rings.len()).collect();
    let max_x = |r: &Vec<Point>| r.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
    hole_order.sort_by(|&a, &b| max_x(&rings[b]).total_cmp(&max_x(&rings[a])));
    for hi in hole_order {
        let hole = &rings[hi];
        let base = vertices.len();
        vertices.extend_from_slice(hole);
        let Some(mi) = (0..hole.len()).max_by(|&a, &b| {
            hole[a][0]
                .total_cmp(&hole[b][0])
                .then(hole[b][1].total_cmp(&hole[a][1]))
        }) else {
            continue;
        };
        let Some(j) = find_bridge(&vertices, &poly, hole[mi]) else {
            continue;
        };
        let mut merged = Vec::with_capacity(poly.len() + hole.len() + 2);
        merged.extend_from_slice(&poly[..=j]);
        for k in 0..hole.len() {
            merged.push(base + (mi + k) % hole.len());
        }
        merged.push(base + mi);
        merged.push(poly[j]);
        merged.extend_from_slice(&poly[j + 1..]);
        poly = merged;
    }
    let triangles = clip_ears(&vertices, &poly)?;
    Some(Triangulation {
        vertices,
        triangles: triangles.into_iter().map(|t| t.map(|i| i as u32)).collect(),
    })
}

/// Index into `poly` of a vertex visible from `m` (a hole's rightmost point).
fn find_bridge(vertices: &[Point], poly: &[usize], m: Point) -> Option<usize> {
    let n = poly.len();
    let mut best: Option<(f64, usize)> = None;
    for i in 0..n {
        let (a, b) = (vertices[poly[i]], vertices[poly[(i + 1) % n]]);
        if a[1] == b[1] {
            if a[1] == m[1] {
                for (x, k) in [(a[0], i), (b[0], (i + 1) % n)] {
                    if x >= m[0] && best.is_none_or(|(bx, _)| x < bx) {
                        best = Some((x, k));
                    }
                }
            }
            continue;
        }
        if (a[1] - m[1]) * (b[1] - m[1]) > 0.0 {
            continue;
        }
        let t = (m[1] - a[1]) / (b[1] - a[1]);
        let x = a[0] + t * (b[0] - a[0]);
        if x >= m[0] && best.is_none_or(|(bx, _)| x < bx) {
            // the visible candidate: the edge endpoint with the larger x
            let k = if x == a[0] && m[1] == a[1] {
                i
            } else if x == b[0] && m[1] == b[1] {
                (i + 1) % n
            } else if a[0] > b[0] {
                i
            } else {
                (i + 1) % n
            };
            best = Some((x, k));
        }
    }
    let (ix, mut p) = best?;
    let pp = vertices[poly[p]];
    if pp[0] == ix && pp[1] == m[1] {
        return Some(pick_copy(vertices, poly, p, m));
    }
    // reflex vertices inside triangle (M, I, P) block the bridge; take the
    // one with the smallest angle to the ray instead
    let i_pt = [ix, m[1]];
    let mut best_tan = f64::INFINITY;
    let mut best_dist = f64::INFINITY;
    for k in 0..n {
        if k == p {
            continue;
        }
        let r = vertices[poly[k]];
        let prev = vertices[poly[(k + n - 1) % n]];
        let next = vertices[poly[(k + 1) % n]];
        if cross(prev, r, next) >= 0.0 {
            continue; // convex
        }
        let (a, b, c) = if m[1] < pp[1] {
            (m, i_pt, pp)
        } else {
            (m, pp, i_pt)
        };
        let ccw = cross(a, b, c) > 0.0;
        let inside = if ccw {
            inside_triangle(r, a, b, c, false)
        } else {
            inside_triangle(r, a, c, b, false)
        };
        if !inside || r[0] <= m[0] {
            continue;
        }
        let tan = (r[1] - m[1]).abs() / (r[0] - m[0]);
        let dist = (r[0] - m[0]).powi(2) + (r[1] - m[1]).powi(2);
        if tan < best_tan || (tan == best_tan && dist < best_dist) {
            best_tan = tan;
            best_dist = dist;
            p = k;
        }
    }
    Some(pick_copy(vertices, poly, p, m))
}

/// After earlier bridges a vertex can occur several times in `poly`; the
/// new bridge must start at the occurrence whose interior wedge contains
/// the direction towards `m`, otherwise the polygon stops being simple.
fn pick_copy(vertices: &[Point], poly: &[usize], k: usize, m: Point) -> usize {
    let n = poly.len();
    let target = vertices[poly[k]];
    let locally_inside = |j: usize| {
        let (a, p, b) = (
            vertices[poly[(j + n - 1) % n]],
            vertices[poly[j]],
            vertices[poly[(j + 1) % n]],
        );
        if cross(a, p, b) >= 0.0 {
            cross(a, p, m) >= 0.0 && cross(p, b, m) >= 0.0
        } else {
            cross(a, p, m) >= 0.0 || cross(p, b, m) >= 0.0
        }
    };
    if locally_inside(k) {
        return k;
    }
    (0..n)
        .find(|&j| j != k && vertices[poly[j]] == target && locally_inside(j))
        .unwrap_or(k)
}

/// Ear clipping of a simple counter-clockwise polygon given as indices.
fn clip_ears(vertices: &[Point], poly: &[usize]) -> Option<Vec<[usize; 3]>> {
    let n = poly.len();
    if n < 3 {
        return None;
    }
    let pt = |k: usize| vertices[poly[k]];
    let scale = poly
        .iter()
        .map(|&i| vertices[i][0].abs().max(vertices[i][1].abs()))
        .fold(1e-300, f64::max);
    let eps = scale * scale * 1e-14;
    let mut prev: Vec<usize> = (0..n).map(|i| (i + n - 1) % n).collect();
    let mut next: Vec<usize> = (0..n).map(|i| (i + 1) % n).collect();
    let mut alive = n;
    let mut out = Vec::with_capacity(n.saturating_sub(2));
    let mut i = 0usize;
    let mut failures = 0usize;
    let mut mode = 0u8; // 0: strict, 1: ignore points on edges, 2: any convex vertex
    let remove = |k: usize, prev: &mut Vec<usize>, next: &mut Vec<usize>| {
        let (a, b) = (prev[k], next[k]);
        next[a] = b;
        prev[b] = a;
    };
    while alive > 3 {
        let (a, b) = (prev[i], next[i]);
        let (pa, pv, pb) = (pt(a), pt(i), pt(b));
        let c = cross(pa, pv, pb);
        if c.abs() <= eps {
            // collinear or duplicate: drop without a triangle
            remove(i, &mut prev, &mut next);
            alive -= 1;
            i = a;
            failures = 0;
            continue;
        }
        let ear = c > 0.0 && {
            if mode >= 2 {
                true
            } else {
                let mut blocked = false;
                let mut k = next[b];
                while k != a {
                    let r = pt(k);
                    let reflex = cross(pt(prev[k]), r, pt(next[k])) <= 0.0;
                    if reflex
                        && r != pa
                        && r != pv
                        && r != pb
                        && inside_triangle(r, pa, pv, pb, mode == 1)
                    {
                        blocked = true;
                        break;
                    }
                    k = next[k];
                }
                !blocked
            }
        };
        if ear {
            out.push([poly[a], poly[i], poly[b]]);
            remove(i, &mut prev, &mut next);
            alive -= 1;
            i = b;
            failures = 0;
            mode = 0;
        } else {
            i = next[i];
            failures += 1;
            if failures > alive {
                if mode >= 2 {
                    return None;
                }
                mode += 1;
                failures = 0;
            }
        }
    }
    let (a, b) = (prev[i], next[i]);
    if cross(pt(a), pt(i), pt(b)).abs() > eps {
        out.push([poly[a], poly[i], poly[b]]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(outer: &[Point], holes: &[Vec<Point>]) -> Triangulation {
        let t = triangulate(outer, holes)
            .unwrap_or_else(|| panic!("triangulation failed for {outer:?} with holes {holes:?}"));
        let expected =
            signed_area(outer).abs() - holes.iter().map(|h| signed_area(h).abs()).sum::<f64>();
        assert!(
            (t.area() - expected).abs() <= expected.abs() * 1e-9 + 1e-9,
            "area {} vs {}",
            t.area(),
            expected
        );
        for tri in &t.triangles {
            let p = |i: u32| t.vertices[i as usize];
            assert!(
                cross(p(tri[0]), p(tri[1]), p(tri[2])) > 0.0,
                "counter-clockwise, non-degenerate"
            );
        }
        t
    }

    #[test]
    fn convex_and_concave() {
        let square = [
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        assert_eq!(check(&square, &[]).triangles.len(), 2);
        // clockwise input
        let cw: Vec<Point> = square.iter().rev().copied().collect();
        check(&cw, &[]);
        // L shape
        check(
            &[
                [0.0, 0.0],
                [6.0, 0.0],
                [6.0, 2.0],
                [2.0, 2.0],
                [2.0, 6.0],
                [0.0, 6.0],
            ],
            &[],
        );
        // star
        let star: Vec<Point> = (0..10)
            .map(|i| {
                let a = i as f64 * std::f64::consts::PI / 5.0;
                let r = if i % 2 == 0 { 10.0 } else { 4.0 };
                [r * a.cos(), r * a.sin()]
            })
            .collect();
        assert_eq!(check(&star, &[]).triangles.len(), 8);
        // comb (many reflex vertices)
        let mut comb = vec![[0.0, 0.0], [20.0, 0.0], [20.0, 10.0]];
        for k in (0..10).rev() {
            comb.push([k as f64 * 2.0 + 1.5, 10.0]);
            comb.push([k as f64 * 2.0 + 1.0, 3.0]);
            comb.push([k as f64 * 2.0 + 0.5, 10.0]);
        }
        comb.push([0.0, 10.0]);
        check(&comb, &[]);
    }

    #[test]
    fn holes_via_bridges() {
        let outer = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let hole = vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]];
        check(&outer, std::slice::from_ref(&hole));
        // two holes, one shadowing the other along the ray
        let h2 = vec![[1.0, 4.0], [2.0, 4.0], [2.0, 6.0], [1.0, 6.0]];
        check(&outer, &[hole.clone(), h2.clone()]);
        check(&outer, &[h2, hole]);
        // hole whose rightmost vertex is level with an outer vertex
        let outer2 = [
            [0.0, 0.0],
            [10.0, 0.0],
            [12.0, 5.0],
            [10.0, 10.0],
            [0.0, 10.0],
        ];
        check(&outer2, &[vec![[4.0, 4.0], [8.0, 5.0], [4.0, 6.0]]]);
        // many small holes in a grid
        let holes: Vec<Vec<Point>> = (0..4)
            .flat_map(|i| {
                (0..4).map(move |j| {
                    let (x, y) = (1.0 + i as f64 * 2.2, 1.0 + j as f64 * 2.2);
                    vec![[x, y], [x + 1.0, y], [x + 1.0, y + 1.0], [x, y + 1.0]]
                })
            })
            .collect();
        check(&outer, &holes);
        // comb shaped outer: the reflex corners between the teeth lie in the
        // triangle of the direct bridge, so the bridge goes to one of them
        let comb = [
            [0.0, 0.0],
            [12.0, 0.0],
            [12.0, 6.0],
            [10.0, 6.0],
            [10.0, 3.0],
            [8.0, 3.0],
            [8.0, 6.0],
            [6.0, 6.0],
            [6.0, 3.0],
            [4.0, 3.0],
            [4.0, 6.0],
            [0.0, 6.0],
        ];
        check(
            &comb,
            &[vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0]]],
        );
        check(
            &comb,
            &[
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0]],
                vec![[1.0, 4.0], [3.0, 4.0], [3.0, 5.0]],
            ],
        );
    }

    #[test]
    fn degenerate_input() {
        assert!(triangulate(&[], &[]).is_none());
        assert!(triangulate(&[[0.0, 0.0], [1.0, 1.0]], &[]).is_none());
        assert!(
            triangulate(&[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]], &[]).is_none(),
            "collinear"
        );
        assert!(triangulate(&[[0.0, 0.0], [f64::NAN, 1.0], [1.0, 0.0]], &[]).is_none());
        // collinear points along edges and duplicates are tolerated
        let t = check(
            &[
                [0.0, 0.0],
                [5.0, 0.0],
                [5.0, 0.0],
                [10.0, 0.0],
                [10.0, 5.0],
                [10.0, 10.0],
                [0.0, 10.0],
                [0.0, 5.0],
            ],
            &[],
        );
        assert!(t.triangles.len() >= 2);
        // self intersecting bow tie never panics
        let _ = triangulate(&[[0.0, 0.0], [10.0, 10.0], [10.0, 0.0], [0.0, 10.0]], &[]);
        // degenerate holes are ignored
        check(
            &[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            &[vec![[1.0, 1.0], [2.0, 2.0]]],
        );
    }

    #[test]
    fn large_circle_is_fast_enough() {
        let n = 4000;
        let ring: Vec<Point> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                [a.cos() * 100.0, a.sin() * 100.0]
            })
            .collect();
        let t = check(&ring, &[]);
        assert_eq!(t.triangles.len(), n - 2);
    }

    #[test]
    fn subsets_of_a_hole_grid() {
        // holes sharing x/y coordinates exercise repeated bridge vertices
        let outer = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let all: Vec<Vec<Point>> = (0..4)
            .flat_map(|i| {
                (0..4).map(move |j| {
                    let (x, y) = (1.0 + i as f64 * 2.2, 1.0 + j as f64 * 2.2);
                    vec![[x, y], [x + 1.0, y], [x + 1.0, y + 1.0], [x, y + 1.0]]
                })
            })
            .collect();
        for mask in (0u32..(1 << 16)).step_by(37) {
            let holes: Vec<Vec<Point>> = (0..16)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| all[i].clone())
                .collect();
            check(&outer, &holes);
        }
    }
}
