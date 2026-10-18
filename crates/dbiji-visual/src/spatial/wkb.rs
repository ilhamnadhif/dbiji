//! WKB / EWKB parser (both byte orders, ISO Z/M type codes, PostGIS
//! Z/M/SRID flags) and helpers to decode geometry values of result sets
//! (WKT/EWKT text, hex encoded EWKB as returned by PostGIS, binary WKB and
//! the MySQL internal format with a leading SRID).

use super::geometry::{Coord, Dims, Geom, Geometry};
use super::wkt::parse_wkt;
use dbiji_core::value::Value;
use dbiji_core::{tr, trf};

const EWKB_Z: u32 = 0x8000_0000;
const EWKB_M: u32 = 0x4000_0000;
const EWKB_SRID: u32 = 0x2000_0000;
const MAX_DEPTH: usize = 32;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.remaining() < n {
            return Err(trf("WKB ends unexpectedly at byte {}", &[&self.pos]));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self, le: bool) -> Result<u32, String> {
        let b = self.take(4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Ok(if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
    }

    fn f64(&mut self, le: bool) -> Result<f64, String> {
        let b = self.take(8)?;
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(if le { f64::from_le_bytes(a) } else { f64::from_be_bytes(a) })
    }

    /// A count of items each at least `item_bytes` long; rejects counts the
    /// remaining input cannot hold (protects against huge allocations).
    fn count(&mut self, le: bool, item_bytes: usize) -> Result<usize, String> {
        let n = self.u32(le)? as usize;
        if n.saturating_mul(item_bytes.max(1)) > self.remaining() {
            return Err(trf("WKB count {} exceeds the data size", &[&n]));
        }
        Ok(n)
    }

    fn coord(&mut self, le: bool, dims: Dims) -> Result<Coord, String> {
        let x = self.f64(le)?;
        let y = self.f64(le)?;
        let mut c = Coord::xy(x, y);
        if dims.has_z() {
            c.z = Some(self.f64(le)?);
        }
        if dims.has_m() {
            c.m = Some(self.f64(le)?);
        }
        Ok(c)
    }

    fn coords(&mut self, le: bool, dims: Dims) -> Result<Vec<Coord>, String> {
        let n = self.count(le, 8 * dims.count())?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.coord(le, dims)?);
        }
        Ok(v)
    }

    fn rings(&mut self, le: bool, dims: Dims) -> Result<Vec<Vec<Coord>>, String> {
        let n = self.count(le, 4)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.coords(le, dims)?);
        }
        Ok(v)
    }

    /// Returns (geometry, srid, dims).
    fn geometry(&mut self, depth: usize) -> Result<(Geom, Option<i32>, Dims), String> {
        if depth > MAX_DEPTH {
            return Err(trf("Geometry nesting deeper than {} levels", &[&MAX_DEPTH]));
        }
        let le = match self.u8()? {
            0 => false,
            1 => true,
            b => return Err(trf("Invalid WKB byte order {}", &[&b])),
        };
        let raw = self.u32(le)?;
        let mut z = raw & EWKB_Z != 0;
        let mut m = raw & EWKB_M != 0;
        let srid = if raw & EWKB_SRID != 0 { Some(self.u32(le)? as i32) } else { None };
        let code = raw & 0x0FFF_FFFF;
        match code / 1000 {
            0 => {}
            1 => z = true,
            2 => m = true,
            3 => {
                z = true;
                m = true;
            }
            _ => return Err(trf("Unsupported WKB geometry type {}", &[&code])),
        }
        let dims = Dims::from_flags(z, m);
        let geom = match code % 1000 {
            1 => {
                let c = self.coord(le, dims)?;
                if c.x.is_nan() && c.y.is_nan() { Geom::Point(None) } else { Geom::Point(Some(c)) }
            }
            2 => Geom::LineString(self.coords(le, dims)?),
            3 => Geom::Polygon(self.rings(le, dims)?),
            4..=7 => {
                let n = self.count(le, 9)?;
                let mut members = Vec::with_capacity(n);
                for _ in 0..n {
                    let (g, _, _) = self.geometry(depth + 1)?;
                    members.push(g);
                }
                match code % 1000 {
                    4 => {
                        let mut pts = Vec::with_capacity(members.len());
                        for g in members {
                            match g {
                                Geom::Point(Some(c)) => pts.push(c),
                                Geom::Point(None) => {}
                                _ => return Err(tr("A WKB multi point may only contain points").to_string()),
                            }
                        }
                        Geom::MultiPoint(pts)
                    }
                    5 => {
                        let mut lines = Vec::with_capacity(members.len());
                        for g in members {
                            match g {
                                Geom::LineString(l) => lines.push(l),
                                _ => return Err(tr("A WKB multi line string may only contain line strings").to_string()),
                            }
                        }
                        Geom::MultiLineString(lines)
                    }
                    6 => {
                        let mut polys = Vec::with_capacity(members.len());
                        for g in members {
                            match g {
                                Geom::Polygon(p) => polys.push(p),
                                _ => return Err(tr("A WKB multi polygon may only contain polygons").to_string()),
                            }
                        }
                        Geom::MultiPolygon(polys)
                    }
                    _ => Geom::GeometryCollection(members),
                }
            }
            other => return Err(trf("Unsupported WKB geometry type {}", &[&other])),
        };
        Ok((geom, srid, dims))
    }
}

