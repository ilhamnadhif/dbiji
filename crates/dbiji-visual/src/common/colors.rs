//! Color roles shared by all visual tools.
//!
//! Chrome (backgrounds, text, strokes) always comes from `ui.visuals()`; the
//! constants here only cover data colors: a validated categorical palette
//! (fixed slot order, separate steps for light and dark surfaces), a heat
//! scale for "how expensive is this" encodings and the reserved status colors.

use egui::{Color32, Visuals};

/// Categorical slots for light surfaces (blue, orange, aqua, yellow, magenta,
/// green, violet, red). The order is part of the color-vision-deficiency
/// safety of the palette: never re-order or cycle it.
pub(crate) const CATEGORICAL_LIGHT: [Color32; 8] = [
    Color32::from_rgb(0x2a, 0x78, 0xd6),
    Color32::from_rgb(0xeb, 0x68, 0x34),
    Color32::from_rgb(0x1b, 0xaf, 0x7a),
    Color32::from_rgb(0xed, 0xa1, 0x00),
    Color32::from_rgb(0xe8, 0x7b, 0xa4),
    Color32::from_rgb(0x00, 0x83, 0x00),
    Color32::from_rgb(0x62, 0x50, 0xd6),
    Color32::from_rgb(0xe3, 0x49, 0x48),
];

/// The same eight hues stepped for dark surfaces.
pub(crate) const CATEGORICAL_DARK: [Color32; 8] = [
    Color32::from_rgb(0x39, 0x87, 0xe5),
    Color32::from_rgb(0xd9, 0x59, 0x26),
    Color32::from_rgb(0x19, 0x9e, 0x70),
    Color32::from_rgb(0xc9, 0x85, 0x00),
    Color32::from_rgb(0xd5, 0x51, 0x81),
    Color32::from_rgb(0x00, 0x83, 0x00),
    Color32::from_rgb(0x90, 0x85, 0xe9),
    Color32::from_rgb(0xe6, 0x67, 0x67),
];

/// Number of categorical slots; series beyond this fold into "Other".
pub(crate) const CATEGORICAL_SLOTS: usize = 8;

/// Neutral color used for the "Other" bucket.
pub(crate) const OTHER_LIGHT: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
pub(crate) const OTHER_DARK: Color32 = Color32::from_rgb(0x6f, 0x6e, 0x69);

/// Status colors (never used for series).
pub(crate) const STATUS_GOOD: Color32 = Color32::from_rgb(0x0c, 0xa3, 0x0c);
pub(crate) const STATUS_WARNING: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
pub(crate) const STATUS_SERIOUS: Color32 = Color32::from_rgb(0xec, 0x83, 0x5a);
pub(crate) const STATUS_CRITICAL: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);

/// Categorical color for slot `i` (slots past the palette wrap; callers that
/// encode identity fold extra series into "Other" before asking).
pub(crate) fn categorical(i: usize, dark: bool) -> Color32 {
    let table = if dark { &CATEGORICAL_DARK } else { &CATEGORICAL_LIGHT };
    table[i % CATEGORICAL_SLOTS]
}

/// Color of the "Other" bucket.
pub(crate) fn other(dark: bool) -> Color32 {
    if dark { OTHER_DARK } else { OTHER_LIGHT }
}

/// Heat scale for shares in `0..=1`: green (cheap) -> yellow -> red (expensive).
pub(crate) fn heat(t: f32) -> Color32 {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    if t < 0.5 {
        STATUS_GOOD.lerp_to_gamma(STATUS_WARNING, t * 2.0)
    } else {
        STATUS_WARNING.lerp_to_gamma(STATUS_CRITICAL, (t - 0.5) * 2.0)
    }
}

/// Mixes `color` into the panel background, for tinted fills that keep text
/// readable in both themes.
pub(crate) fn tint(visuals: &Visuals, color: Color32, amount: f32) -> Color32 {
    visuals.panel_fill.lerp_to_gamma(color, amount.clamp(0.0, 1.0))
}

/// White or near-black text, whichever reads better on `fill`.
pub(crate) fn text_on(fill: Color32) -> Color32 {
    let [r, g, b, _] = fill.to_srgba_unmultiplied();
    let lum = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    if lum > 150.0 { Color32::from_rgb(0x0b, 0x0b, 0x0b) } else { Color32::WHITE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_endpoints() {
        assert_eq!(heat(0.0), STATUS_GOOD);
        assert_eq!(heat(1.0), STATUS_CRITICAL);
        assert_eq!(heat(f32::NAN), STATUS_GOOD);
        assert_eq!(heat(7.0), STATUS_CRITICAL);
    }

    #[test]
    fn palette_slots_are_distinct() {
        for table in [&CATEGORICAL_LIGHT, &CATEGORICAL_DARK] {
            for i in 0..CATEGORICAL_SLOTS {
                for j in i + 1..CATEGORICAL_SLOTS {
                    assert_ne!(table[i], table[j]);
                }
            }
        }
        assert_eq!(categorical(8, false), categorical(0, false));
        assert_eq!(text_on(Color32::WHITE), Color32::from_rgb(0x0b, 0x0b, 0x0b));
        assert_eq!(text_on(Color32::BLACK), Color32::WHITE);
    }
}
