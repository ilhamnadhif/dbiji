//! Read-only code viewer for DDL tabs, query plans and logs.

use std::hash::Hash;

use egui::{Id, Ui};

use crate::language::EditorLanguage;
use crate::options::EditorOptions;
use crate::state::EditorState;
use crate::theme::EditorTheme;
use crate::widget::{EditorResponse, SqlEditor};

#[derive(Clone, Default)]
struct CodeViewState {
    source: String,
    editor: Option<EditorState>,
}

/// A read-only, selectable, copyable, highlighted view of `text` with line
/// numbers, find (Cmd+F) and go-to-line. State (scroll, selection, layout
/// cache) is kept in egui memory under the widget id; appending to the text
/// (log views) keeps the scroll position and selection.
pub struct CodeView<'a> {
    text: &'a str,
    lang: &'a dyn EditorLanguage,
    theme: &'a EditorTheme,
    options: Option<&'a EditorOptions>,
    id_salt: Option<Id>,
    line_numbers: bool,
    word_wrap: bool,
    font_size: Option<f32>,
}

impl<'a> CodeView<'a> {
    /// Viewer for `text`.
    pub fn new(text: &'a str, lang: &'a dyn EditorLanguage, theme: &'a EditorTheme) -> Self {
        Self {
            text,
            lang,
            theme,
            options: None,
            id_salt: None,
            line_numbers: true,
            word_wrap: false,
            font_size: None,
        }
    }

    /// Distinguish several viewers in the same `Ui`.
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        self.id_salt = Some(Id::new(("dbiji_code_view", crate::state::hash_id(&salt))));
        self
    }

    /// Use these options (forced read-only) instead of
    /// [`EditorOptions::read_only_viewer`].
    pub fn options(mut self, options: &'a EditorOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// Show or hide line numbers (default: shown).
    pub fn line_numbers(mut self, on: bool) -> Self {
        self.line_numbers = on;
        self
    }

    /// Soft-wrap long lines (default: off).
    pub fn word_wrap(mut self, on: bool) -> Self {
        self.word_wrap = on;
        self
    }

    /// Font size in points.
    pub fn font_size(mut self, size: f32) -> Self {
        self.font_size = Some(size);
        self
    }

    /// Show the viewer, filling the available space.
    pub fn show(self, ui: &mut Ui) -> EditorResponse {
        let id = ui.make_persistent_id(self.id_salt.unwrap_or_else(|| Id::new("dbiji_code_view")));
        let mut cv: CodeViewState = ui
            .data_mut(|d| d.remove_temp::<CodeViewState>(id))
            .unwrap_or_default();
        let ctx = ui.ctx().clone();
        match cv.editor.as_mut() {
            None => {
                cv.editor = Some(EditorState::new(id, self.text.to_owned()));
                cv.source = self.text.to_owned();
            }
            Some(editor) if cv.source != self.text => {
                if !cv.source.is_empty() && self.text.starts_with(cv.source.as_str()) {
                    // Appended text (log views): keep scroll and selection.
                    let tail =
                        crate::text::normalize_newlines(&self.text[cv.source.len()..]).into_owned();
                    let end = editor.text().len();
                    editor.raw_edit(end..end, &tail);
                    editor.undo.clear();
                } else {
                    editor.reset_text(&ctx, self.text.to_owned());
                }
                cv.source = self.text.to_owned();
            }
            Some(_) => {}
        }
        let mut options = self
            .options
            .cloned()
            .unwrap_or_else(EditorOptions::read_only_viewer);
        options.read_only = true;
        options.auto_complete = false;
        options.line_numbers = self.line_numbers;
        options.word_wrap = self.word_wrap;
        if let Some(size) = self.font_size {
            options.font_size = size;
        }
        let response = match cv.editor.as_mut() {
            Some(editor) => SqlEditor::new(editor, self.lang, &options, self.theme)
                .app_actions(&[])
                .show(ui),
            None => EditorResponse {
                response: ui.label(""),
                changed: false,
                events: Vec::new(),
            },
        };
        ui.data_mut(|d| d.insert_temp(id, cv));
        response
    }
}

/// Read-only highlighted viewer with line numbers (see [`CodeView`] for options).
pub fn code_view(
    ui: &mut Ui,
    text: &str,
    lang: &dyn EditorLanguage,
    theme: &EditorTheme,
) -> EditorResponse {
    CodeView::new(text, lang, theme).show(ui)
}
