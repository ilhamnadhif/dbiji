//! Editor colors for light and dark mode.

use egui::{Color32, Visuals};

use crate::language::TokenStyle;

/// All colors used by the editor and [`crate::code_view`].
///
/// Use [`EditorTheme::light`] / [`EditorTheme::dark`] or
/// [`EditorTheme::from_visuals`] (follows the current egui theme and picks
/// up its background and selection colors).
#[derive(Clone, Debug, PartialEq)]
pub struct EditorTheme {
    /// Text area background.
    pub background: Color32,
    /// Plain text (and [`TokenStyle::Plain`]).
    pub text: Color32,
    /// Keywords.
    pub keyword: Color32,
    /// Unquoted identifiers.
    pub identifier: Color32,
    /// Quoted identifiers.
    pub quoted_identifier: Color32,
    /// String literals.
    pub string: Color32,
    /// Numbers.
    pub number: Color32,
    /// Comments.
    pub comment: Color32,
    /// Operators.
    pub operator: Color32,
    /// Punctuation.
    pub punctuation: Color32,
    /// Bind parameters.
    pub parameter: Color32,
    /// Variables.
    pub variable: Color32,
    /// Function names.
    pub function: Color32,
    /// Data types.
    pub data_type: Color32,
    /// Client side commands.
    pub client_command: Color32,
    /// Render comments in italics.
    pub italic_comments: bool,
    /// Gutter background.
    pub gutter_background: Color32,
    /// Line numbers.
    pub gutter_text: Color32,
    /// Line number of the cursor line.
    pub gutter_current_text: Color32,
    /// Thin separator between gutter and text.
    pub gutter_separator: Color32,
    /// Background of the cursor line.
    pub current_line: Color32,
    /// Background of the statement range set by
    /// [`crate::EditorState::set_statement_highlight`].
    pub statement_background: Color32,
    /// Gutter bar next to the highlighted statement.
    pub statement_marker: Color32,
    /// Selection background.
    pub selection: Color32,
    /// Selection background while the editor does not have keyboard focus.
    pub selection_inactive: Color32,
    /// Text cursor.
    pub cursor: Color32,
    /// Error squiggles and markers.
    pub error: Color32,
    /// Warning squiggles and markers.
    pub warning: Color32,
    /// Info squiggles and markers.
    pub info: Color32,
    /// Spell-check dotted underline.
    pub spelling: Color32,
    /// Background of every find match.
    pub find_match: Color32,
    /// Other occurrences of the word under the cursor.
    pub occurrence: Color32,
    /// Background of the current find match.
    pub find_current: Color32,
    /// Background of matching brackets.
    pub bracket_match: Color32,
    /// Outline of matching brackets.
    pub bracket_outline: Color32,
    /// Hyperlink underline / text (command + hover).
    pub link: Color32,
    /// Underline of IME pre-edit (composition) text.
    pub ime_underline: Color32,
}

