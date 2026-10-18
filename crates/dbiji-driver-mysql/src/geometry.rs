//! Conversion of MySQL internal geometry values (4-byte little-endian SRID
//! followed by WKB) and plain / ISO / EWKB well-known binary into EWKT text
//! (`SRID=4326;POINT(1 2)`).
//!
//! All OGC / SQL-MM geometry types are supported, including Z, M and ZM
//! variants in both ISO (type + 1000/2000/3000) and EWKB (high flag bits)
//! encodings. Two-dimensional geometries are written without a dimension tag
//! (`POINT(1 2)`), others with the ISO tag (`POINT Z (1 2 3)`).

use std::fmt::Write as _;

/// Maximum nesting depth of geometry collections accepted.
const MAX_DEPTH: usize = 32;

/// Convert a MySQL geometry column value (`SRID` + WKB) into EWKT.
/// The SRID prefix is omitted for SRID 0. Returns `None` for malformed input.
pub fn mysql_geometry_to_ewkt(value: &[u8]) -> Option<String> {
    if value.len() < 4 {
        return None;
    }
    let srid = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
    let mut r = Reader {
        buf: &value[4..],
        pos: 0,
    };
    let mut out = String::with_capacity(value.len() * 2);
    if srid != 0 {
        let _ = write!(out, "SRID={srid};");
    }
    let start = out.len();
    let inner_srid = read_geometry(&mut r, &mut out, 0)?;
    if srid == 0
        && let Some(s) = inner_srid.filter(|s| *s != 0)
    {
        out.insert_str(start, &format!("SRID={s};"));
    }
    if r.pos != r.buf.len() {
        return None;
    }
    Some(out)
}

/// Convert plain WKB / ISO WKB / EWKB into (E)WKT. An embedded EWKB SRID is
/// emitted as `SRID=n;` prefix.
pub fn wkb_to_ewkt(wkb: &[u8]) -> Option<String> {
    let mut r = Reader { buf: wkb, pos: 0 };
    let mut out = String::with_capacity(wkb.len() * 2);
    let srid = read_geometry(&mut r, &mut out, 0)?;
    if r.pos != r.buf.len() {
        return None;
    }
    if let Some(s) = srid.filter(|s| *s != 0) {
        out.insert_str(0, &format!("SRID={s};"));
    }
    Some(out)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn u32(&mut self, le: bool) -> Option<u32> {
        let b = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }

    fn f64(&mut self, le: bool) -> Option<f64> {
        let b = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Some(if le {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        })
    }
}

#[derive(Clone, Copy)]
struct Dims {
    z: bool,
    m: bool,
}

impl Dims {
    fn count(self) -> usize {
        2 + self.z as usize + self.m as usize
    }

    fn tag(self) -> &'static str {
        match (self.z, self.m) {
            (false, false) => "",
            (true, false) => " Z",
            (false, true) => " M",
            (true, true) => " ZM",
        }
    }
}

struct Header {
    le: bool,
    base: u32,
    dims: Dims,
    srid: Option<u32>,
}

fn read_header(r: &mut Reader<'_>) -> Option<Header> {
    let le = match r.u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let raw = r.u32(le)?;
    let ewkb_z = raw & 0x8000_0000 != 0;
    let ewkb_m = raw & 0x4000_0000 != 0;
    let ewkb_srid = raw & 0x2000_0000 != 0;
    let t = raw & 0x0FFF_FFFF;
    let srid = if ewkb_srid { Some(r.u32(le)?) } else { None };
    let (base, iso_z, iso_m) = match t / 1000 {
        0 => (t, false, false),
        1 => (t - 1000, true, false),
        2 => (t - 2000, false, true),
        3 => (t - 3000, true, true),
        _ => return None,
    };
    Some(Header {
        le,
        base,
        dims: Dims {
            z: iso_z || ewkb_z,
            m: iso_m || ewkb_m,
        },
        srid,
    })
}

fn type_name(base: u32) -> Option<&'static str> {
    Some(match base {
        1 => "POINT",
        2 => "LINESTRING",
        3 => "POLYGON",
        4 => "MULTIPOINT",
        5 => "MULTILINESTRING",
        6 => "MULTIPOLYGON",
        7 => "GEOMETRYCOLLECTION",
        8 => "CIRCULARSTRING",
        9 => "COMPOUNDCURVE",
        10 => "CURVEPOLYGON",
        11 => "MULTICURVE",
        12 => "MULTISURFACE",
        13 => "CURVE",
        14 => "SURFACE",
        15 => "POLYHEDRALSURFACE",
        16 => "TIN",
        17 => "TRIANGLE",
        _ => return None,
    })
}

fn push_num(out: &mut String, v: f64) {
    if v == 0.0 {
        out.push('0');
    } else {
        let _ = write!(out, "{v}");
    }
}

fn read_coords(r: &mut Reader<'_>, le: bool, dims: Dims, out: &mut String) -> Option<bool> {
    let n = dims.count();
    let mut vals = [0f64; 4];
    for v in vals.iter_mut().take(n) {
        *v = r.f64(le)?;
    }
    let empty = vals[..n].iter().all(|v| v.is_nan());
    if !empty {
        for (i, v) in vals[..n].iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            push_num(out, *v);
        }
    }
    Some(empty)
}

