//! Helpers for the special value viewers (FR-D7): hex dumps, image
//! detection and decoding, JSON / XML pretty printing, array listing.

use dbiji_core::{Value, tr, trf};
use std::fmt::Write as _;

/// Image formats recognised in binary values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    /// PNG
    Png,
    /// JPEG
    Jpeg,
    /// GIF
    Gif,
    /// BMP
    Bmp,
    /// WebP
    WebP,
}

impl ImageKind {
    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            ImageKind::Png => "PNG",
            ImageKind::Jpeg => "JPEG",
            ImageKind::Gif => "GIF",
            ImageKind::Bmp => "BMP",
            ImageKind::WebP => "WebP",
        }
    }

    /// Usual file extension.
    pub fn extension(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Bmp => "bmp",
            ImageKind::WebP => "webp",
        }
    }
}

/// Detect an image by its magic bytes.
pub fn detect_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() > 14 && bytes.starts_with(b"BM") {
        Some(ImageKind::Bmp)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::WebP)
    } else {
        None
    }
}

/// A decoded image ready to be uploaded as a texture.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedImage {
    /// Pixels (possibly downscaled).
    pub image: egui::ColorImage,
    /// Original width and height.
    pub original_size: [u32; 2],
    /// Detected format.
    pub kind: ImageKind,
}

/// Decode an image value, downscaling so that no side exceeds `max_side`.
pub fn decode_image(bytes: &[u8], max_side: u32) -> Result<DecodedImage, String> {
    let kind = detect_image(bytes).ok_or_else(|| tr("Not a supported image (PNG, JPEG, GIF, BMP, WebP)").to_string())?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| trf("Cannot read image: {}", &[&e]))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| trf("Cannot decode image: {}", &[&e]))?;
    let original_size = [img.width(), img.height()];
    let max_side = max_side.max(1);
    let img = if img.width() > max_side || img.height() > max_side { img.thumbnail(max_side, max_side) } else { img };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(DecodedImage { image: egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), original_size, kind })
}

/// Human readable byte size (`1.5 KB`).
pub fn byte_size_text(n: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Classic hex dump: `offset | 16 hex bytes | ascii`, at most `max_bytes` bytes.
pub fn hex_dump(bytes: &[u8], max_bytes: usize) -> String {
    let shown = &bytes[..bytes.len().min(max_bytes)];
    let mut out = String::with_capacity(shown.len() * 4 + 64);
    for (line, chunk) in shown.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", line * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for b in chunk {
            out.push(if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' });
        }
        out.push_str("|\n");
    }
    if bytes.len() > shown.len() {
        out.push_str(&trf("… {} more bytes", &[&(bytes.len() - shown.len())]));
        out.push('\n');
    }
    out
}

/// Pretty print JSON text.
pub fn pretty_json(text: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| trf("Invalid JSON: {}", &[&e]))?;
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

/// Pretty print an XML document with two-space indentation. Elements that
/// contain only text stay on one line.
pub fn pretty_xml(text: &str) -> Result<String, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0usize;
    // Per open element: did it get child elements (so the end tag goes on its own line)?
    let mut stack: Vec<bool> = Vec::new();
    let mut inline = false;
    let newline = |out: &mut String, depth: usize| {
        if !out.is_empty() {
            out.push('\n');
        }
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    fn s(b: &str) -> String {
        b.to_string()
    }
    loop {
        let ev = reader.read_event().map_err(|e| trf("Invalid XML at byte {}: {}", &[&reader.error_position(), &e]))?;
        match ev {
            Event::Eof => break,
            Event::Decl(d) => {
                newline(&mut out, depth);
                let _ = write!(out, "<?{}?>", s(&d));
            }
            Event::PI(p) => {
                newline(&mut out, depth);
                let _ = write!(out, "<?{}?>", s(&p));
            }
            Event::DocType(d) => {
                newline(&mut out, depth);
                let _ = write!(out, "<!DOCTYPE {}>", s(&d));
            }
            Event::Comment(c) => {
                newline(&mut out, depth);
                let _ = write!(out, "<!--{}-->", s(&c));
                if let Some(top) = stack.last_mut() {
                    *top = true;
                }
            }
            Event::Start(e) => {
                if let Some(top) = stack.last_mut() {
                    *top = true;
                }
                newline(&mut out, depth);
                let _ = write!(out, "<{}>", s(&e));
                stack.push(false);
                depth += 1;
                inline = false;
            }
            Event::Empty(e) => {
                if let Some(top) = stack.last_mut() {
                    *top = true;
                }
                newline(&mut out, depth);
                let _ = write!(out, "<{}/>", s(&e));
                inline = false;
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                let had_children = stack.pop().unwrap_or(false);
                if had_children && !inline {
                    newline(&mut out, depth);
                }
                let _ = write!(out, "</{}>", s(&e));
                inline = false;
            }
            Event::Text(t) => {
                let raw = s(&t);
                if raw.trim().is_empty() {
                    continue;
                }
                let has_children = stack.last().copied().unwrap_or(false);
                if has_children && !inline {
                    newline(&mut out, depth);
                    out.push_str(raw.trim());
                } else {
                    out.push_str(if inline { &raw } else { raw.trim_start() });
                }
                inline = true;
            }
            Event::GeneralRef(r) => {
                let _ = write!(out, "&{};", s(&r));
                inline = true;
            }
            Event::CData(c) => {
                let _ = write!(out, "<![CDATA[{}]]>", s(&c));
                inline = true;
            }
        }
    }
    if !stack.is_empty() {
        return Err(tr("Invalid XML: unclosed element").to_string());
    }
    Ok(out)
}

