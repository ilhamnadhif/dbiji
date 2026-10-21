//! Fonts, light/dark visuals, accessibility tweaks and connection-type colours.

use egui::{Color32, Context, FontData, FontDefinitions, FontFamily, Stroke, Theme, ThemePreference, Visuals};
use std::sync::Arc;

/// User theme choice (stored in `settings.json`).
pub use dbiji_config::settings::Theme as ThemeChoice;

/// Inter 4.1 Regular and Medium (SIL Open Font License, `assets/fonts/Inter-LICENSE.txt`):
/// a typeface designed for screens, crisp at UI sizes; egui's default UI font
/// is a light weight that looks faint and blurry. Subset without the private
/// use area, where the Phosphor icons live (`pyftsubset --unicodes=
/// U+0000-DFFF,U+F900-FFFF,U+10000-10FFFF --layout-features='*'`).
const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const INTER_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.ttf");

/// Fonts: Inter for the UI, Hack for code, then egui's defaults (emoji and
/// symbols) and the Phosphor icon font as fallbacks, so icons can be mixed
/// into any label. Inter is the Medium weight in the light theme: dark text
/// on a light background reads thinner than light text on a dark one.
pub fn font_definitions(theme: Theme) -> FontDefinitions {
    let inter = if theme == Theme::Light { INTER_MEDIUM } else { INTER };
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("Inter".into(), Arc::new(FontData::from_static(inter)));
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.retain(|f| f != "Ubuntu-Light");
    proportional.insert(0, "Inter".into());
    // Labels in monospace still find letters Hack lacks.
    fonts.families.entry(FontFamily::Monospace).or_default().push("Inter".into());
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    dbiji_icons::add_fill_font(&mut fonts);
    fonts
}

pub fn setup_fonts(ctx: &Context) {
    set_fonts(ctx, ctx.theme());
}

fn set_fonts(ctx: &Context, theme: Theme) {
    ctx.set_fonts(font_definitions(theme));
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("dbiji-font-theme"), theme));
}

/// Every frame: the UI font follows the theme, also when it follows the system's.
pub fn sync_fonts(ctx: &Context) {
    let theme = ctx.theme();
    if ctx.data(|d| d.get_temp::<Theme>(egui::Id::new("dbiji-font-theme"))) != Some(theme) {
        set_fonts(ctx, theme);
    }
}

/// Apply the appearance preferences: theme, contrast, zoom and UI font size.
pub fn apply_settings(ctx: &Context, settings: &dbiji_config::AppSettings) {
    apply_theme(ctx, settings.general.theme, settings.accessibility.high_contrast);
    let scale = settings.general.ui_scale.clamp(0.5, 3.0);
    if (ctx.zoom_factor() - scale).abs() > f32::EPSILON {
        ctx.set_zoom_factor(scale);
    }
    let size = settings.general.ui_font_size.clamp(8.0, 32.0);
    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            use egui::{FontFamily, FontId, TextStyle};
            style.text_styles.insert(TextStyle::Body, FontId::new(size, FontFamily::Proportional));
            style.text_styles.insert(TextStyle::Button, FontId::new(size, FontFamily::Proportional));
            style.text_styles.insert(TextStyle::Small, FontId::new((size - 3.0).max(7.0), FontFamily::Proportional));
            style.text_styles.insert(TextStyle::Heading, FontId::new(size + 5.0, FontFamily::Proportional));
            style.text_styles.insert(TextStyle::Monospace, FontId::new(size, FontFamily::Monospace));
            // Back to egui's default when switched off again.
            style.animation_time = if settings.accessibility.reduce_motion { 0.0 } else { egui::Style::default().animation_time };
        });
    }
}