/// Read `count` with a sanity check against the remaining input so a corrupt
/// length cannot trigger huge allocations or long loops.
fn read_count(r: &mut Reader<'_>, le: bool, min_item_bytes: usize) -> Option<usize> {
    let n = r.u32(le)? as usize;
    if n.saturating_mul(min_item_bytes) > r.remaining() {
        return None;
    }
    Some(n)
}

fn read_point_list(r: &mut Reader<'_>, le: bool, dims: Dims, out: &mut String) -> Option<()> {
    let n = read_count(r, le, dims.count() * 8)?;
    if n == 0 {
        out.push_str(" EMPTY");
        return Some(());
    }
    out.push('(');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        read_coords(r, le, dims, out)?;
    }
    out.push(')');
    Some(())
}

fn read_rings(r: &mut Reader<'_>, le: bool, dims: Dims, out: &mut String) -> Option<()> {
    let n = read_count(r, le, 4)?;
    if n == 0 {
        out.push_str(" EMPTY");
        return Some(());
    }
    out.push('(');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        let start = out.len();
        read_point_list(r, le, dims, out)?;
        if out[start..].starts_with(" EMPTY") {
            out.truncate(start);
            out.push_str("EMPTY");
        }
    }
    out.push(')');
    Some(())
}

/// Write the body of a geometry (the part after the type keyword) for a
/// nested element of a multi-geometry, i.e. without repeating the keyword
/// where WKT omits it (MULTIPOINT, MULTILINESTRING, MULTIPOLYGON members).
fn read_member_body(r: &mut Reader<'_>, expect: u32, out: &mut String, depth: usize) -> Option<()> {
    let h = read_header(r)?;
    if h.base != expect {
        return None;
    }
    let start = out.len();
    match h.base {
        1 => {
            out.push('(');
            let empty = read_coords(r, h.le, h.dims, out)?;
            if empty {
                out.truncate(start);
                out.push_str("EMPTY");
            } else {
                out.push(')');
            }
        }
        2 => read_point_list(r, h.le, h.dims, out)?,
        3 => read_rings(r, h.le, h.dims, out)?,
        _ => {
            let _ = depth;
            return None;
        }
    }
    if out[start..].starts_with(' ') {
        // " EMPTY" inside a multi geometry is written without the leading space
        let trimmed = out[start..].trim_start().to_string();
        out.truncate(start);
        out.push_str(&trimmed);
    }
    Some(())
}

