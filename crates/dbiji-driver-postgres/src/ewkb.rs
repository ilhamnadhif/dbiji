//! Parser for PostGIS (E)WKB, producing EWKT text such as
//! `SRID=4326;POINT(1 2)`.
//!
//! Supports both byte orders, the EWKB flag bits (Z = `0x8000_0000`,
//! M = `0x4000_0000`, SRID = `0x2000_0000`) as well as ISO WKB type codes
//! (1000/2000/3000 offsets), every OGC geometry type PostGIS can store
//! (including curves, triangles, TIN and polyhedral surfaces) and empty
//! geometries. The output follows `ST_AsEWKT`: Z coordinates are implied by
//! the coordinate count, M-only geometries get an `M` suffix (`POINTM(1 2 3)`).

use std::fmt::Write;

/// Errors are plain messages; callers fall back to the raw value.
pub(crate) type EwkbResult<T> = Result<T, &'static str>;

const MAX_DEPTH: usize = 32;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> EwkbResult<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or("length overflow")?;
        if end > self.buf.len() {
            return Err("truncated EWKB");
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> EwkbResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self, le: bool) -> EwkbResult<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| "truncated EWKB")?;
        Ok(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn f64(&mut self, le: bool) -> EwkbResult<f64> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| "truncated EWKB")?;
        Ok(if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }

    /// A count that must be plausible for the remaining bytes.
    fn count(&mut self, le: bool, min_item_bytes: usize) -> EwkbResult<usize> {
        let n = self.u32(le)? as usize;
        let remaining = self.buf.len() - self.pos;
        if n.saturating_mul(min_item_bytes.max(1)) > remaining {
            return Err("invalid element count");
        }
        Ok(n)
    }
}

#[derive(Clone, Copy)]
struct Dims {
    z: bool,
    m: bool,
}

impl Dims {
    fn n(self) -> usize {
        2 + self.z as usize + self.m as usize
    }
}

fn type_name(code: u32) -> Option<&'static str> {
    Some(match code {
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
        15 => "POLYHEDRALSURFACE",
        16 => "TIN",
        17 => "TRIANGLE",
        _ => return None,
    })
}

/// Convert EWKB bytes into EWKT text.
pub(crate) fn ewkb_to_ewkt(buf: &[u8]) -> EwkbResult<String> {
    let mut r = Reader { buf, pos: 0 };
    let mut out = String::with_capacity(buf.len() * 2);
    parse_geometry(&mut r, &mut out, true, 0)?;
    Ok(out)
}

/// Convert hex encoded EWKB (the PostGIS text output) into EWKT.
pub(crate) fn hex_ewkb_to_ewkt(hex_text: &str) -> EwkbResult<String> {
    let bytes = hex::decode(hex_text.trim()).map_err(|_| "not hex encoded")?;
    ewkb_to_ewkt(&bytes)
}