/// Apply theme preference and DBiji's style adjustments to both themes.
pub fn apply_theme(ctx: &Context, choice: ThemeChoice, high_contrast: bool) {
    ctx.set_theme(match choice {
        ThemeChoice::System => ThemePreference::System,
        ThemeChoice::Light => ThemePreference::Light,
        ThemeChoice::Dark => ThemePreference::Dark,
    });
    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(6.0, 4.0);
            style.spacing.button_padding = egui::vec2(6.0, 2.0);
            style.spacing.interact_size.y = 20.0;
            style.interaction.tooltip_delay = 0.4;
            // Scroll bars stay visible (egui shows them only on hover), wider when used.
            style.spacing.scroll = egui::style::ScrollStyle {
                floating_width: 6.0,
                bar_width: 10.0,
                dormant_handle_opacity: 0.55,
                active_handle_opacity: 0.8,
                interact_handle_opacity: 1.0,
                foreground_color: true,
                ..egui::style::ScrollStyle::floating()
            };
            style.visuals = visuals(theme);
            let v = &mut style.visuals;
            v.window_highlight_topmost = true;
            if high_contrast {
                let (fg, bg) = if v.dark_mode { (Color32::WHITE, Color32::BLACK) } else { (Color32::BLACK, Color32::WHITE) };
                v.override_text_color = Some(fg);
                v.panel_fill = bg;
                v.window_fill = bg;
                v.extreme_bg_color = bg;
                v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, fg);
                v.widgets.inactive.bg_stroke = Stroke::new(1.0, fg);
                v.selection.stroke = Stroke::new(2.0, fg);
            } else {
                v.override_text_color = None;
            }
        });
    }
}

fn gray(v: u8) -> Color32 {
    Color32::from_gray(v)
}

fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// DBiji's palettes: near-black (light) / near-white (dark) text with a
/// solid secondary colour instead of egui's translucent grey, calm neutral
/// panels and a clear blue selection.
pub fn visuals(theme: Theme) -> Visuals {
    let dark = theme == Theme::Dark;
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    // Sharper glyphs: no fractional glyph positions (they blur stems).
    v.text_options.subpixel_binning = false;
    struct P {
        text: Color32,
        weak: Color32,
        strong: Color32,
        panel: Color32,
        window: Color32,
        field: Color32,
        faint: Color32,
        code: Color32,
        line: Color32,
        button: Color32,
        button_hover: Color32,
        button_active: Color32,
        border: Color32,
        border_hover: Color32,
        accent: Color32,
        selection: Color32,
        link: Color32,
        warn: Color32,
        error: Color32,
    }
    let p = if dark {
        P {
            text: hex(0xDFE1E5),
            weak: hex(0x9DA2AB),
            strong: hex(0xFFFFFF),
            panel: hex(0x1E1F22),
            window: hex(0x2B2D30),
            field: hex(0x1A1B1E),
            faint: hex(0x232428),
            code: hex(0x2B2D30),
            line: hex(0x393B40),
            button: hex(0x2B2D30),
            button_hover: hex(0x393B40),
            button_active: hex(0x43454A),
            border: hex(0x43454A),
            border_hover: hex(0x5A5D63),
            accent: hex(0x548AF7),
            selection: hex(0x2E436E),
            link: hex(0x6CA7FF),
            warn: hex(0xE0A64B),
            error: hex(0xF2766D),
        }
    } else {
        P {
            text: hex(0x1F2328),
            weak: hex(0x4A525C),
            strong: hex(0x000000),
            panel: hex(0xF3F4F6),
            window: hex(0xFFFFFF),
            field: hex(0xFFFFFF),
            faint: hex(0xF6F8FA),
            code: hex(0xEEF0F3),
            line: hex(0xD5DAE0),
            button: hex(0xE8EBEF),
            button_hover: hex(0xDCE1E7),
            button_active: hex(0xCED5DD),
            border: hex(0xC9D1D9),
            border_hover: hex(0x9AA5B1),
            accent: hex(0x0969DA),
            selection: hex(0xC8DEFF),
            link: hex(0x0969DA),
            warn: hex(0x9A6700),
            error: hex(0xCF222E),
        }
    };
    v.override_text_color = None;
    v.weak_text_color = Some(p.weak);
    v.panel_fill = p.panel;
    v.window_fill = p.window;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.extreme_bg_color = p.field;
    v.text_edit_bg_color = Some(p.field);
    v.faint_bg_color = p.faint;
    v.code_bg_color = p.code;
    v.hyperlink_color = p.link;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, if dark { p.strong } else { p.accent });
    let w = &mut v.widgets;
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.bg_fill = p.window;
    w.noninteractive.weak_bg_fill = if dark { gray(0x26) } else { hex(0xEAEDF1) };
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.bg_fill = p.button;
    w.inactive.weak_bg_fill = p.button;
    // Also the border of text fields: they must stand out in dialogs too.
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.hovered.fg_stroke = Stroke::new(1.5, p.strong);
    w.hovered.bg_fill = p.button_hover;
    w.hovered.weak_bg_fill = p.button_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_hover);
    w.active.fg_stroke = Stroke::new(2.0, p.strong);
    w.active.bg_fill = p.button_active;
    w.active.weak_bg_fill = p.button_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.open.fg_stroke = Stroke::new(1.0, p.strong);
    w.open.bg_fill = p.button_hover;
    w.open.weak_bg_fill = p.button_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    v
}

