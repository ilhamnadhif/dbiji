//! Minimal SVG document writer used by the image exports.
//!
//! Every exported document is self-contained (no external references), uses
//! the `Helvetica, Arial, sans-serif` font stack and escapes all text so the
//! output is always well-formed XML.

use egui::{Color32, Pos2};
use std::fmt::Write as _;

/// Font stack used by every exported SVG.
pub(crate) const FONT_FAMILY: &str = "Helvetica, Arial, sans-serif";

/// Escapes text for use in XML character data and attribute values and drops
/// characters that are not allowed in XML 1.0 documents.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            '\u{FFFE}' | '\u{FFFF}' => {}
            c if (0xD800..=0xDFFF).contains(&(c as u32)) => {}
            c => out.push(c),
        }
    }
    out
}

/// Formats a coordinate with at most two decimals (no trailing zeros).
pub(crate) fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() {
        "0".into()
    } else {
        s.to_string()
    }
}

/// `#rrggbb` for a color (alpha handled separately with [`opacity`]).
pub(crate) fn color(c: Color32) -> String {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Opacity of a color in 0..=1.
pub(crate) fn opacity(c: Color32) -> f32 {
    c.to_srgba_unmultiplied()[3] as f32 / 255.0
}

fn paint_attr(name: &str, c: Option<Color32>) -> String {
    match c {
        None => format!(" {name}=\"none\""),
        Some(c) => {
            let a = opacity(c);
            if a >= 0.999 {
                format!(" {name}=\"{}\"", color(c))
            } else {
                format!(" {name}=\"{}\" {name}-opacity=\"{}\"", color(c), num(a))
            }
        }
    }
}

/// Text anchoring for [`SvgDoc::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    Start,
    Middle,
    End,
}

impl Anchor {
    fn attr(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Middle => "middle",
            Anchor::End => "end",
        }
    }
}

/// Text style for [`SvgDoc::text`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextStyle {
    pub size: f32,
    pub color: Color32,
    pub bold: bool,
    pub italic: bool,
    pub anchor: Anchor,
}

impl TextStyle {
    pub fn new(size: f32, color: Color32) -> Self {
        TextStyle {
            size,
            color,
            bold: false,
            italic: false,
            anchor: Anchor::Start,
        }
    }
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }
}

/// An SVG document under construction.
pub(crate) struct SvgDoc {
    buf: String,
}

