//! Find / replace bar and the go-to-line popup.

use std::ops::Range;

use dbiji_core::{tr, trf};
use egui::text::{CCursor, CCursorRange};
use egui::{Context, Key, Modifiers, RichText, TextEdit, Ui, vec2};

use crate::find::{FindOptions, MAX_MATCHES, Searcher, next_match, prev_match};
use crate::language::EditorLanguage;
use crate::ops::{Edit, EditPlan, map_selection};
use crate::options::{EditorOptions, any_matches};
use crate::state::{EditorState, ScrollRequest};
use crate::text::Selection;
use crate::theme::EditorTheme;
use crate::undo::EditKind;

/// Documents larger than this rescan find matches only after typing pauses.
const RESCAN_DEBOUNCE_BYTES: usize = 256 * 1024;
/// Pause before rescanning.
const RESCAN_DEBOUNCE_SECS: f64 = 0.25;

/// Find bar state (part of [`EditorState`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct FindState {
    pub(crate) open: bool,
    pub(crate) replace_open: bool,
    pub(crate) query: String,
    pub(crate) replacement: String,
    pub(crate) options: FindOptions,
    pub(crate) focus_query: bool,
    pub(crate) dirty: bool,
    pub(crate) matches: Vec<Range<usize>>,
    pub(crate) current: Option<usize>,
    pub(crate) error: Option<String>,
    computed_version: Option<u64>,
    searcher_key: Option<(String, FindOptions)>,
    searcher: Option<Searcher>,
}

impl FindState {
    /// Matches to highlight.
    pub(crate) fn visible_matches(&self) -> &[Range<usize>] {
        if self.open { &self.matches } else { &[] }
    }
}

/// Go-to-line popup state.
#[derive(Clone, Debug, Default)]
pub(crate) struct GotoState {
    pub(crate) open: bool,
    pub(crate) input: String,
    pub(crate) focus: bool,
    pub(crate) invalid: bool,
}

/// Recompute matches if the query, flags or text changed.
pub(crate) fn refresh(st: &mut EditorState, lang: &dyn EditorLanguage) {
    let version = st.version();
    let f = &mut st.find;
    if !f.open {
        return;
    }
    let key = (f.query.clone(), f.options);
    if f.searcher_key.as_ref() != Some(&key) {
        match Searcher::new(&f.query, f.options) {
            Ok(s) => {
                f.searcher = s;
                f.error = None;
            }
            Err(e) => {
                f.searcher = None;
                f.error = Some(e);
            }
        }
        f.searcher_key = Some(key);
        f.dirty = true;
    }
    if !f.dirty && f.computed_version == Some(version) {
        return;
    }
    // While typing in a large document the matches follow the edits (see
    // `EditorState::shift_markers`); rescan once typing pauses.
    if !f.dirty
        && st.text.len() > RESCAN_DEBOUNCE_BYTES
        && st.now - st.last_edit_time < RESCAN_DEBOUNCE_SECS
    {
        st.pending_find_rescan = Some(st.last_edit_time + RESCAN_DEBOUNCE_SECS);
        return;
    }
    st.pending_find_rescan = None;
    let f = &mut st.find;
    f.matches = match &f.searcher {
        Some(s) => s.find_all(&st.text, MAX_MATCHES, |c| lang.is_word_char(c)),
        None => Vec::new(),
    };
    f.computed_version = Some(version);
    f.dirty = false;
    let sel = st.sel.range();
    f.current = f
        .matches
        .iter()
        .position(|m| *m == sel)
        .or_else(|| next_match(&f.matches, sel.start));
}

