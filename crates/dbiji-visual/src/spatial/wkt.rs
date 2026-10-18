//! (E)WKT parser: `SRID=n;` prefix, all seven OGC types, Z/M/ZM in ISO
//! (`POINT Z (..)`) and PostGIS (`POINTM(..)`) spelling or inferred from the
//! number of ordinates, `EMPTY`, any case and whitespace.

use super::geometry::{Coord, Dims, Geom, Geometry};
use dbiji_core::trf;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Num(f64),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Eq,
}

fn tokenize(input: &str) -> Result<Vec<(usize, Tok)>, String> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' | '[' => {
                out.push((pos, Tok::LParen));
                i += 1;
            }
            ')' | ']' => {
                out.push((pos, Tok::RParen));
                i += 1;
            }
            ',' => {
                out.push((pos, Tok::Comma));
                i += 1;
            }
            ';' => {
                out.push((pos, Tok::Semicolon));
                i += 1;
            }
            '=' => {
                out.push((pos, Tok::Eq));
                i += 1;
            }
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => {
                let start = i;
                i += 1;
                while i < chars.len() {
                    let d = chars[i].1;
                    let prev = chars[i - 1].1;
                    if d.is_ascii_digit() || d == '.' || d == 'e' || d == 'E' || ((d == '-' || d == '+') && (prev == 'e' || prev == 'E')) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let end = chars.get(i).map(|(p, _)| *p).unwrap_or(input.len());
                let text = &input[pos..end];
                let v: f64 = text.parse().map_err(|_| trf("Invalid number '{}' at position {}", &[&text, &(start + 1)]))?;
                if !v.is_finite() {
                    return Err(trf("Invalid number '{}' at position {}", &[&text, &(start + 1)]));
                }
                out.push((pos, Tok::Num(v)));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = pos;
                while i < chars.len() && (chars[i].1.is_alphanumeric() || chars[i].1 == '_') {
                    i += 1;
                }
                let end = chars.get(i).map(|(p, _)| *p).unwrap_or(input.len());
                out.push((start, Tok::Word(input[start..end].to_ascii_uppercase())));
            }
            other => return Err(trf("Unexpected character '{}' at position {}", &[&other, &(pos + 1)])),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    Collection,
}

fn split_type(word: &str) -> Option<(Kind, Option<Dims>)> {
    const TYPES: [(&str, Kind); 7] = [
        ("GEOMETRYCOLLECTION", Kind::Collection),
        ("MULTILINESTRING", Kind::MultiLineString),
        ("MULTIPOLYGON", Kind::MultiPolygon),
        ("MULTIPOINT", Kind::MultiPoint),
        ("LINESTRING", Kind::LineString),
        ("POLYGON", Kind::Polygon),
        ("POINT", Kind::Point),
    ];
    for (name, kind) in TYPES {
        if let Some(rest) = word.strip_prefix(name) {
            let dims = match rest {
                "" => None,
                "Z" => Some(Dims::Xyz),
                "M" => Some(Dims::Xym),
                "ZM" => Some(Dims::Xyzm),
                _ => continue,
            };
            return Some((kind, dims));
        }
    }
    None
}

const MAX_DEPTH: usize = 32;