impl SvgDoc {
    /// Starts a document of `width` x `height` user units, optionally filled
    /// with a background color.
    pub fn new(width: f32, height: f32, background: Option<Color32>) -> Self {
        let (w, h) = (num(width.max(1.0)), num(height.max(1.0)));
        let mut buf = String::with_capacity(64 * 1024);
        buf.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            buf,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"{FONT_FAMILY}\">"
        );
        let mut doc = SvgDoc { buf };
        if let Some(bg) = background {
            doc.rect(
                0.0,
                0.0,
                width.max(1.0),
                height.max(1.0),
                0.0,
                Some(bg),
                None,
            );
        }
        doc
    }

    /// Adds a `<title>` element (accessible name of the image).
    pub fn title(&mut self, title: &str) {
        let _ = writeln!(self.buf, "<title>{}</title>", escape(title));
    }

    /// Rectangle with optional corner radius, fill and stroke.
    #[allow(clippy::too_many_arguments)]
    pub fn rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        rx: f32,
        fill: Option<Color32>,
        stroke: Option<(Color32, f32)>,
    ) {
        let _ = write!(
            self.buf,
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"",
            num(x),
            num(y),
            num(w.max(0.0)),
            num(h.max(0.0))
        );
        if rx > 0.0 {
            let _ = write!(self.buf, " rx=\"{}\"", num(rx));
        }
        self.buf.push_str(&paint_attr("fill", fill));
        if let Some((c, width)) = stroke {
            self.buf.push_str(&paint_attr("stroke", Some(c)));
            let _ = write!(self.buf, " stroke-width=\"{}\"", num(width));
        }
        self.buf.push_str("/>\n");
    }

    /// Path given as SVG path data.
    pub fn path(
        &mut self,
        d: &str,
        fill: Option<Color32>,
        stroke: Option<(Color32, f32)>,
        dash: Option<&str>,
    ) {
        let _ = write!(self.buf, "<path d=\"{}\"", escape(d));
        self.buf.push_str(&paint_attr("fill", fill));
        if let Some((c, width)) = stroke {
            self.buf.push_str(&paint_attr("stroke", Some(c)));
            let _ = write!(
                self.buf,
                " stroke-width=\"{}\" stroke-linejoin=\"round\" stroke-linecap=\"round\"",
                num(width)
            );
        }
        if let Some(d) = dash {
            let _ = write!(self.buf, " stroke-dasharray=\"{}\"", escape(d));
        }
        self.buf.push_str("/>\n");
    }

    /// Open polyline.
    pub fn polyline(&mut self, points: &[Pos2], stroke: Color32, width: f32, dash: Option<&str>) {
        if points.len() < 2 {
            return;
        }
        let d = polyline_path(points, false);
        self.path(&d, None, Some((stroke, width)), dash);
    }

    /// Closed polygon.
    pub fn polygon(
        &mut self,
        points: &[Pos2],
        fill: Option<Color32>,
        stroke: Option<(Color32, f32)>,
    ) {
        if points.len() < 3 {
            return;
        }
        let d = polyline_path(points, true);
        self.path(&d, fill, stroke, None);
    }

    /// Straight line segment.
    pub fn line(&mut self, a: Pos2, b: Pos2, stroke: Color32, width: f32) {
        let _ = write!(
            self.buf,
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"",
            num(a.x),
            num(a.y),
            num(b.x),
            num(b.y)
        );
        self.buf.push_str(&paint_attr("stroke", Some(stroke)));
        let _ = writeln!(self.buf, " stroke-width=\"{}\"/>", num(width));
    }

    /// Circle.
    pub fn circle(
        &mut self,
        c: Pos2,
        r: f32,
        fill: Option<Color32>,
        stroke: Option<(Color32, f32)>,
    ) {
        let _ = write!(
            self.buf,
            "<circle cx=\"{}\" cy=\"{}\" r=\"{}\"",
            num(c.x),
            num(c.y),
            num(r.max(0.0))
        );
        self.buf.push_str(&paint_attr("fill", fill));
        if let Some((c, width)) = stroke {
            self.buf.push_str(&paint_attr("stroke", Some(c)));
            let _ = write!(self.buf, " stroke-width=\"{}\"", num(width));
        }
        self.buf.push_str("/>\n");
    }

    /// Single line of text with its baseline at `y`.
    pub fn text(&mut self, x: f32, y: f32, text: &str, style: TextStyle) {
        if text.is_empty() {
            return;
        }
        let _ = write!(
            self.buf,
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"{}\"",
            num(x),
            num(y),
            num(style.size),
            style.anchor.attr()
        );
        self.buf.push_str(&paint_attr("fill", Some(style.color)));
        if style.bold {
            self.buf.push_str(" font-weight=\"bold\"");
        }
        if style.italic {
            self.buf.push_str(" font-style=\"italic\"");
        }
        let _ = writeln!(self.buf, ">{}</text>", escape(text));
    }

    /// Opens a group with a `<title>` tooltip (hover text in browsers).
    pub fn open_group(&mut self, title: Option<&str>) {
        self.buf.push_str("<g>\n");
        if let Some(t) = title {
            let _ = writeln!(self.buf, "<title>{}</title>", escape(t));
        }
    }

    /// Closes a group opened with [`Self::open_group`].
    pub fn close_group(&mut self) {
        self.buf.push_str("</g>\n");
    }

    /// Finishes the document.
    pub fn finish(mut self) -> String {
        self.buf.push_str("</svg>\n");
        self.buf
    }
}