impl EditorTheme {
    /// Light palette.
    pub fn light() -> Self {
        Self {
            background: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text: Color32::from_rgb(0x1F, 0x1F, 0x1F),
            keyword: Color32::from_rgb(0x00, 0x33, 0xB3),
            identifier: Color32::from_rgb(0x1F, 0x1F, 0x1F),
            quoted_identifier: Color32::from_rgb(0x00, 0x5C, 0x7A),
            string: Color32::from_rgb(0x06, 0x7D, 0x17),
            number: Color32::from_rgb(0x17, 0x50, 0xEB),
            comment: Color32::from_rgb(0x8C, 0x8C, 0x8C),
            operator: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            punctuation: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            parameter: Color32::from_rgb(0x87, 0x10, 0x94),
            variable: Color32::from_rgb(0x87, 0x10, 0x94),
            function: Color32::from_rgb(0x00, 0x62, 0x7A),
            data_type: Color32::from_rgb(0x00, 0x7A, 0x6C),
            client_command: Color32::from_rgb(0x9E, 0x5A, 0x00),
            italic_comments: true,
            gutter_background: Color32::from_rgb(0xF7, 0xF7, 0xF7),
            gutter_text: Color32::from_rgb(0x86, 0x86, 0x86),
            gutter_current_text: Color32::from_rgb(0x33, 0x33, 0x33),
            gutter_separator: Color32::from_rgb(0xE6, 0xE6, 0xE6),
            current_line: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 10),
            statement_background: Color32::from_rgba_unmultiplied(0x2F, 0x6F, 0xDB, 18),
            statement_marker: Color32::from_rgb(0x2F, 0x6F, 0xDB),
            selection: Color32::from_rgb(0xB4, 0xD5, 0xFE),
            selection_inactive: Color32::from_rgb(0xDD, 0xE3, 0xEB),
            cursor: Color32::from_rgb(0x00, 0x00, 0x00),
            error: Color32::from_rgb(0xD3, 0x1F, 0x1F),
            warning: Color32::from_rgb(0xC2, 0x7C, 0x00),
            info: Color32::from_rgb(0x1A, 0x73, 0xE8),
            spelling: Color32::from_rgb(0x1A, 0x73, 0xE8),
            find_match: Color32::from_rgba_unmultiplied(0xF5, 0xA6, 0x23, 90),
            occurrence: Color32::from_rgba_unmultiplied(0x2D, 0x7F, 0xF9, 40),
            find_current: Color32::from_rgba_unmultiplied(0xF5, 0x7C, 0x00, 170),
            bracket_match: Color32::from_rgba_unmultiplied(0x00, 0x96, 0x46, 40),
            bracket_outline: Color32::from_rgb(0x8C, 0xB4, 0x96),
            link: Color32::from_rgb(0x00, 0x5F, 0xCC),
            ime_underline: Color32::from_rgb(0x1F, 0x1F, 0x1F),
        }
    }

    /// Dark palette.
    pub fn dark() -> Self {
        Self {
            background: Color32::from_rgb(0x1E, 0x1F, 0x22),
            text: Color32::from_rgb(0xD4, 0xD4, 0xD4),
            keyword: Color32::from_rgb(0x6C, 0xA8, 0xF0),
            identifier: Color32::from_rgb(0xD4, 0xD4, 0xD4),
            quoted_identifier: Color32::from_rgb(0x9C, 0xDC, 0xFE),
            string: Color32::from_rgb(0x8F, 0xC9, 0x7A),
            number: Color32::from_rgb(0x2A, 0xAC, 0xB8),
            comment: Color32::from_rgb(0x7A, 0x7E, 0x85),
            operator: Color32::from_rgb(0xC8, 0xC8, 0xC8),
            punctuation: Color32::from_rgb(0xB0, 0xB0, 0xB0),
            parameter: Color32::from_rgb(0xC5, 0x86, 0xC0),
            variable: Color32::from_rgb(0xC5, 0x86, 0xC0),
            function: Color32::from_rgb(0x56, 0xC1, 0xD6),
            data_type: Color32::from_rgb(0x4E, 0xC9, 0xB0),
            client_command: Color32::from_rgb(0xE5, 0xB5, 0x67),
            italic_comments: true,
            gutter_background: Color32::from_rgb(0x1E, 0x1F, 0x22),
            gutter_text: Color32::from_rgb(0x6F, 0x73, 0x7A),
            gutter_current_text: Color32::from_rgb(0xC8, 0xC8, 0xC8),
            gutter_separator: Color32::from_rgb(0x2B, 0x2D, 0x30),
            current_line: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 10),
            statement_background: Color32::from_rgba_unmultiplied(0x6C, 0xA8, 0xF0, 20),
            statement_marker: Color32::from_rgb(0x6C, 0xA8, 0xF0),
            selection: Color32::from_rgb(0x26, 0x4F, 0x78),
            selection_inactive: Color32::from_rgb(0x3A, 0x3D, 0x41),
            cursor: Color32::from_rgb(0xE6, 0xE6, 0xE6),
            error: Color32::from_rgb(0xF4, 0x47, 0x47),
            warning: Color32::from_rgb(0xE2, 0xB3, 0x3C),
            info: Color32::from_rgb(0x4A, 0x9E, 0xFF),
            spelling: Color32::from_rgb(0x4A, 0x9E, 0xFF),
            find_match: Color32::from_rgba_unmultiplied(0xE0, 0x8A, 0x1E, 70),
            occurrence: Color32::from_rgba_unmultiplied(0x6C, 0xA8, 0xFF, 45),
            find_current: Color32::from_rgba_unmultiplied(0xF0, 0x9A, 0x2E, 150),
            bracket_match: Color32::from_rgba_unmultiplied(0x3F, 0xBF, 0x7F, 40),
            bracket_outline: Color32::from_rgb(0x6A, 0x9A, 0x7A),
            link: Color32::from_rgb(0x58, 0x9D, 0xF6),
            ime_underline: Color32::from_rgb(0xD4, 0xD4, 0xD4),
        }
    }

    /// Palette matching `visuals` (dark or light), using its text-edit
    /// background and selection colors so the editor blends into the app.
    pub fn from_visuals(visuals: &Visuals) -> Self {
        let mut theme = if visuals.dark_mode {
            Self::dark()
        } else {
            Self::light()
        };
        theme.background = visuals.text_edit_bg_color();
        theme.gutter_background = visuals.text_edit_bg_color();
        theme.selection = visuals.selection.bg_fill;
        theme
    }

    /// Text color for a token style.
    pub fn token_color(&self, style: TokenStyle) -> Color32 {
        match style {
            TokenStyle::Keyword => self.keyword,
            TokenStyle::Identifier => self.identifier,
            TokenStyle::QuotedIdentifier => self.quoted_identifier,
            TokenStyle::String => self.string,
            TokenStyle::Number => self.number,
            TokenStyle::Comment => self.comment,
            TokenStyle::Operator => self.operator,
            TokenStyle::Punctuation => self.punctuation,
            TokenStyle::Parameter => self.parameter,
            TokenStyle::Variable => self.variable,
            TokenStyle::Function => self.function,
            TokenStyle::DataType => self.data_type,
            TokenStyle::ClientCommand => self.client_command,
            TokenStyle::Plain => self.text,
        }
    }

    /// Hash of everything that affects text layout (token colors and
    /// italics); used to invalidate cached line galleys.
    pub(crate) fn layout_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for style in TokenStyle::ALL {
            self.token_color(style).hash(&mut h);
        }
        self.italic_comments.hash(&mut h);
        h.finish()
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_differ_and_follow_visuals() {
        let light = EditorTheme::light();
        let dark = EditorTheme::dark();
        assert_ne!(light.layout_key(), dark.layout_key());
        let v = Visuals::dark();
        let t = EditorTheme::from_visuals(&v);
        assert_eq!(t.background, v.text_edit_bg_color());
        assert_eq!(t.keyword, dark.keyword);
        let t = EditorTheme::from_visuals(&Visuals::light());
        assert_eq!(t.keyword, light.keyword);
        for style in TokenStyle::ALL {
            assert_ne!(light.token_color(style), Color32::TRANSPARENT);
        }
    }

    #[test]
    fn text_contrasts_with_background() {
        fn luminance(c: Color32) -> f32 {
            let f = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        fn contrast(a: Color32, b: Color32) -> f32 {
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        }
        for theme in [EditorTheme::light(), EditorTheme::dark()] {
            for style in TokenStyle::ALL {
                let c = contrast(theme.token_color(style), theme.background);
                assert!(c >= 3.0, "{style:?} contrast {c}");
            }
            assert!(contrast(theme.gutter_text, theme.gutter_background) >= 3.0);
        }
    }
}
