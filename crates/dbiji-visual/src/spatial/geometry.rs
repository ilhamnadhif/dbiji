//! Simple feature geometry model (OGC types with optional Z/M and SRID).

use std::fmt::Write as _;

/// A coordinate with optional Z and M values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    /// X (longitude for geographic data).
    pub x: f64,
    /// Y (latitude for geographic data).
    pub y: f64,
    /// Elevation.
    pub z: Option<f64>,
    /// Measure.
    pub m: Option<f64>,
}

impl Coord {
    /// 2D coordinate.
    pub fn xy(x: f64, y: f64) -> Self {
        Coord { x, y, z: None, m: None }
    }
}

/// Coordinate dimensions of a geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dims {
    /// X Y
    #[default]
    Xy,
    /// X Y Z
    Xyz,
    /// X Y M
    Xym,
    /// X Y Z M
    Xyzm,
}

impl Dims {
    /// From Z / M flags.
    pub fn from_flags(z: bool, m: bool) -> Dims {
        match (z, m) {
            (false, false) => Dims::Xy,
            (true, false) => Dims::Xyz,
            (false, true) => Dims::Xym,
            (true, true) => Dims::Xyzm,
        }
    }

    /// Has a Z value.
    pub fn has_z(self) -> bool {
        matches!(self, Dims::Xyz | Dims::Xyzm)
    }

    /// Has an M value.
    pub fn has_m(self) -> bool {
        matches!(self, Dims::Xym | Dims::Xyzm)
    }

    /// Numbers per coordinate.
    pub fn count(self) -> usize {
        2 + self.has_z() as usize + self.has_m() as usize
    }

    fn wkt_suffix(self) -> &'static str {
        match self {
            Dims::Xy => "",
            Dims::Xyz => " Z",
            Dims::Xym => " M",
            Dims::Xyzm => " ZM",
        }
    }
}

/// Geometry body.
#[derive(Debug, Clone, PartialEq)]
pub enum Geom {
    /// Point (`None` = `POINT EMPTY`).
    Point(Option<Coord>),
    /// Line string.
    LineString(Vec<Coord>),
    /// Polygon: exterior ring followed by holes (rings repeat the first point).
    Polygon(Vec<Vec<Coord>>),
    /// Multi point.
    MultiPoint(Vec<Coord>),
    /// Multi line string.
    MultiLineString(Vec<Vec<Coord>>),
    /// Multi polygon.
    MultiPolygon(Vec<Vec<Vec<Coord>>>),
    /// Collection of geometries.
    GeometryCollection(Vec<Geom>),
}

/// A geometry with its spatial reference id.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    /// SRID from EWKT (`SRID=4326;`) or EWKB.
    pub srid: Option<i32>,
    /// Coordinate dimensions.
    pub dims: Dims,
    /// The shape.
    pub geom: Geom,
}

/// Axis aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// Smallest x.
    pub min_x: f64,
    /// Smallest y.
    pub min_y: f64,
    /// Largest x.
    pub max_x: f64,
    /// Largest y.
    pub max_y: f64,
}

impl Bounds {
    /// Box of one point.
    pub fn point(x: f64, y: f64) -> Bounds {
        Bounds { min_x: x, min_y: y, max_x: x, max_y: y }
    }

    /// Smallest box containing both.
    pub fn union(self, o: Bounds) -> Bounds {
        Bounds {
            min_x: self.min_x.min(o.min_x),
            min_y: self.min_y.min(o.min_y),
            max_x: self.max_x.max(o.max_x),
            max_y: self.max_y.max(o.max_y),
        }
    }

    /// Adds a point.
    pub fn extend(self, x: f64, y: f64) -> Bounds {
        self.union(Bounds::point(x, y))
    }

    /// Width.
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Height.
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Centre.
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) * 0.5, (self.min_y + self.max_y) * 0.5)
    }

    /// Grows the box by `d` on every side.
    pub fn expand(self, d: f64) -> Bounds {
        Bounds { min_x: self.min_x - d, min_y: self.min_y - d, max_x: self.max_x + d, max_y: self.max_y + d }
    }

    /// Boxes overlap (touching counts).
    pub fn intersects(&self, o: &Bounds) -> bool {
        self.min_x <= o.max_x && o.min_x <= self.max_x && self.min_y <= o.max_y && o.min_y <= self.max_y
    }

    /// Point inside (inclusive).
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}

fn bounds_of<'a>(coords: impl IntoIterator<Item = &'a Coord>) -> Option<Bounds> {
    let mut out: Option<Bounds> = None;
    for c in coords {
        if !c.x.is_finite() || !c.y.is_finite() {
            continue;
        }
        out = Some(match out {
            None => Bounds::point(c.x, c.y),
            Some(b) => b.extend(c.x, c.y),
        });
    }
    out
}

fn merge(a: Option<Bounds>, b: Option<Bounds>) -> Option<Bounds> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

impl Geom {
    /// Bounding box (`None` for empty geometries).
    pub fn bounds(&self) -> Option<Bounds> {
        match self {
            Geom::Point(p) => p.as_ref().and_then(|c| bounds_of([c])),
            Geom::LineString(cs) | Geom::MultiPoint(cs) => bounds_of(cs),
            Geom::Polygon(rings) | Geom::MultiLineString(rings) => bounds_of(rings.iter().flatten()),
            Geom::MultiPolygon(polys) => bounds_of(polys.iter().flatten().flatten()),
            Geom::GeometryCollection(gs) => gs.iter().fold(None, |acc, g| merge(acc, g.bounds())),
        }
    }