struct Parser {
    toks: Vec<(usize, Tok)>,
    pos: usize,
    /// Dimensions of the whole geometry: explicit, or inferred from the first coordinate.
    dims: Option<Dims>,
    explicit: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(_, t)| t)
    }

    fn at(&self) -> usize {
        self.toks.get(self.pos).map(|(p, _)| *p + 1).unwrap_or_else(|| self.toks.last().map(|(p, _)| *p + 2).unwrap_or(1))
    }

    fn err(&self, expected: &str) -> String {
        match self.peek() {
            Some(t) => trf("Invalid WKT at position {}: expected {}, found {}", &[&self.at(), &expected, &describe(t)]),
            None => trf("Invalid WKT: expected {} but the text ended", &[&expected]),
        }
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(_, t)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, tok: Tok, what: &str) -> Result<(), String> {
        if self.peek() == Some(&tok) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(what))
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(x)) if x == w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn geometry(&mut self, depth: usize) -> Result<Geom, String> {
        if depth > MAX_DEPTH {
            return Err(trf("Geometry nesting deeper than {} levels", &[&MAX_DEPTH]));
        }
        let word = match self.peek() {
            Some(Tok::Word(w)) => w.clone(),
            _ => return Err(self.err("a geometry type")),
        };
        let Some((kind, fused)) = split_type(&word) else {
            return Err(trf("Unsupported geometry type '{}' at position {}", &[&word, &self.at()]));
        };
        self.pos += 1;
        let mut declared = fused;
        if let Some(Tok::Word(w)) = self.peek() {
            let d = match w.as_str() {
                "Z" => Some(Dims::Xyz),
                "M" => Some(Dims::Xym),
                "ZM" => Some(Dims::Xyzm),
                _ => None,
            };
            if d.is_some() {
                declared = d;
                self.pos += 1;
            }
        }
        if let Some(d) = declared {
            match self.dims {
                Some(existing) if self.explicit && existing != d => {
                    return Err(trf("Mixed coordinate dimensions at position {}", &[&self.at()]));
                }
                _ => {
                    self.dims = Some(d);
                    self.explicit = true;
                }
            }
        }
        if self.eat_word("EMPTY") {
            return Ok(match kind {
                Kind::Point => Geom::Point(None),
                Kind::LineString => Geom::LineString(Vec::new()),
                Kind::Polygon => Geom::Polygon(Vec::new()),
                Kind::MultiPoint => Geom::MultiPoint(Vec::new()),
                Kind::MultiLineString => Geom::MultiLineString(Vec::new()),
                Kind::MultiPolygon => Geom::MultiPolygon(Vec::new()),
                Kind::Collection => Geom::GeometryCollection(Vec::new()),
            });
        }
        Ok(match kind {
            Kind::Point => {
                self.expect(Tok::LParen, "'('")?;
                let c = self.coord()?;
                self.expect(Tok::RParen, "')'")?;
                Geom::Point(Some(c))
            }
            Kind::LineString => Geom::LineString(self.coord_list()?),
            Kind::Polygon => Geom::Polygon(self.ring_list()?),
            Kind::MultiPoint => {
                self.expect(Tok::LParen, "'('")?;
                let mut pts = Vec::new();
                loop {
                    if self.eat_word("EMPTY") {
                        // empty member points are skipped
                    } else if self.peek() == Some(&Tok::LParen) {
                        self.pos += 1;
                        if !self.eat_word("EMPTY") {
                            pts.push(self.coord()?);
                        }
                        self.expect(Tok::RParen, "')'")?;
                    } else {
                        pts.push(self.coord()?);
                    }
                    if self.peek() == Some(&Tok::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' or ','")?;
                Geom::MultiPoint(pts)
            }
            Kind::MultiLineString => {
                self.expect(Tok::LParen, "'('")?;
                let mut lines = Vec::new();
                loop {
                    if !self.eat_word("EMPTY") {
                        lines.push(self.coord_list()?);
                    }
                    if self.peek() == Some(&Tok::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' or ','")?;
                Geom::MultiLineString(lines)
            }
            Kind::MultiPolygon => {
                self.expect(Tok::LParen, "'('")?;
                let mut polys = Vec::new();
                loop {
                    if !self.eat_word("EMPTY") {
                        polys.push(self.ring_list()?);
                    }
                    if self.peek() == Some(&Tok::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' or ','")?;
                Geom::MultiPolygon(polys)
            }
            Kind::Collection => {
                self.expect(Tok::LParen, "'('")?;
                let mut gs = Vec::new();
                loop {
                    gs.push(self.geometry(depth + 1)?);
                    if self.peek() == Some(&Tok::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' or ','")?;
                Geom::GeometryCollection(gs)
            }
        })
    }

    fn coord(&mut self) -> Result<Coord, String> {
        let mut nums = Vec::with_capacity(4);
        while let Some(Tok::Num(v)) = self.peek() {
            nums.push(*v);
            self.pos += 1;
        }
        if nums.len() < 2 || nums.len() > 4 {
            return Err(if nums.is_empty() {
                self.err("a coordinate")
            } else {
                trf("A coordinate needs 2 to 4 numbers, found {} (position {})", &[&nums.len(), &self.at()])
            });
        }
        let dims = match self.dims {
            Some(d) => {
                if d.count() != nums.len() {
                    return Err(trf(
                        "Coordinate has {} numbers but the geometry has {} dimensions (position {})",
                        &[&nums.len(), &d.count(), &self.at()],
                    ));
                }
                d
            }
            None => {
                let d = match nums.len() {
                    2 => Dims::Xy,
                    3 => Dims::Xyz,
                    _ => Dims::Xyzm,
                };
                self.dims = Some(d);
                d
            }
        };
        let mut c = Coord::xy(nums[0], nums[1]);
        match dims {
            Dims::Xy => {}
            Dims::Xyz => c.z = nums.get(2).copied(),
            Dims::Xym => c.m = nums.get(2).copied(),
            Dims::Xyzm => {
                c.z = nums.get(2).copied();
                c.m = nums.get(3).copied();
            }
        }
        Ok(c)
    }

    fn coord_list(&mut self) -> Result<Vec<Coord>, String> {
        self.expect(Tok::LParen, "'('")?;
        let mut cs = Vec::new();
        loop {
            cs.push(self.coord()?);
            if self.peek() == Some(&Tok::Comma) {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.expect(Tok::RParen, "')' or ','")?;
        Ok(cs)
    }

    fn ring_list(&mut self) -> Result<Vec<Vec<Coord>>, String> {
        self.expect(Tok::LParen, "'('")?;
        let mut rings = Vec::new();
        loop {
            if !self.eat_word("EMPTY") {
                rings.push(self.coord_list()?);
            }
            if self.peek() == Some(&Tok::Comma) {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.expect(Tok::RParen, "')' or ','")?;
        Ok(rings)
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Word(w) => format!("'{w}'"),
        Tok::Num(n) => format!("{n}"),
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::Comma => "','".into(),
        Tok::Semicolon => "';'".into(),
        Tok::Eq => "'='".into(),
    }
}

/// Parses WKT or PostGIS EWKT (`SRID=4326;POINT(1 2)`).
pub fn parse_wkt(input: &str) -> Result<Geometry, String> {
    let toks = tokenize(input)?;
    let mut p = Parser { toks, pos: 0, dims: None, explicit: false };
    let mut srid = None;
    if matches!(p.peek(), Some(Tok::Word(w)) if w == "SRID") {
        p.pos += 1;
        p.expect(Tok::Eq, "'='")?;
        let n = match p.next() {
            Some(Tok::Num(n)) if n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64 => n as i32,
            _ => return Err(trf("Invalid SRID at position {}", &[&p.at()])),
        };
        p.expect(Tok::Semicolon, "';'")?;
        srid = Some(n);
    }
    let geom = p.geometry(0)?;
    if p.peek().is_some() {
        return Err(p.err("end of text"));
    }
    Ok(Geometry { srid, dims: p.dims.unwrap_or_default(), geom })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> Coord {
        Coord::xy(x, y)
    }

    #[test]
    fn basic_types_and_ewkt() {
        let g = parse_wkt("SRID=4326;POINT(106.8 -6.2)").unwrap();
        assert_eq!(g.srid, Some(4326));
        assert_eq!(g.geom, Geom::Point(Some(pt(106.8, -6.2))));
        let g = parse_wkt("  linestring ( 0 0 , 1 1,2 2 )  ").unwrap();
        assert_eq!(g.geom, Geom::LineString(vec![pt(0.0, 0.0), pt(1.0, 1.0), pt(2.0, 2.0)]));
        let g = parse_wkt("POLYGON((0 0,10 0,10 10,0 10,0 0),(2 2,4 2,4 4,2 2))").unwrap();
        let Geom::Polygon(rings) = &g.geom else { panic!() };
        assert_eq!(rings.len(), 2);
        let g = parse_wkt("MULTIPOINT((1 2),(3 4))").unwrap();
        assert_eq!(g.geom, Geom::MultiPoint(vec![pt(1.0, 2.0), pt(3.0, 4.0)]));
        let g = parse_wkt("MULTIPOINT(1 2, 3 4, EMPTY)").unwrap();
        assert_eq!(g.geom, Geom::MultiPoint(vec![pt(1.0, 2.0), pt(3.0, 4.0)]));
        let g = parse_wkt("MULTILINESTRING((0 0,1 1),(2 2,3 3))").unwrap();
        assert!(matches!(g.geom, Geom::MultiLineString(ref l) if l.len() == 2));
        let g = parse_wkt("MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5),(5.2 5.1,5.8 5.1,5.8 5.5,5.2 5.1)))").unwrap();
        assert!(matches!(g.geom, Geom::MultiPolygon(ref p) if p.len() == 2 && p[1].len() == 2));
        let g = parse_wkt("GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1 1),POLYGON EMPTY)").unwrap();
        assert!(matches!(g.geom, Geom::GeometryCollection(ref gs) if gs.len() == 3));
        assert_eq!(parse_wkt("point empty").unwrap().geom, Geom::Point(None));
        assert_eq!(parse_wkt("GEOMETRYCOLLECTION EMPTY").unwrap().geom, Geom::GeometryCollection(vec![]));
        assert_eq!(parse_wkt("POINT(1e3 -2.5E-1)").unwrap().geom, Geom::Point(Some(pt(1000.0, -0.25))));
    }

    #[test]
    fn dimensions() {
        let g = parse_wkt("POINT Z (1 2 3)").unwrap();
        assert_eq!(g.dims, Dims::Xyz);
        assert_eq!(g.geom, Geom::Point(Some(Coord { x: 1.0, y: 2.0, z: Some(3.0), m: None })));
        let g = parse_wkt("POINTM(1 2 3)").unwrap();
        assert_eq!(g.dims, Dims::Xym);
        assert_eq!(g.geom, Geom::Point(Some(Coord { x: 1.0, y: 2.0, z: None, m: Some(3.0) })));
        let g = parse_wkt("LINESTRING ZM (1 2 3 4, 5 6 7 8)").unwrap();
        assert_eq!(g.dims, Dims::Xyzm);
        let g = parse_wkt("POINT(1 2 3)").unwrap();
        assert_eq!(g.dims, Dims::Xyz, "inferred from the ordinate count");
        assert!(parse_wkt("LINESTRING(1 2 3, 4 5)").is_err(), "mixed dimensions");
        assert!(parse_wkt("POINT Z (1 2)").is_err());
        assert!(parse_wkt("POINT(1 2 3 4 5)").is_err());
        let back = parse_wkt(&g.to_wkt()).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn errors_are_reported() {
        for bad in [
            "",
            "POINT",
            "POINT(1)",
            "POINT(1 2",
            "POINT(1 2) extra",
            "CIRCULARSTRING(0 0,1 1,2 0)",
            "SRID=abc;POINT(1 2)",
            "SRID=4326 POINT(1 2)",
            "POLYGON((0 0,1 1)",
            "POINT(1 2)#",
            "POINT(--1 2)",
        ] {
            let e = parse_wkt(bad).unwrap_err();
            assert!(!e.is_empty(), "{bad}");
        }
        let deep = format!("{}POINT(1 1){}", "GEOMETRYCOLLECTION(".repeat(40), ")".repeat(40));
        assert!(parse_wkt(&deep).is_err());
    }

    #[test]
    fn roundtrip_through_wkt() {
        for wkt in [
            "SRID=3857;MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))",
            "GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1.5 1))",
            "MULTIPOINT((1 2),(3 4))",
        ] {
            let g = parse_wkt(wkt).unwrap();
            assert_eq!(g.to_wkt(), wkt);
            assert_eq!(parse_wkt(&g.to_wkt()).unwrap(), g);
        }
    }
}