fn parse_geometry(r: &mut Reader<'_>, out: &mut String, top: bool, depth: usize) -> EwkbResult<Dims> {
    if depth > MAX_DEPTH {
        return Err("geometry nesting too deep");
    }
    let order = r.u8()?;
    let le = match order {
        0 => false,
        1 => true,
        _ => return Err("invalid byte order"),
    };
    let raw = r.u32(le)?;
    let mut z = raw & 0x8000_0000 != 0;
    let mut m = raw & 0x4000_0000 != 0;
    let has_srid = raw & 0x2000_0000 != 0;
    let mut code = raw & 0x0FFF_FFFF;
    // ISO WKB dimension offsets
    match code / 1000 {
        1 => z = true,
        2 => m = true,
        3 => {
            z = true;
            m = true;
        }
        _ => {}
    }
    code %= 1000;
    let srid = if has_srid { Some(r.u32(le)? as i32) } else { None };
    let name = type_name(code).ok_or("unknown geometry type")?;
    let dims = Dims { z, m };
    if top
        && let Some(s) = srid
        && s != 0
    {
        let _ = write!(out, "SRID={s};");
    }
    out.push_str(name);
    if m && !z {
        out.push('M');
    }
    match code {
        1 => {
            let coords = read_coords(r, le, dims)?;
            if coords.iter().all(|c| c.is_nan()) {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                write_coords(out, &coords);
                out.push(')');
            }
        }
        2 | 8 => {
            let n = r.count(le, dims.n() * 8)?;
            if n == 0 {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                write_point_list(r, le, dims, n, out)?;
                out.push(')');
            }
        }
        3 | 17 => {
            let rings = r.count(le, 4)?;
            if rings == 0 {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                for i in 0..rings {
                    if i > 0 {
                        out.push(',');
                    }
                    let n = r.count(le, dims.n() * 8)?;
                    out.push('(');
                    write_point_list(r, le, dims, n, out)?;
                    out.push(')');
                }
                out.push(')');
            }
        }
        4 => {
            // MULTIPOINT(1 2,3 4): points without inner parentheses (ST_AsEWKT style)
            let n = r.count(le, 5)?;
            if n == 0 {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    let mut sub = String::new();
                    parse_geometry(r, &mut sub, false, depth + 1)?;
                    // strip the "POINT" / "POINTM" prefix and the parentheses
                    let inner = sub.trim_start_matches("POINTM").trim_start_matches("POINT").trim_start_matches('(').trim_end_matches(')');
                    if inner.trim() == "EMPTY" {
                        out.push_str("EMPTY");
                    } else {
                        out.push_str(inner);
                    }
                }
                out.push(')');
            }
        }
        5 | 6 | 7 | 9 | 10 | 11 | 12 | 15 | 16 => {
            let n = r.count(le, 5)?;
            if n == 0 {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    let mut sub = String::new();
                    parse_geometry(r, &mut sub, false, depth + 1)?;
                    match code {
                        // Typed collections omit the member type names except for curves.
                        5 => out.push_str(strip_type(&sub, &["LINESTRINGM", "LINESTRING"])),
                        6 => out.push_str(strip_type(&sub, &["POLYGONM", "POLYGON"])),
                        15 => out.push_str(strip_type(&sub, &["POLYGONM", "POLYGON"])),
                        16 => out.push_str(strip_type(&sub, &["TRIANGLEM", "TRIANGLE"])),
                        9..=12 => {
                            // curve members: plain linestrings / polygons drop their name
                            let s = strip_type(&sub, &["LINESTRINGM", "LINESTRING"]);
                            let s = if s.len() == sub.len() { strip_type(&sub, &["POLYGONM", "POLYGON"]) } else { s };
                            out.push_str(s);
                        }
                        _ => out.push_str(&sub),
                    }
                }
                out.push(')');
            }
        }
        _ => return Err("unsupported geometry type"),
    }
    Ok(dims)
}

fn strip_type<'a>(s: &'a str, names: &[&str]) -> &'a str {
    for n in names {
        if let Some(rest) = s.strip_prefix(n)
            && rest.starts_with('(')
        {
            return rest;
        }
    }
    s
}

fn read_coords(r: &mut Reader<'_>, le: bool, dims: Dims) -> EwkbResult<Vec<f64>> {
    let mut v = Vec::with_capacity(dims.n());
    for _ in 0..dims.n() {
        v.push(r.f64(le)?);
    }
    Ok(v)
}

fn write_point_list(r: &mut Reader<'_>, le: bool, dims: Dims, n: usize, out: &mut String) -> EwkbResult<()> {
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        let c = read_coords(r, le, dims)?;
        write_coords(out, &c);
    }
    Ok(())
}

fn write_coords(out: &mut String, coords: &[f64]) {
    for (i, c) in coords.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_number(out, *c);
    }
}