/// Select the next (or previous) match, wrapping around.
pub(crate) fn find_step(st: &mut EditorState, lang: &dyn EditorLanguage, forward: bool) {
    if !st.find.open && !st.find.query.is_empty() {
        st.find.open = true;
        st.find.dirty = true;
    }
    refresh(st, lang);
    let sel = st.sel.range();
    let idx = if forward {
        let from = if st.find.matches.contains(&sel) {
            sel.end.max(sel.start + 1)
        } else {
            sel.start
        };
        next_match(&st.find.matches, from)
    } else {
        prev_match(&st.find.matches, sel.start)
    };
    if let Some(i) = idx {
        let m = st.find.matches[i].clone();
        st.find.current = Some(i);
        st.set_selection(Selection::new(m.start, m.end));
        st.undo.close_group();
        st.pending_scroll = Some(ScrollRequest::Center);
    }
}

/// Replace the current match (if selected) and move to the next one.
pub(crate) fn replace_one(st: &mut EditorState, lang: &dyn EditorLanguage) {
    refresh(st, lang);
    let sel = st.sel.range();
    let Some(searcher) = st.find.searcher.clone() else {
        return;
    };
    if !sel.is_empty() && searcher.matches_exactly(&st.text, sel.clone(), |c| lang.is_word_char(c))
    {
        let with = searcher.replacement_for(&st.text, sel.clone(), &st.find.replacement);
        let end = sel.start + with.len();
        st.apply_plan(
            EditPlan::single(sel, with, Selection::caret(end)),
            EditKind::Other,
        );
        st.find.dirty = true;
    }
    find_step(st, lang, true);
}

/// Replace every match as a single undo step. Returns the number replaced.
pub(crate) fn replace_all(st: &mut EditorState, lang: &dyn EditorLanguage) -> usize {
    refresh(st, lang);
    let Some(searcher) = st.find.searcher.clone() else {
        return 0;
    };
    let edits: Vec<Edit> = st
        .find
        .matches
        .iter()
        .map(|m| {
            Edit::new(
                m.clone(),
                searcher.replacement_for(&st.text, m.clone(), &st.find.replacement),
            )
        })
        .collect();
    let n = edits.len();
    if n == 0 {
        return 0;
    }
    let selection = map_selection(st.sel, &edits);
    st.apply_plan(EditPlan { edits, selection }, EditKind::Other);
    st.undo.close_group();
    st.find.dirty = true;
    refresh(st, lang);
    n
}

fn select_all_in(ctx: &Context, id: egui::Id, text: &str) {
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    let n = text.chars().count();
    state
        .cursor
        .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(n))));
    TextEdit::store_state(ctx, id, state);
}