/// Read one geometry and append its WKT. Returns the embedded EWKB SRID, if any.
fn read_geometry(r: &mut Reader<'_>, out: &mut String, depth: usize) -> Option<Option<u32>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let h = read_header(r)?;
    let name = type_name(h.base)?;
    out.push_str(name);
    out.push_str(h.dims.tag());
    let tagged = !h.dims.tag().is_empty();
    match h.base {
        1 => {
            let mut body = String::new();
            let empty = read_coords(r, h.le, h.dims, &mut body)?;
            if empty {
                out.push_str(" EMPTY");
            } else {
                if tagged {
                    out.push(' ');
                }
                out.push('(');
                out.push_str(&body);
                out.push(')');
            }
        }
        2 | 8 => {
            let mut body = String::new();
            read_point_list(r, h.le, h.dims, &mut body)?;
            if tagged && !body.starts_with(' ') {
                out.push(' ');
            }
            out.push_str(&body);
        }
        3 | 17 => {
            let mut body = String::new();
            read_rings(r, h.le, h.dims, &mut body)?;
            if tagged && !body.starts_with(' ') {
                out.push(' ');
            }
            out.push_str(&body);
        }
        4 | 5 | 6 | 15 | 16 => {
            let member = match h.base {
                4 => 1,
                5 => 2,
                _ => 3,
            };
            let n = read_count(r, h.le, 5)?;
            if n == 0 {
                out.push_str(" EMPTY");
            } else {
                if tagged {
                    out.push(' ');
                }
                out.push('(');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    read_member_body(r, member, out, depth + 1)?;
                }
                out.push(')');
            }
        }
        7 | 9 | 10 | 11 | 12 => {
            let n = read_count(r, h.le, 5)?;
            if n == 0 {
                out.push_str(" EMPTY");
            } else {
                if tagged {
                    out.push(' ');
                }
                out.push('(');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    read_geometry(r, out, depth + 1)?;
                }
                out.push(')');
            }
        }
        _ => return None,
    }
    Some(h.srid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le_point(x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    fn with_srid(srid: u32, wkb: Vec<u8>) -> Vec<u8> {
        let mut v = srid.to_le_bytes().to_vec();
        v.extend(wkb);
        v
    }

    #[test]
    fn point_with_srid() {
        assert_eq!(
            mysql_geometry_to_ewkt(&with_srid(4326, le_point(1.0, 2.0))).unwrap(),
            "SRID=4326;POINT(1 2)"
        );
        assert_eq!(
            mysql_geometry_to_ewkt(&with_srid(0, le_point(1.5, -2.25))).unwrap(),
            "POINT(1.5 -2.25)"
        );
        assert_eq!(
            mysql_geometry_to_ewkt(&with_srid(0, le_point(f64::NAN, f64::NAN))).unwrap(),
            "POINT EMPTY"
        );
    }

    #[test]
    fn big_endian_linestring() {
        let mut v = vec![0u8];
        v.extend_from_slice(&2u32.to_be_bytes());
        v.extend_from_slice(&2u32.to_be_bytes());
        for c in [0.0f64, 0.0, 10.0, 20.0] {
            v.extend_from_slice(&c.to_be_bytes());
        }
        assert_eq!(wkb_to_ewkt(&v).unwrap(), "LINESTRING(0 0,10 20)");
    }

    #[test]
    fn polygon_and_multi() {
        let mut poly = vec![1u8];
        poly.extend_from_slice(&3u32.to_le_bytes());
        poly.extend_from_slice(&1u32.to_le_bytes());
        poly.extend_from_slice(&4u32.to_le_bytes());
        for c in [0.0f64, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0] {
            poly.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wkb_to_ewkt(&poly).unwrap(), "POLYGON((0 0,1 0,1 1,0 0))");

        let mut mp = vec![1u8];
        mp.extend_from_slice(&4u32.to_le_bytes());
        mp.extend_from_slice(&2u32.to_le_bytes());
        mp.extend(le_point(1.0, 2.0));
        mp.extend(le_point(3.0, 4.0));
        assert_eq!(wkb_to_ewkt(&mp).unwrap(), "MULTIPOINT((1 2),(3 4))");

        let mut mpoly = vec![1u8];
        mpoly.extend_from_slice(&6u32.to_le_bytes());
        mpoly.extend_from_slice(&1u32.to_le_bytes());
        mpoly.extend(poly.clone());
        assert_eq!(
            wkb_to_ewkt(&mpoly).unwrap(),
            "MULTIPOLYGON(((0 0,1 0,1 1,0 0)))"
        );

        let mut gc = vec![1u8];
        gc.extend_from_slice(&7u32.to_le_bytes());
        gc.extend_from_slice(&2u32.to_le_bytes());
        gc.extend(le_point(1.0, 2.0));
        gc.extend(poly);
        assert_eq!(
            wkb_to_ewkt(&gc).unwrap(),
            "GEOMETRYCOLLECTION(POINT(1 2),POLYGON((0 0,1 0,1 1,0 0)))"
        );

        let mut empty_gc = vec![1u8];
        empty_gc.extend_from_slice(&7u32.to_le_bytes());
        empty_gc.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            mysql_geometry_to_ewkt(&with_srid(3857, empty_gc)).unwrap(),
            "SRID=3857;GEOMETRYCOLLECTION EMPTY"
        );
    }

    #[test]
    fn z_m_variants() {
        // ISO PointZ (1001)
        let mut v = vec![1u8];
        v.extend_from_slice(&1001u32.to_le_bytes());
        for c in [1.0f64, 2.0, 3.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wkb_to_ewkt(&v).unwrap(), "POINT Z (1 2 3)");
        // ISO LineStringM (2002)
        let mut v = vec![1u8];
        v.extend_from_slice(&2002u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        for c in [1.0f64, 2.0, 7.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wkb_to_ewkt(&v).unwrap(), "LINESTRING M (1 2 7)");
        // EWKB PointZM with SRID flag
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | 0x8000_0000 | 0x4000_0000 | 0x2000_0000).to_le_bytes());
        v.extend_from_slice(&4326u32.to_le_bytes());
        for c in [1.0f64, 2.0, 3.0, 4.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wkb_to_ewkt(&v).unwrap(), "SRID=4326;POINT ZM (1 2 3 4)");
        // MultiPoint Z
        let mut v = vec![1u8];
        v.extend_from_slice(&1004u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.push(1);
        v.extend_from_slice(&1001u32.to_le_bytes());
        for c in [5.0f64, 6.0, 7.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(wkb_to_ewkt(&v).unwrap(), "MULTIPOINT Z ((5 6 7))");
    }

    #[test]
    fn malformed() {
        assert!(mysql_geometry_to_ewkt(&[0, 0]).is_none());
        assert!(wkb_to_ewkt(&[2, 1, 0, 0, 0]).is_none());
        // truncated point
        let mut v = le_point(1.0, 2.0);
        v.truncate(10);
        assert!(wkb_to_ewkt(&v).is_none());
        // absurd count
        let mut v = vec![1u8];
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(wkb_to_ewkt(&v).is_none());
        // trailing garbage
        let mut v = le_point(1.0, 2.0);
        v.push(0);
        assert!(wkb_to_ewkt(&v).is_none());
        // excessive nesting
        let mut v = Vec::new();
        for _ in 0..40 {
            v.push(1u8);
            v.extend_from_slice(&7u32.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes());
        }
        v.extend(le_point(0.0, 0.0));
        assert!(wkb_to_ewkt(&v).is_none());
    }
}