/// List array elements one per line: `[1] value`.
pub fn array_lines(items: &[Value]) -> String {
    let mut out = String::new();
    for (i, v) in items.iter().enumerate() {
        let text = match v {
            Value::Null => "NULL".to_string(),
            other => other.to_text().into_owned(),
        };
        let _ = writeln!(out, "[{}] {}", i + 1, text);
    }
    out
}

/// Split an EWKT geometry text into (SRID, WKT).
pub fn split_ewkt(text: &str) -> (Option<String>, &str) {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("SRID=").or_else(|| t.strip_prefix("srid="))
        && let Some((srid, wkt)) = rest.split_once(';')
    {
        return (Some(srid.to_string()), wkt.trim());
    }
    (None, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_detection() {
        assert_eq!(detect_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some(ImageKind::Png));
        assert_eq!(detect_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageKind::Jpeg));
        assert_eq!(detect_image(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(detect_image(b"RIFF\0\0\0\0WEBPVP8 "), Some(ImageKind::WebP));
        assert_eq!(detect_image(b"hello"), None);
        assert!(decode_image(b"hello", 64).is_err());
    }

    #[test]
    fn png_roundtrip_decode() {
        // Encode a tiny PNG with the image crate and decode it back, downscaled.
        let img = image::RgbaImage::from_fn(40, 20, |x, _| image::Rgba([x as u8 * 6, 0, 0, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
        let d = decode_image(&buf, 10).unwrap();
        assert_eq!(d.kind, ImageKind::Png);
        assert_eq!(d.original_size, [40, 20]);
        assert_eq!(d.image.size, [10, 5]);
        assert!(decode_image(&buf[..30], 10).is_err());
    }

    #[test]
    fn hex_dump_format() {
        let d = hex_dump(b"Hello, world!\x00\x01\x02xyz", 1024);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 02  |Hello, world!...|"));
        assert!(lines[1].starts_with("00000010  78 79 7a"));
        assert!(lines[1].ends_with("|xyz|"));
        let d = hex_dump(&[0u8; 40], 16);
        assert!(d.contains("24 more bytes"), "{d}");
        assert_eq!(byte_size_text(10), "10 B");
        assert_eq!(byte_size_text(1536), "1.5 KB");
    }

    #[test]
    fn json_and_xml_pretty() {
        assert_eq!(pretty_json(r#"{"b":1,"a":[1,2]}"#).unwrap(), "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert!(pretty_json("{bad").is_err());
        let x = pretty_xml("<?xml version=\"1.0\"?><a x=\"1\"><b>Tom &amp; Jerry</b><c/><!--n--></a>").unwrap();
        assert_eq!(x, "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>Tom &amp; Jerry</b>\n  <c/>\n  <!--n-->\n</a>");
        let x = pretty_xml("<r>\n   <i>1</i>\n   <i>2</i>\n</r>").unwrap();
        assert_eq!(x, "<r>\n  <i>1</i>\n  <i>2</i>\n</r>");
        assert!(pretty_xml("<a><b></a>").is_err());
        assert!(pretty_xml("<a>").is_err());
    }

    #[test]
    fn misc() {
        assert_eq!(array_lines(&[Value::Int(1), Value::Null]), "[1] 1\n[2] NULL\n");
        assert_eq!(split_ewkt("SRID=4326;POINT(1 2)"), (Some("4326".to_string()), "POINT(1 2)"));
        assert_eq!(split_ewkt("POINT(1 2)"), (None, "POINT(1 2)"));
    }
}