/// Show the find bar at the top of the editor.
pub(crate) fn show_find_bar(
    ui: &mut Ui,
    st: &mut EditorState,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    theme: &EditorTheme,
) {
    use egui_phosphor::regular as ph;
    let ctx = ui.ctx().clone();
    let query_id = st.id.with("find_query");
    let replace_id = st.id.with("find_replace");
    // The panel fill contrasts with both the text area and the input fields
    // (which use the text-edit background).
    let frame = egui::Frame::NONE
        .fill(ui.visuals().panel_fill)
        .inner_margin(egui::Margin::symmetric(6, 4));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        let field_w = (ui.available_width() * 0.35).clamp(140.0, 320.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let toggle_icon = if st.find.replace_open { ph::CARET_DOWN } else { ph::CARET_RIGHT };
            if ui.add(egui::Button::new(toggle_icon).frame(false)).on_hover_text(tr("Toggle replace")).clicked() {
                st.find.replace_open = !st.find.replace_open;
            }
            let resp = ui.add(
                TextEdit::singleline(&mut st.find.query)
                    .id(query_id)
                    .hint_text(tr("Find"))
                    .desired_width(field_w)
                    .return_key(None),
            );
            if std::mem::take(&mut st.find.focus_query) {
                resp.request_focus();
                select_all_in(&ctx, query_id, &st.find.query);
            }
            if resp.changed() {
                st.find.dirty = true;
            }
            let field_focused = resp.has_focus();
            let mut opts = st.find.options;
            ui.toggle_value(&mut opts.case_sensitive, "Aa").on_hover_text(tr("Match case"));
            ui.toggle_value(&mut opts.whole_word, RichText::new("ab").underline()).on_hover_text(tr("Whole word"));
            ui.toggle_value(&mut opts.regex, ".*").on_hover_text(tr("Use regular expression"));
            if opts != st.find.options {
                st.find.options = opts;
                st.find.dirty = true;
            }
            refresh(st, lang);
            let status = if let Some(err) = &st.find.error {
                RichText::new(trf("Invalid regular expression: {}", &[&first_line(err)])).color(theme.error)
            } else if st.find.query.is_empty() {
                RichText::new("")
            } else if st.find.matches.is_empty() {
                RichText::new(tr("No results")).color(theme.warning)
            } else {
                let total = st.find.matches.len();
                let total_s = if total >= MAX_MATCHES { format!("{total}+") } else { total.to_string() };
                match st.find.current {
                    Some(i) => RichText::new(trf("{} of {}", &[&(i + 1), &total_s])),
                    None => RichText::new(trf("{} matches", &[&total_s])),
                }
            };
            ui.label(status);
            let (enter, shift) = ui.input(|i| (i.key_pressed(Key::Enter), i.modifiers.shift));
            let prev = ui.add(egui::Button::new(ph::ARROW_UP).frame(false)).on_hover_text(tr("Previous match")).clicked();
            let next = ui.add(egui::Button::new(ph::ARROW_DOWN).frame(false)).on_hover_text(tr("Next match")).clicked();
            if prev || (field_focused && enter && shift) {
                find_step(st, lang, false);
            } else if next || (field_focused && enter) {
                find_step(st, lang, true);
            }
            if field_focused {
                let (key_next, key_prev) = ui.input(|i| {
                    let pressed = |list: &[egui::KeyboardShortcut]| {
                        i.events.iter().any(|e| matches!(e, egui::Event::Key { key, pressed: true, modifiers, .. } if any_matches(list, *key, *modifiers)))
                    };
                    (pressed(&options.keys.find_next), pressed(&options.keys.find_previous))
                });
                if key_next {
                    find_step(st, lang, true);
                } else if key_prev {
                    find_step(st, lang, false);
                }
            }
            if ui.add(egui::Button::new(ph::X).frame(false)).on_hover_text(tr("Close")).clicked() {
                close = true;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
        if st.find.replace_open {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().interact_size.y * 0.6 + 4.0);
                let resp = ui.add_enabled(
                    !options.read_only,
                    TextEdit::singleline(&mut st.find.replacement).id(replace_id).hint_text(tr("Replace")).desired_width(field_w).return_key(None),
                );
                let enter = resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.matches_exact(Modifiers::NONE));
                let enabled = !options.read_only && st.find.searcher.is_some();
                if ui.add_enabled(enabled, egui::Button::new(tr("Replace"))).clicked() || (enter && enabled) {
                    replace_one(st, lang);
                }
                if ui.add_enabled(enabled, egui::Button::new(tr("Replace all"))).clicked() {
                    replace_all(st, lang);
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            });
        }
        if close {
            st.close_find();
        }
    });
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| l.starts_with("error:"))
        .or_else(|| s.lines().last())
        .unwrap_or(s);
    line.trim_start_matches("error:").trim().to_owned()
}

/// Parse `line` or `line:column` (1-based).
pub(crate) fn parse_goto(input: &str) -> Option<(usize, usize)> {
    let mut parts = input.trim().splitn(2, [':', ',']);
    let line = parts.next()?.trim().parse::<usize>().ok()?;
    let col = match parts.next() {
        Some(c) => c.trim().parse::<usize>().ok()?,
        None => 1,
    };
    (line > 0).then_some((line, col.max(1)))
}