fn write_number(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    } else if v == v.trunc() && v.abs() < 1e15 {
        let _ = write!(out, "{}", v as i64);
    } else {
        let _ = write!(out, "{v}");
    }
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

    #[test]
    fn point_with_srid_hex() {
        // SELECT 'SRID=4326;POINT(1 2)'::geometry
        let hex = "0101000020E6100000000000000000F03F0000000000000040";
        assert_eq!(hex_ewkb_to_ewkt(hex).unwrap(), "SRID=4326;POINT(1 2)");
    }

    #[test]
    fn big_endian_point_z() {
        let mut v = vec![0u8];
        v.extend_from_slice(&(1u32 | 0x8000_0000).to_be_bytes());
        for c in [1.5f64, -2.25, 3.0] {
            v.extend_from_slice(&c.to_be_bytes());
        }
        assert_eq!(ewkb_to_ewkt(&v).unwrap(), "POINT(1.5 -2.25 3)");
    }

    #[test]
    fn point_m_and_zm_iso() {
        let mut v = vec![1u8];
        v.extend_from_slice(&2001u32.to_le_bytes()); // ISO Point M
        for c in [1.0f64, 2.0, 7.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(ewkb_to_ewkt(&v).unwrap(), "POINTM(1 2 7)");
        let mut v = vec![1u8];
        v.extend_from_slice(&(1u32 | 0x8000_0000 | 0x4000_0000).to_le_bytes());
        for c in [1.0f64, 2.0, 3.0, 4.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(ewkb_to_ewkt(&v).unwrap(), "POINT(1 2 3 4)");
    }

    #[test]
    fn linestring_polygon_multi() {
        // LINESTRING(0 0,1 1)
        let mut ls = vec![1u8];
        ls.extend_from_slice(&2u32.to_le_bytes());
        ls.extend_from_slice(&2u32.to_le_bytes());
        for c in [0.0f64, 0.0, 1.0, 1.0] {
            ls.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(ewkb_to_ewkt(&ls).unwrap(), "LINESTRING(0 0,1 1)");
        // POLYGON((0 0,1 0,1 1,0 0))
        let mut pg = vec![1u8];
        pg.extend_from_slice(&3u32.to_le_bytes());
        pg.extend_from_slice(&1u32.to_le_bytes());
        pg.extend_from_slice(&4u32.to_le_bytes());
        for c in [0.0f64, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0] {
            pg.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(ewkb_to_ewkt(&pg).unwrap(), "POLYGON((0 0,1 0,1 1,0 0))");
        // MULTIPOINT(1 2,3 4) with SRID
        let mut mp = vec![1u8];
        mp.extend_from_slice(&(4u32 | 0x2000_0000).to_le_bytes());
        mp.extend_from_slice(&3857u32.to_le_bytes());
        mp.extend_from_slice(&2u32.to_le_bytes());
        mp.extend(le_point(1.0, 2.0));
        mp.extend(le_point(3.0, 4.0));
        assert_eq!(ewkb_to_ewkt(&mp).unwrap(), "SRID=3857;MULTIPOINT(1 2,3 4)");
        // MULTIPOLYGON(((0 0,1 0,1 1,0 0)))
        let mut mpg = vec![1u8];
        mpg.extend_from_slice(&6u32.to_le_bytes());
        mpg.extend_from_slice(&1u32.to_le_bytes());
        mpg.extend(pg.clone());
        assert_eq!(ewkb_to_ewkt(&mpg).unwrap(), "MULTIPOLYGON(((0 0,1 0,1 1,0 0)))");
        // MULTILINESTRING((0 0,1 1))
        let mut mls = vec![1u8];
        mls.extend_from_slice(&5u32.to_le_bytes());
        mls.extend_from_slice(&1u32.to_le_bytes());
        mls.extend(ls.clone());
        assert_eq!(ewkb_to_ewkt(&mls).unwrap(), "MULTILINESTRING((0 0,1 1))");
        // GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))
        let mut gc = vec![1u8];
        gc.extend_from_slice(&7u32.to_le_bytes());
        gc.extend_from_slice(&2u32.to_le_bytes());
        gc.extend(le_point(1.0, 2.0));
        gc.extend(ls);
        assert_eq!(ewkb_to_ewkt(&gc).unwrap(), "GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))");
    }

    #[test]
    fn empty_geometries() {
        let mut e = vec![1u8];
        e.extend_from_slice(&7u32.to_le_bytes());
        e.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(ewkb_to_ewkt(&e).unwrap(), "GEOMETRYCOLLECTION EMPTY");
        let p = le_point(f64::NAN, f64::NAN);
        assert_eq!(ewkb_to_ewkt(&p).unwrap(), "POINT EMPTY");
        let mut ls = vec![0u8];
        ls.extend_from_slice(&2u32.to_be_bytes());
        ls.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(ewkb_to_ewkt(&ls).unwrap(), "LINESTRING EMPTY");
    }

    #[test]
    fn rejects_garbage() {
        assert!(ewkb_to_ewkt(&[]).is_err());
        assert!(ewkb_to_ewkt(&[1, 1, 0, 0]).is_err());
        assert!(ewkb_to_ewkt(&[7, 1, 0, 0, 0]).is_err());
        // absurd count must not allocate
        let mut v = vec![1u8];
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(ewkb_to_ewkt(&v).is_err());
        assert!(hex_ewkb_to_ewkt("zz").is_err());
    }
}
