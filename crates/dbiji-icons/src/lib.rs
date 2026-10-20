//! Icon colours, DBeaver-style: folders orange, tables blue, keys gold,
//! "new" green, "delete" red … so icons can be told apart at a glance. One
//! palette serves both themes: every colour has a contrast of about 3:1 or
//! more against the light and the dark panel background.

use egui::{Color32, FontFamily, FontId, RichText};
use egui_phosphor::regular as i;

const fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

pub const ORANGE: Color32 = rgb(0xD97706);
pub const BLUE: Color32 = rgb(0x3B82F6);
pub const INDIGO: Color32 = rgb(0x6366F1);
pub const TEAL: Color32 = rgb(0x0D9488);
pub const CYAN: Color32 = rgb(0x0891B2);
pub const SLATE: Color32 = rgb(0x64748B);
pub const GOLD: Color32 = rgb(0xB88300);
pub const GREEN: Color32 = rgb(0x16A34A);
pub const PURPLE: Color32 = rgb(0xA855F7);
pub const RED: Color32 = rgb(0xDC2626);
pub const MAGENTA: Color32 = rgb(0xC026D3);
pub const ROSE: Color32 = rgb(0xE11D48);
pub const BROWN: Color32 = rgb(0xA16207);
pub const VIOLET: Color32 = rgb(0x8B5CF6);
pub const SKY: Color32 = rgb(0x0284C7);

/// The colour of an icon glyph (`egui_phosphor::regular`), `None` for icons
/// drawn in the text colour.
pub fn color(glyph: &str) -> Option<Color32> {
    Some(match glyph {
        // Folders and groups.
        i::FOLDER | i::FOLDER_OPEN | i::FOLDER_SIMPLE | i::FOLDERS | i::FOLDER_PLUS => ORANGE,
        // Database objects.
        i::DATABASE => SKY,
        i::STACK => INDIGO,
        i::TABLE | i::ROWS => BLUE,
        i::EYE | i::EYE_CLOSED => TEAL,
        i::GLOBE => CYAN,
        i::COLUMNS => SLATE,
        i::LIGHTNING => GOLD,
        i::KEY | i::FINGERPRINT => GOLD,
        i::LINK => PURPLE,
        i::LIGHTNING_A => ROSE,
        i::LIST_NUMBERS => GREEN,
        i::FUNCTION | i::GEAR_SIX => MAGENTA,
        i::SHAPES => CYAN,
        i::PUZZLE_PIECE => GREEN,
        i::CALENDAR => ROSE,
        i::USER | i::USERS => INDIGO,
        i::HARD_DRIVES | i::HARD_DRIVE => BROWN,
        i::SHIELD_CHECK => GREEN,
        i::WRENCH => SLATE,
        i::INFO => SKY,
        i::SLIDERS => SLATE,
        i::PLUG | i::PLUGS_CONNECTED => GREEN,
        i::PLUGS => RED,
        i::CODE => VIOLET,
        i::GAUGE | i::CHART_BAR | i::CHART_LINE | i::CHART_PIE => ORANGE,
        i::FILE_SQL | i::TERMINAL => SKY,
        i::FILE_ZIP | i::FILE_CSV | i::FILE_TEXT => BROWN,
        i::GRAPH | i::TREE_STRUCTURE => PURPLE,
        // Actions.
        i::PLUS
        | i::PLUS_CIRCLE
        | i::PLAY
        | i::PLAY_CIRCLE
        | i::CHECK
        | i::CHECK_CIRCLE
        | i::CHECK_SQUARE => GREEN,
        i::TRASH
        | i::STOP
        | i::STOP_CIRCLE
        | i::X_CIRCLE
        | i::MINUS_CIRCLE
        | i::WARNING_OCTAGON => RED,
        i::PENCIL | i::PENCIL_SIMPLE | i::NOTE_PENCIL => GOLD,
        i::ARROW_CLOCKWISE | i::ARROWS_CLOCKWISE | i::ARROW_COUNTER_CLOCKWISE => SKY,
        i::EXPORT | i::UPLOAD_SIMPLE | i::DOWNLOAD | i::DOWNLOAD_SIMPLE => TEAL,
        i::FLOPPY_DISK => BLUE,
        i::GIT_DIFF | i::ARROWS_LEFT_RIGHT => ORANGE,
        i::FUNNEL | i::FUNNEL_X | i::FUNNEL_SIMPLE => VIOLET,
        i::SPARKLE | i::ROBOT | i::MAGIC_WAND => VIOLET,
        i::CLOCK_COUNTER_CLOCKWISE | i::CLOCK | i::HOURGLASS => GOLD,
        i::WARNING => ORANGE,
        i::ARROW_CIRCLE_UP => GREEN,
        i::HOUSE => ORANGE,
        i::GEAR => SLATE,
        _ => return None,
    })
}

/// Font family of the filled Phosphor icons (the navigator's objects).
pub const FILL_FAMILY: &str = "phosphor-fill";

/// Register the filled icons as [`FILL_FAMILY`] (the regular ones stay the
/// fallback of proportional text).
pub fn add_fill_font(fonts: &mut egui::FontDefinitions) {
    egui_phosphor::add_font_bytes_as_family(
        fonts,
        FILL_FAMILY,
        egui_phosphor::Variant::Fill.font_bytes(),
    );
}

/// The filled icons at this size; the regular ones when the filled font is
/// not registered.
pub fn fill_font(ctx: &egui::Context, size: f32) -> FontId {
    let family = FontFamily::Name(FILL_FAMILY.into());
    if ctx.fonts(|f| f.definitions().families.contains_key(&family)) {
        FontId::new(size, family)
    } else {
        FontId::proportional(size)
    }
}

/// An icon as a coloured text atom: `ui.button((ic(icon::TABLE), "Tables"))`
/// colours the icon and keeps the label in the widget's text colour.
pub fn ic(glyph: &str) -> RichText {
    match color(glyph) {
        Some(c) => RichText::new(glyph).color(c),
        None => RichText::new(glyph),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(color(i::FOLDER), Some(ORANGE));
        assert_eq!(color(i::TABLE), Some(BLUE));
        assert_eq!(color(i::TRASH), Some(RED));
        assert_eq!(color(i::COPY), None);
        assert_eq!(ic(i::TABLE).text(), i::TABLE);
    }
}