/// Show the go-to-line popup centred at the top of `editor_rect`.
pub(crate) fn show_goto(
    ctx: &Context,
    layer_id: egui::LayerId,
    st: &mut EditorState,
    editor_rect: egui::Rect,
    theme: &EditorTheme,
) {
    if !st.goto.open {
        return;
    }
    let field_id = st.id.with("goto_field");
    let popup_id = st.id.with("goto_popup");
    let anchor =
        egui::Rect::from_center_size(editor_rect.center_top() + vec2(0.0, 6.0), vec2(1.0, 1.0));
    let lines = st.line_count();
    let mut close = false;
    let mut target = None;
    egui::Popup::new(
        popup_id,
        ctx.clone(),
        egui::PopupAnchor::ParentRect(anchor),
        layer_id,
    )
    .open(true)
    .align(egui::RectAlign::BOTTOM)
    .show(|ui| {
        ui.horizontal(|ui| {
            ui.label(trf("Go to line (1 - {}):", &[&lines]));
            let mut edit = TextEdit::singleline(&mut st.goto.input)
                .id(field_id)
                .desired_width(90.0)
                .return_key(None);
            if st.goto.invalid {
                edit = edit.text_color(theme.error);
            }
            let resp = ui.add(edit);
            if std::mem::take(&mut st.goto.focus) {
                resp.request_focus();
            }
            if resp.changed() {
                st.goto.invalid = false;
            }
            if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                match parse_goto(&st.goto.input) {
                    Some(t) => target = Some(t),
                    None => st.goto.invalid = true,
                }
            }
            if resp.lost_focus() && target.is_none() {
                close = true;
            }
        });
    });
    if let Some((line, col)) = target {
        st.goto_line_col(ctx, line, col);
        close = true;
    }
    if close {
        st.goto.open = false;
        st.pending_focus = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::PlainText;

    #[test]
    fn goto_parsing() {
        assert_eq!(parse_goto("12"), Some((12, 1)));
        assert_eq!(parse_goto(" 3:7 "), Some((3, 7)));
        assert_eq!(parse_goto("3,0"), Some((3, 1)));
        assert_eq!(parse_goto("0"), None);
        assert_eq!(parse_goto("x"), None);
        assert_eq!(parse_goto("4:"), None);
    }

    #[test]
    fn find_next_prev_wraps_and_replace_all_is_one_step() {
        let ctx = Context::default();
        let mut st = EditorState::new(1, "select a, A, b from a".into());
        st.find.open = true;
        st.find.query = "a".into();
        st.find.options.whole_word = true;
        find_step(&mut st, &PlainText, true);
        assert_eq!(st.selection(), Some(7..8));
        find_step(&mut st, &PlainText, true);
        assert_eq!(st.selection(), Some(10..11));
        find_step(&mut st, &PlainText, true);
        assert_eq!(st.selection(), Some(20..21));
        find_step(&mut st, &PlainText, true);
        assert_eq!(st.selection(), Some(7..8), "wraps");
        find_step(&mut st, &PlainText, false);
        assert_eq!(st.selection(), Some(20..21), "wraps backwards");
        assert_eq!(st.find.current, Some(2));

        st.find.replacement = "xyz".into();
        replace_one(&mut st, &PlainText);
        assert_eq!(st.text(), "select a, A, b from xyz");
        assert_eq!(st.selection(), Some(7..8));
        let n = replace_all(&mut st, &PlainText);
        assert_eq!(n, 2);
        assert_eq!(st.text(), "select xyz, xyz, b from xyz");
        assert!(st.find.matches.is_empty());
        st.undo(&ctx);
        assert_eq!(st.text(), "select a, A, b from xyz");
    }

    #[test]
    fn regex_replace_with_groups_and_errors() {
        let mut st = EditorState::new(1, "id=1 x=22".into());
        st.find.open = true;
        st.find.query = r"(\w+)=(\d+)".into();
        st.find.options.regex = true;
        st.find.replacement = "$2:$1".into();
        assert_eq!(replace_all(&mut st, &PlainText), 2);
        assert_eq!(st.text(), "1:id 22:x");
        st.find.query = "(".into();
        refresh(&mut st, &PlainText);
        assert!(st.find.error.is_some());
        assert!(st.find.matches.is_empty());
        assert_eq!(replace_all(&mut st, &PlainText), 0);
    }
}