/// Parses WKB or PostGIS EWKB.
pub fn parse_wkb(bytes: &[u8]) -> Result<Geometry, String> {
    let mut r = Reader { data: bytes, pos: 0 };
    let (geom, srid, dims) = r.geometry(0)?;
    if r.remaining() != 0 {
        return Err(trf("{} unexpected bytes after the WKB geometry", &[&r.remaining()]));
    }
    Ok(Geometry { srid, dims, geom })
}

/// Parses hex encoded (E)WKB (`0101000020E6100000…`, optional `\x` / `0x` prefix).
pub fn parse_wkb_hex(text: &str) -> Result<Geometry, String> {
    let t = text.trim();
    let t = t.strip_prefix("\\x").or_else(|| t.strip_prefix("0x")).or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if !t.len().is_multiple_of(2) || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(tr("Not a hex encoded WKB value").to_string());
    }
    let bytes: Vec<u8> = (0..t.len())
        .step_by(2)
        .filter_map(|i| t.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect();
    parse_wkb(&bytes)
}

fn looks_like_hex_wkb(t: &str) -> bool {
    let t = t.trim();
    let t = t.strip_prefix("\\x").or_else(|| t.strip_prefix("0x")).unwrap_or(t);
    t.len() >= 10 && t.len().is_multiple_of(2) && (t.starts_with("00") || t.starts_with("01")) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decodes a geometry cell: WKT/EWKT or hex (E)WKB text, binary (E)WKB,
/// or MySQL's internal format (4 byte little endian SRID followed by WKB).
pub fn parse_geometry_value(value: &Value) -> Result<Geometry, String> {
    match value {
        Value::Null => Err(tr("NULL").to_string()),
        Value::Text(s) | Value::Json(s) => {
            if looks_like_hex_wkb(s) {
                parse_wkb_hex(s).or_else(|e| parse_wkt(s).map_err(|_| e))
            } else {
                parse_wkt(s)
            }
        }
        Value::Bytes(b) => match parse_wkb(b) {
            Ok(g) => Ok(g),
            Err(e) => {
                if b.len() > 4 {
                    let srid = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    let mut g = parse_wkb(&b[4..]).map_err(|_| e)?;
                    if srid != 0 && g.srid.is_none() {
                        g.srid = Some(srid);
                    }
                    Ok(g)
                } else {
                    Err(e)
                }
            }
        },
        other => Err(trf("Cannot read a geometry from a {} value", &[&other.variant_name()])),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal WKB writer for tests (ISO type codes, optional EWKB SRID).
    pub(crate) fn write_wkb(g: &Geometry, le: bool, ewkb: bool) -> Vec<u8> {
        fn put_u32(out: &mut Vec<u8>, v: u32, le: bool) {
            out.extend_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
        }
        fn put_f64(out: &mut Vec<u8>, v: f64, le: bool) {
            out.extend_from_slice(&if le { v.to_le_bytes() } else { v.to_be_bytes() });
        }
        fn coord(out: &mut Vec<u8>, c: &Coord, dims: Dims, le: bool) {
            put_f64(out, c.x, le);
            put_f64(out, c.y, le);
            if dims.has_z() {
                put_f64(out, c.z.unwrap_or(0.0), le);
            }
            if dims.has_m() {
                put_f64(out, c.m.unwrap_or(0.0), le);
            }
        }
        fn geom(out: &mut Vec<u8>, g: &Geom, dims: Dims, le: bool, srid: Option<i32>, ewkb: bool) {
            out.push(le as u8);
            let base = match g {
                Geom::Point(_) => 1,
                Geom::LineString(_) => 2,
                Geom::Polygon(_) => 3,
                Geom::MultiPoint(_) => 4,
                Geom::MultiLineString(_) => 5,
                Geom::MultiPolygon(_) => 6,
                Geom::GeometryCollection(_) => 7,
            };
            let code = if ewkb {
                let mut c = base;
                if dims.has_z() {
                    c |= EWKB_Z;
                }
                if dims.has_m() {
                    c |= EWKB_M;
                }
                if srid.is_some() {
                    c |= EWKB_SRID;
                }
                c
            } else {
                base + match dims {
                    Dims::Xy => 0,
                    Dims::Xyz => 1000,
                    Dims::Xym => 2000,
                    Dims::Xyzm => 3000,
                }
            };
            put_u32(out, code, le);
            if let (true, Some(s)) = (ewkb, srid) {
                put_u32(out, s as u32, le);
            }
            match g {
                Geom::Point(Some(c)) => coord(out, c, dims, le),
                Geom::Point(None) => {
                    for _ in 0..dims.count() {
                        put_f64(out, f64::NAN, le);
                    }
                }
                Geom::LineString(cs) => {
                    put_u32(out, cs.len() as u32, le);
                    cs.iter().for_each(|c| coord(out, c, dims, le));
                }
                Geom::Polygon(rings) => {
                    put_u32(out, rings.len() as u32, le);
                    for r in rings {
                        put_u32(out, r.len() as u32, le);
                        r.iter().for_each(|c| coord(out, c, dims, le));
                    }
                }
                Geom::MultiPoint(cs) => {
                    put_u32(out, cs.len() as u32, le);
                    cs.iter().for_each(|c| geom(out, &Geom::Point(Some(*c)), dims, le, None, ewkb));
                }
                Geom::MultiLineString(ls) => {
                    put_u32(out, ls.len() as u32, le);
                    ls.iter().for_each(|l| geom(out, &Geom::LineString(l.clone()), dims, le, None, ewkb));
                }
                Geom::MultiPolygon(ps) => {
                    put_u32(out, ps.len() as u32, le);
                    ps.iter().for_each(|p| geom(out, &Geom::Polygon(p.clone()), dims, le, None, ewkb));
                }
                Geom::GeometryCollection(gs) => {
                    put_u32(out, gs.len() as u32, le);
                    gs.iter().for_each(|x| geom(out, x, dims, le, None, ewkb));
                }
            }
        }
        let mut out = Vec::new();
        geom(&mut out, &g.geom, g.dims, le, g.srid, ewkb);
        out
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    #[test]
    fn roundtrip_all_types_both_orders() {
        let samples = [
            "POINT(1 2)",
            "POINT Z (1 2 3)",
            "POINT M (1 2 4)",
            "POINT ZM (1 2 3 4)",
            "LINESTRING(0 0,1 1,2 0.5)",
            "POLYGON((0 0,10 0,10 10,0 10,0 0),(2 2,4 2,4 4,2 2))",
            "MULTIPOINT((1 2),(3 4))",
            "MULTILINESTRING((0 0,1 1),(2 2,3 3))",
            "MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))",
            "GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1 1))",
            "POINT EMPTY",
            "LINESTRING EMPTY",
        ];
        for wkt in samples {
            let g = parse_wkt(wkt).unwrap();
            for le in [true, false] {
                for ewkb in [false, true] {
                    let bytes = write_wkb(&g, le, ewkb);
                    let back = parse_wkb(&bytes).unwrap_or_else(|e| panic!("{wkt} le={le} ewkb={ewkb}: {e}"));
                    assert_eq!(back, g, "{wkt} le={le} ewkb={ewkb}");
                }
            }
        }
    }

    #[test]
    fn ewkb_srid_and_postgis_hex() {
        let mut g = parse_wkt("POINT(106.8 -6.2)").unwrap();
        g.srid = Some(4326);
        let bytes = write_wkb(&g, true, true);
        let text = hex(&bytes);
        assert!(text.starts_with("0101000020E6100000"), "{text}");
        let back = parse_wkb_hex(&text).unwrap();
        assert_eq!(back.srid, Some(4326));
        assert_eq!(back.to_wkt(), "SRID=4326;POINT(106.8 -6.2)");
        assert_eq!(parse_geometry_value(&Value::text(text.clone())).unwrap(), back);
        assert_eq!(parse_geometry_value(&Value::text(format!("\\x{}", text.to_lowercase()))).unwrap(), back);
        assert_eq!(parse_geometry_value(&Value::text("SRID=4326;POINT(106.8 -6.2)")).unwrap(), back);
        // MySQL internal format: SRID (LE) + WKB
        let mut mysql = 4326i32.to_le_bytes().to_vec();
        mysql.extend(write_wkb(&parse_wkt("POINT(1 2)").unwrap(), true, false));
        let m = parse_geometry_value(&Value::bytes(mysql)).unwrap();
        assert_eq!((m.srid, m.to_wkt()), (Some(4326), "SRID=4326;POINT(1 2)".to_string()));
        let plain = parse_geometry_value(&Value::bytes(write_wkb(&parse_wkt("LINESTRING(0 0,1 1)").unwrap(), false, false))).unwrap();
        assert_eq!(plain.type_name(), "LINESTRING");
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert!(parse_wkb(&[]).is_err());
        assert!(parse_wkb(&[7, 1, 0, 0, 0]).is_err(), "bad byte order");
        assert!(parse_wkb(&[1, 99, 0, 0, 0]).is_err(), "bad type");
        // huge count must not allocate
        assert!(parse_wkb(&[1, 2, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]).is_err());
        let ok = write_wkb(&parse_wkt("LINESTRING(0 0,1 1)").unwrap(), true, false);
        assert!(parse_wkb(&ok[..ok.len() - 3]).is_err(), "truncated");
        let mut trailing = ok.clone();
        trailing.push(0);
        assert!(parse_wkb(&trailing).is_err());
        assert!(parse_wkb_hex("01zz").is_err());
        assert!(parse_geometry_value(&Value::Int(5)).is_err());
        assert!(parse_geometry_value(&Value::Null).is_err());
        // deep nesting
        let mut deep = Vec::new();
        for _ in 0..40 {
            deep.extend([1u8, 7, 0, 0, 0, 1, 0, 0, 0]);
        }
        assert!(parse_wkb(&deep).is_err());
        // fuzz-ish: random prefixes of valid input never panic
        let valid = write_wkb(&parse_wkt("GEOMETRYCOLLECTION(POLYGON((0 0,1 0,1 1,0 0)),POINT(1 1))").unwrap(), true, true);
        for n in 0..valid.len() {
            let _ = parse_wkb(&valid[..n]);
        }
    }
}
