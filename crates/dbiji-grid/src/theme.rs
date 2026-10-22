//! Colours of the grid and views, derived from the current [`egui::Visuals`]
//! so light and dark themes both work.

use egui::{Color32, Visuals};

/// Linear blend of two colours in gamma space (`t` = 0 → `a`, 1 → `b`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Grid colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridTheme {
    /// Cell background.
    pub bg: Color32,
    /// Alternate row background.
    pub stripe: Color32,
    /// Grid lines.
    pub line: Color32,
    /// Header / row number background.
    pub header_bg: Color32,
    /// Header text.
    pub header_text: Color32,
    /// Header background of selected rows / columns.
    pub header_selected_bg: Color32,
    /// Row numbers.
    pub rownum_text: Color32,
    /// Values of the record view (one colour, no stripes).
    pub record_bg: Color32,
    /// Normal text.
    pub text: Color32,
    /// Weak text (NULL, defaults, descriptions).
    pub weak: Color32,
    /// Selected cells overlay.
    pub selection: Color32,
    /// Active cell border.
    pub active_border: Color32,
    /// Modified cell background.
    pub modified: Color32,
    /// Inserted row background.
    pub inserted: Color32,
    /// Deleted row background.
    pub deleted: Color32,
    /// Foreign key links.
    pub link: Color32,
    /// Badge background.
    pub badge_bg: Color32,
    /// Badge text.
    pub badge_text: Color32,
    /// Error text.
    pub error: Color32,
    /// Warning / read-only accent.
    pub warn: Color32,
    /// `true` values.
    pub bool_true: Color32,
}

impl GridTheme {
    /// Theme for `visuals`.
    pub fn from_visuals(v: &Visuals) -> Self {
        let bg = v.extreme_bg_color;
        let text = v.text_color();
        let dark = v.dark_mode;
        let green = if dark { Color32::from_rgb(0x3f, 0xb9, 0x50) } else { Color32::from_rgb(0x1a, 0x7f, 0x37) };
        let header_base = v.widgets.noninteractive.weak_bg_fill;
        GridTheme {
            bg,
            stripe: mix(bg, text, if dark { 0.053 } else { 0.03 }),
            // Darker than the rows in the dark theme (as in DBeaver), else darker than white.
            line: if dark { mix(bg, Color32::BLACK, 0.17) } else { mix(bg, text, 0.12) },
            header_bg: mix(header_base, bg, 0.25),
            header_text: v.strong_text_color(),
            // DBeaver's grey-violet in the dark theme (selected headers, the record view's field).
            header_selected_bg: if dark { Color32::from_rgb(0x5F, 0x5F, 0x6F) } else { mix(header_base, v.selection.bg_fill, 0.45) },
            // White on the dark blue-grey header (as in DBeaver), grey on the light one.
            rownum_text: if dark { v.strong_text_color() } else { v.weak_text_color() },
            record_bg: if dark { mix(bg, text, 0.053) } else { bg },
            text,
            weak: v.weak_text_color(),
            selection: v.selection.bg_fill.gamma_multiply(if dark { 0.55 } else { 0.45 }),
            active_border: v.selection.stroke.color,
            modified: mix(bg, v.warn_fg_color, if dark { 0.28 } else { 0.30 }),
            inserted: mix(bg, green, if dark { 0.26 } else { 0.20 }),
            deleted: mix(bg, v.error_fg_color, if dark { 0.26 } else { 0.18 }),
            link: v.hyperlink_color,
            badge_bg: mix(bg, text, 0.13),
            badge_text: mix(text, bg, 0.25),
            error: v.error_fg_color,
            warn: v.warn_fg_color,
            bool_true: green,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(200, 100, 50);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(100, 50, 25));
        let light = GridTheme::from_visuals(&Visuals::light());
        let dark = GridTheme::from_visuals(&Visuals::dark());
        assert_ne!(light.bg, dark.bg);
        assert_ne!(light.modified, light.bg);
    }
}
