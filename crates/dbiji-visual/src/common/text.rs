//! Font independent text width estimation.
//!
//! Diagram geometry (box sizes, layout, SVG export) must not depend on the
//! fonts installed in the UI, otherwise a saved diagram would look different
//! on every machine and layouts would not be reproducible in tests. Widths are
//! therefore estimated from the standard Helvetica advance widths (AFM metrics,
//! 1/1000 em), which are also what SVG viewers use for the
//! `Helvetica, Arial, sans-serif` font stack of exported images.

use std::borrow::Cow;

/// Helvetica advance widths for ASCII 32..=126.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278,
    278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // '0'..'9'
    278, 278, 584, 584, 584, 556, 1015, // ':'..'@'
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, // 'A'..'M'
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, // 'N'..'Z'
    278, 278, 278, 469, 556, 333, // '['..'`'
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, // 'a'..'m'
    556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, // 'n'..'z'
    334, 260, 334, 584, // '{'..'~'
];

/// Helvetica-Bold advance widths for ASCII 32..=126.
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278,
    278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // '0'..'9'
    333, 333, 584, 584, 584, 611, 975, // ':'..'@'
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, // 'A'..'M'
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, // 'N'..'Z'
    333, 278, 333, 584, 556, 333, // '['..'`'
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, // 'a'..'m'
    611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, // 'n'..'z'
    389, 280, 389, 584, // '{'..'~'
];

/// Safety factor applied on top of the Helvetica metrics so that the UI font
/// (which may be slightly wider) still fits inside boxes sized by the estimate.
pub(crate) const UI_SAFETY: f32 = 1.06;

/// East Asian wide / full-width characters and emoji occupy a full em.
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

/// Advance width of one character in em units.
pub(crate) fn char_width_em(c: char, bold: bool) -> f32 {
    let code = c as u32;
    if (32..=126).contains(&code) {
        let table = if bold { &HELVETICA_BOLD } else { &HELVETICA };
        let idx = (code - 32) as usize;
        return table.get(idx).copied().unwrap_or(556) as f32 / 1000.0;
    }
    if c.is_control() {
        0.0
    } else if is_wide(c) {
        1.0
    } else if c.is_whitespace() {
        0.278
    } else if bold {
        0.62
    } else {
        0.58
    }
}

/// Estimated width of `text` in points at `font_size`.
pub(crate) fn text_width(text: &str, font_size: f32, bold: bool) -> f32 {
    text.chars().map(|c| char_width_em(c, bold)).sum::<f32>() * font_size
}

/// Truncates `text` with an ellipsis so that its estimated width does not
/// exceed `max_width`.
pub(crate) fn truncate_to_width(
    text: &str,
    max_width: f32,
    font_size: f32,
    bold: bool,
) -> Cow<'_, str> {
    if text_width(text, font_size, bold) <= max_width {
        return Cow::Borrowed(text);
    }
    let ellipsis = char_width_em('…', bold) * font_size;
    let mut width = 0.0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        let w = char_width_em(c, bold) * font_size;
        if width + w + ellipsis > max_width {
            break;
        }
        width += w;
        end = i + c.len_utf8();
    }
    Cow::Owned(format!("{}…", &text[..end]))
}

/// Truncates to at most `max_chars` characters (ellipsis included).
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> Cow<'_, str> {
    if text.chars().count() <= max_chars {
        return Cow::Borrowed(text);
    }
    let keep: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    Cow::Owned(format!("{keep}…"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_plausible() {
        // "Hello" in Helvetica 1000 units: 722+556+222+222+556 = 2278
        let w = text_width("Hello", 10.0, false);
        assert!((w - 22.78).abs() < 0.01, "{w}");
        assert!(text_width("Hello", 10.0, true) > w);
        assert_eq!(text_width("", 12.0, false), 0.0);
        assert!(text_width("数据库", 10.0, false) >= 30.0);
    }

    #[test]
    fn truncation() {
        let t = truncate_to_width("a_very_long_column_name_indeed", 60.0, 12.0, false);
        assert!(t.ends_with('…'));
        assert!(text_width(&t, 12.0, false) <= 60.0 + 0.001);
        assert_eq!(truncate_to_width("id", 60.0, 12.0, false), "id");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("abc", 4), "abc");
        // multi-byte characters never split
        let t = truncate_to_width("ééééééééééééééé", 30.0, 12.0, false);
        assert!(t.ends_with('…'));
    }
}