/// Colour of a connection type marker.
pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Marker colour suitable for the current theme: white "no colour" markers
/// become transparent so development connections are not highlighted.
pub fn connection_marker(c: [u8; 3]) -> Option<Color32> {
    if c == [255, 255, 255] { None } else { Some(rgb(c)) }
}

/// Softened version of a marker colour for backgrounds (tab strips, status bar).
pub fn tint(c: Color32, dark_mode: bool) -> Color32 {
    let alpha = if dark_mode { 70 } else { 90 };
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Fill of primary buttons (white text on it), blue in both themes.
pub fn accent(dark_mode: bool) -> Color32 {
    if dark_mode { hex(0x2F7BEA) } else { hex(0x0B6BDB) }
}

/// A database type's brand colour (connection icons), readable in both themes.
pub fn brand_color(kind: dbiji_core::DbKind) -> Color32 {
    use dbiji_core::DbKind as K;
    match kind {
        K::Postgres => hex(0x4479A1),
        K::CockroachDb => hex(0x7C5CFF),
        K::MySql => hex(0x0A84A0),
        K::MariaDb => hex(0xC0765A),
        K::Sqlite => hex(0x0F80CC),
        K::SqlServer => hex(0xCC2927),
        K::ClickHouse | K::DuckDb => dbiji_icons::GOLD,
        _ => dbiji_icons::SLATE,
    }
}

/// A database logo's colour: the brand colour, lighter on dark backgrounds.
pub fn logo_color(kind: dbiji_core::DbKind, dark_mode: bool) -> Color32 {
    let c = brand_color(kind);
    if dark_mode { crate::theme::lighten(c, 0.35) } else { c }
}

/// `c` mixed with white by `t` (0 … 1).
pub fn lighten(c: Color32, t: f32) -> Color32 {
    let mix = |a: u8| (a as f32 + (255.0 - a as f32) * t).round() as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

pub fn success_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(110, 200, 120) } else { Color32::from_rgb(30, 130, 50) }
}

pub fn warning_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(240, 190, 90) } else { Color32::from_rgb(180, 110, 0) }
}

pub fn error_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(255, 120, 110) } else { Color32::from_rgb(190, 30, 30) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inter(ctx: &Context) -> usize {
        ctx.data(|d| d.get_temp::<Theme>(egui::Id::new("dbiji-font-theme"))).map_or(0, |t| font_definitions(t).font_data["Inter"].font.len())
    }

    #[test]
    fn light_theme_uses_a_heavier_inter() {
        assert_eq!(font_definitions(Theme::Light).font_data["Inter"].font.len(), INTER_MEDIUM.len());
        assert_eq!(font_definitions(Theme::Dark).font_data["Inter"].font.len(), INTER.len());
        let ctx = Context::default();
        ctx.set_theme(Theme::Dark);
        sync_fonts(&ctx);
        assert_eq!(inter(&ctx), INTER.len());
        ctx.set_theme(Theme::Light);
        sync_fonts(&ctx);
        assert_eq!(inter(&ctx), INTER_MEDIUM.len());
    }
}