/// SVG path data for a polyline.
pub(crate) fn polyline_path(points: &[Pos2], closed: bool) -> String {
    let mut d = String::with_capacity(points.len() * 14);
    for (i, p) in points.iter().enumerate() {
        let _ = write!(
            d,
            "{}{} {} ",
            if i == 0 { 'M' } else { 'L' },
            num(p.x),
            num(p.y)
        );
    }
    if closed {
        d.push('Z');
    }
    d.trim_end().to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Very small XML well-formedness checker used by the export tests:
    /// verifies tag nesting, attribute quoting and entity syntax.
    pub(crate) fn assert_well_formed(xml: &str) {
        let body = xml.trim_start();
        let body = match body.strip_prefix("<?xml") {
            Some(rest) => match rest.find("?>") {
                Some(end) => &rest[end + 2..],
                None => panic!("unterminated XML declaration"),
            },
            None => body,
        };
        let mut stack: Vec<String> = Vec::new();
        let bytes = body.as_bytes();
        let mut i = 0;
        let mut roots = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' {
                let end = body[i..]
                    .find('>')
                    .map(|e| i + e)
                    .unwrap_or_else(|| panic!("unterminated tag at {i}"));
                let tag = &body[i + 1..end];
                if let Some(name) = tag.strip_prefix('/') {
                    let open = stack
                        .pop()
                        .unwrap_or_else(|| panic!("unbalanced </{name}>"));
                    assert_eq!(open, name.trim(), "mismatched closing tag");
                } else if !tag.starts_with('!') && !tag.starts_with('?') {
                    let self_closing = tag.ends_with('/');
                    let inner = tag.trim_end_matches('/');
                    let name = inner.split_whitespace().next().unwrap_or("").to_string();
                    assert!(!name.is_empty(), "empty tag name");
                    check_attributes(&inner[name.len()..]);
                    if stack.is_empty() {
                        roots += 1;
                    }
                    if !self_closing {
                        stack.push(name);
                    }
                }
                i = end + 1;
            } else {
                let next = body[i..].find('<').map(|e| i + e).unwrap_or(bytes.len());
                let text = &body[i..next];
                if stack.is_empty() {
                    assert!(text.trim().is_empty(), "text outside root: {text:?}");
                }
                check_entities(text);
                i = next;
            }
        }
        assert!(stack.is_empty(), "unclosed tags: {stack:?}");
        assert_eq!(roots, 1, "exactly one root element expected");
    }

    fn check_attributes(attrs: &str) {
        let mut rest = attrs.trim();
        while !rest.is_empty() {
            let eq = rest
                .find('=')
                .unwrap_or_else(|| panic!("attribute without value: {rest}"));
            let name = rest[..eq].trim();
            assert!(
                !name.is_empty() && !name.contains(char::is_whitespace),
                "bad attribute name {name:?}"
            );
            let after = rest[eq + 1..].trim_start();
            assert!(after.starts_with('"'), "unquoted attribute value: {after}");
            let close = after[1..]
                .find('"')
                .unwrap_or_else(|| panic!("unterminated attribute"));
            let value = &after[1..1 + close];
            assert!(!value.contains('<'), "'<' in attribute value");
            check_entities(value);
            rest = after[close + 2..].trim_start();
        }
    }

    fn check_entities(text: &str) {
        let mut rest = text;
        while let Some(pos) = rest.find('&') {
            let after = &rest[pos + 1..];
            let semi = after
                .find(';')
                .unwrap_or_else(|| panic!("bare '&' in {text:?}"));
            let ent = &after[..semi];
            assert!(
                matches!(ent, "amp" | "lt" | "gt" | "quot" | "apos") || ent.starts_with('#'),
                "unknown entity &{ent};"
            );
            rest = &after[semi + 1..];
        }
    }

    #[test]
    fn escaping_and_numbers() {
        assert_eq!(
            escape("a<b & \"c\" 'd'>"),
            "a&lt;b &amp; &quot;c&quot; &apos;d&apos;&gt;"
        );
        assert_eq!(escape("x\u{1}y\nz"), "xy z");
        assert_eq!(num(1.0), "1");
        assert_eq!(num(1.256), "1.26");
        assert_eq!(num(-0.001), "0");
        assert_eq!(num(f32::NAN), "0");
        assert_eq!(color(Color32::from_rgb(255, 0, 16)), "#ff0010");
    }

    #[test]
    fn document_is_well_formed() {
        let mut doc = SvgDoc::new(100.0, 50.0, Some(Color32::WHITE));
        doc.title("A & B");
        doc.rect(
            1.0,
            1.0,
            10.0,
            10.0,
            2.0,
            Some(Color32::from_rgba_unmultiplied(10, 20, 30, 128)),
            Some((Color32::BLACK, 1.0)),
        );
        doc.text(
            5.0,
            20.0,
            "<script>&",
            TextStyle::new(12.0, Color32::BLACK).bold(),
        );
        doc.polyline(
            &[Pos2::new(0.0, 0.0), Pos2::new(5.0, 5.0)],
            Color32::RED,
            2.0,
            Some("4 2"),
        );
        doc.open_group(Some("tip \"quoted\""));
        doc.circle(Pos2::new(3.0, 3.0), 2.0, None, Some((Color32::BLUE, 1.0)));
        doc.close_group();
        let svg = doc.finish();
        assert_well_formed(&svg);
        assert!(svg.contains("&lt;script&gt;&amp;"));
        assert!(svg.contains("fill-opacity=\"0.5\""));
    }
}