    /// OGC type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Geom::Point(_) => "POINT",
            Geom::LineString(_) => "LINESTRING",
            Geom::Polygon(_) => "POLYGON",
            Geom::MultiPoint(_) => "MULTIPOINT",
            Geom::MultiLineString(_) => "MULTILINESTRING",
            Geom::MultiPolygon(_) => "MULTIPOLYGON",
            Geom::GeometryCollection(_) => "GEOMETRYCOLLECTION",
        }
    }

    /// No coordinates at all.
    pub fn is_empty(&self) -> bool {
        self.num_points() == 0
    }

    /// Number of coordinates.
    pub fn num_points(&self) -> usize {
        match self {
            Geom::Point(p) => p.is_some() as usize,
            Geom::LineString(cs) | Geom::MultiPoint(cs) => cs.len(),
            Geom::Polygon(rings) | Geom::MultiLineString(rings) => rings.iter().map(Vec::len).sum(),
            Geom::MultiPolygon(polys) => polys.iter().flatten().map(Vec::len).sum(),
            Geom::GeometryCollection(gs) => gs.iter().map(Geom::num_points).sum(),
        }
    }

    fn write_wkt(&self, dims: Dims, out: &mut String) {
        out.push_str(self.type_name());
        out.push_str(dims.wkt_suffix());
        if self.is_empty() && !matches!(self, Geom::GeometryCollection(gs) if !gs.is_empty()) {
            out.push_str(" EMPTY");
            return;
        }
        if dims != Dims::Xy {
            out.push(' ');
        }
        match self {
            Geom::Point(p) => {
                out.push('(');
                if let Some(c) = p {
                    write_coord(c, dims, out);
                }
                out.push(')');
            }
            Geom::LineString(cs) => write_list(cs, dims, out),
            Geom::Polygon(rings) => write_rings(rings, dims, out),
            Geom::MultiPoint(cs) => {
                out.push('(');
                for (i, c) in cs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('(');
                    write_coord(c, dims, out);
                    out.push(')');
                }
                out.push(')');
            }
            Geom::MultiLineString(lines) => write_rings(lines, dims, out),
            Geom::MultiPolygon(polys) => {
                out.push('(');
                for (i, p) in polys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_rings(p, dims, out);
                }
                out.push(')');
            }
            Geom::GeometryCollection(gs) => {
                out.push('(');
                for (i, g) in gs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    g.write_wkt(dims, out);
                }
                out.push(')');
            }
        }
    }
}

fn write_num(v: f64, out: &mut String) {
    let _ = write!(out, "{v}");
}

fn write_coord(c: &Coord, dims: Dims, out: &mut String) {
    write_num(c.x, out);
    out.push(' ');
    write_num(c.y, out);
    if dims.has_z() {
        out.push(' ');
        write_num(c.z.unwrap_or(0.0), out);
    }
    if dims.has_m() {
        out.push(' ');
        write_num(c.m.unwrap_or(0.0), out);
    }
}

fn write_list(cs: &[Coord], dims: Dims, out: &mut String) {
    out.push('(');
    for (i, c) in cs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_coord(c, dims, out);
    }
    out.push(')');
}

fn write_rings(rings: &[Vec<Coord>], dims: Dims, out: &mut String) {
    out.push('(');
    for (i, r) in rings.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_list(r, dims, out);
    }
    out.push(')');
}

impl Geometry {
    /// 2D geometry without SRID.
    pub fn new(geom: Geom) -> Self {
        Geometry { srid: None, dims: Dims::Xy, geom }
    }

    /// Bounding box in x/y (`None` when empty).
    pub fn bounds(&self) -> Option<Bounds> {
        self.geom.bounds()
    }

    /// OGC type name.
    pub fn type_name(&self) -> &'static str {
        self.geom.type_name()
    }

    /// No coordinates.
    pub fn is_empty(&self) -> bool {
        self.geom.is_empty()
    }

    /// Number of coordinates.
    pub fn num_points(&self) -> usize {
        self.geom.num_points()
    }

    /// (E)WKT text: `SRID=n;` prefix when an SRID is known, ISO dimension
    /// keywords (`POINT Z (1 2 3)`).
    pub fn to_wkt(&self) -> String {
        let mut out = String::new();
        if let Some(s) = self.srid {
            let _ = write!(out, "SRID={s};");
        }
        self.geom.write_wkt(self.dims, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkt_output_and_bounds() {
        let g = Geometry {
            srid: Some(4326),
            dims: Dims::Xy,
            geom: Geom::Polygon(vec![vec![Coord::xy(0.0, 0.0), Coord::xy(4.0, 0.0), Coord::xy(4.0, 3.5), Coord::xy(0.0, 0.0)]]),
        };
        assert_eq!(g.to_wkt(), "SRID=4326;POLYGON((0 0,4 0,4 3.5,0 0))");
        assert_eq!(g.bounds(), Some(Bounds { min_x: 0.0, min_y: 0.0, max_x: 4.0, max_y: 3.5 }));
        assert_eq!(Geometry::new(Geom::Point(None)).to_wkt(), "POINT EMPTY");
        let z = Geometry { srid: None, dims: Dims::Xyz, geom: Geom::Point(Some(Coord { x: 1.0, y: 2.0, z: Some(3.0), m: None })) };
        assert_eq!(z.to_wkt(), "POINT Z (1 2 3)");
        let c = Geometry::new(Geom::GeometryCollection(vec![Geom::Point(Some(Coord::xy(1.0, 1.0))), Geom::LineString(vec![])]));
        assert_eq!(c.to_wkt(), "GEOMETRYCOLLECTION(POINT(1 1),LINESTRING EMPTY)");
        assert_eq!(c.num_points(), 1);
        assert!(Geometry::new(Geom::MultiPolygon(vec![])).bounds().is_none());
        assert_eq!(Dims::from_flags(true, true).count(), 4);
    }
}
