//! Fonts, light/dark visuals, accessibility tweaks and connection-type colours.

use egui::{Color32, Context, FontDefinitions, Stroke, Theme, ThemePreference};

/// User theme choice (stored in `settings.json`).
pub use dbiji_config::settings::Theme as ThemeChoice;

/// Install the default fonts plus the Phosphor icon font as a fallback, so icons
/// can be mixed into any label.
pub fn setup_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
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
            if settings.accessibility.reduce_motion {
                style.animation_time = 0.0;
            }
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
            let v = &mut style.visuals;
            v.window_highlight_topmost = true;
            if high_contrast {
                let (fg, bg) = if v.dark_mode {
                    (Color32::WHITE, Color32::BLACK)
                } else {
                    (Color32::BLACK, Color32::WHITE)
                };
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

pub fn success_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(110, 200, 120) } else { Color32::from_rgb(30, 130, 50) }
}

pub fn warning_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(240, 190, 90) } else { Color32::from_rgb(180, 110, 0) }
}

pub fn error_color(dark_mode: bool) -> Color32 {
    if dark_mode { Color32::from_rgb(255, 120, 110) } else { Color32::from_rgb(190, 30, 30) }
}
