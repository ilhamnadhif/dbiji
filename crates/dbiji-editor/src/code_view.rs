//! Read-only code viewer for DDL tabs, query plans and logs.

use std::collections::HashMap;
use std::hash::Hash;

use egui::{Context, Id, Ui};

use crate::language::EditorLanguage;
use crate::options::EditorOptions;
use crate::state::EditorState;
use crate::theme::EditorTheme;
use crate::widget::{EditorResponse, SqlEditor};

/// Viewers not shown for this long (seconds) drop their cached state.
const FORGET_AFTER_SECS: f64 = 600.0;

#[derive(Clone, Default)]
struct CodeViewState {
    source: String,
    editor: Option<EditorState>,
}

/// When each viewer state was last shown (egui never frees temp data on its
/// own, so closed viewers are swept from here).
#[derive(Clone, Default)]
struct CodeViewRegistry {
    last_shown: HashMap<Id, f64>,
}

fn registry_id() -> Id {
    Id::new("dbiji_code_view_registry")
}

fn salted_id(salt: &impl Hash) -> Id {
    Id::new(("dbiji_code_view", crate::state::hash_id(salt)))
}

/// A read-only, selectable, copyable, highlighted view of `text` with line
/// numbers, find (Cmd+F) and go-to-line. State (scroll, selection, layout
/// cache) is kept in egui memory; appending to the text (log views) keeps the
/// scroll position and selection.
///
/// Give every viewer a unique [`CodeView::id_salt`] (e.g. the tab id) and
/// call [`CodeView::forget`] when its tab closes; state of viewers that have
/// not been shown for ten minutes is dropped automatically.
pub struct CodeView<'a> {
    text: &'a str,
    lang: &'a dyn EditorLanguage,
    theme: &'a EditorTheme,
    options: Option<&'a EditorOptions>,
    id: Option<Id>,
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
            id: None,
            line_numbers: true,
            word_wrap: false,
            font_size: None,
        }
    }

    /// Identify this viewer (must be unique among the viewers of the
    /// application, e.g. the tab id). Without a salt the id is derived from
    /// the parent `Ui`.
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        self.id = Some(salted_id(&salt));
        self
    }

    /// Drop the cached state of the viewer created with `id_salt(salt)`.
    pub fn forget(ctx: &Context, salt: impl Hash) {
        let id = salted_id(&salt);
        ctx.data_mut(|d| {
            d.remove::<CodeViewState>(id);
            if let Some(mut reg) = d.remove_temp::<CodeViewRegistry>(registry_id()) {
                reg.last_shown.remove(&id);
                d.insert_temp(registry_id(), reg);
            }
        });
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
        let id = self
            .id
            .unwrap_or_else(|| ui.make_persistent_id("dbiji_code_view"));
        let ctx = ui.ctx().clone();
        let now = ui.input(|i| i.time);
        sweep(&ctx, id, now);
        let mut cv: CodeViewState = ui
            .data_mut(|d| d.remove_temp::<CodeViewState>(id))
            .unwrap_or_default();
        match cv.editor.as_mut() {
            None => {
                cv.editor = Some(EditorState::new(id, self.text.to_owned()));
                cv.source = self.text.to_owned();
            }
            Some(editor) if cv.source != self.text => {
                if !cv.source.is_empty() && self.text.starts_with(cv.source.as_str()) {
                    // Appended text (log views): keep scroll and selection.
                    // A `\r\n` split between two appends: its `\r` already became a line break.
                    let mut raw = &self.text[cv.source.len()..];
                    if cv.source.ends_with('\r') {
                        raw = raw.strip_prefix('\n').unwrap_or(raw);
                    }
                    let tail = crate::text::normalize_newlines(raw).into_owned();
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

/// Record that viewer `id` is shown and drop viewers unseen for too long.
fn sweep(ctx: &Context, id: Id, now: f64) {
    ctx.data_mut(|d| {
        let reg_id = registry_id();
        let mut reg = d
            .remove_temp::<CodeViewRegistry>(reg_id)
            .unwrap_or_default();
        reg.last_shown.insert(id, now);
        let stale: Vec<Id> = reg
            .last_shown
            .iter()
            .filter(|(_, t)| now - **t > FORGET_AFTER_SECS)
            .map(|(id, _)| *id)
            .collect();
        for old in stale {
            reg.last_shown.remove(&old);
            d.remove::<CodeViewState>(old);
        }
        d.insert_temp(reg_id, reg);
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::PlainText;

    #[test]
    fn forget_and_automatic_sweep() {
        let ctx = Context::default();
        let lang = PlainText;
        let theme = EditorTheme::dark();
        let run = |time: f64, salts: &[&str]| {
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                for s in salts {
                    let _ = CodeView::new("select 1", &lang, &theme).id_salt(s).show(ui);
                }
            })
            .drop_without_applying_deltas();
        };
        let has = |s: &str| ctx.data_mut(|d| d.get_temp::<CodeViewState>(salted_id(&s)).is_some());
        run(0.0, &["a", "b"]);
        assert!(has("a") && has("b"));
        CodeView::forget(&ctx, "a");
        assert!(!has("a"));
        run(1_000.0, &["b"]);
        assert!(has("b"));
        run(2_000.0, &["c"]);
        assert!(!has("b"), "viewers unseen for ten minutes are dropped");
        assert!(has("c"));
    }
}
