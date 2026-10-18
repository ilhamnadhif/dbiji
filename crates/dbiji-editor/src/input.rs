//! Keyboard, clipboard and IME handling of the focused editor, plus the
//! editor-local commands shared with the context menu.

use egui::epaint::text::CharIndex;
use egui::text::CCursor;
use egui::{Context, Event, ImeEvent, Key, Modifiers, ViewportCommand};

use crate::completion;
use crate::find_bar;
use crate::language::{EditorLanguage, TokenStyle};
use crate::ops::{self, EditPlan, TypedAction};
use crate::options::{EditorOptions, any_matches};
use crate::state::{EditorState, ScrollRequest};
use crate::text::{
    Selection, next_word_boundary, normalize_newlines, prev_word_boundary, word_range_at,
};
use crate::undo::EditKind;
use crate::widget::{ContextAction, EditorEvent};

/// Result of processing this frame's keyboard events.
#[derive(Default)]
pub(crate) struct KeyboardOutput {
    /// Characters typed (for completion triggers).
    pub typed: Vec<char>,
}

fn is_mac() -> bool {
    cfg!(target_os = "macos")
}

/// Is `modifiers` the platform "word" modifier (Alt on macOS, Ctrl elsewhere)?
fn word_mod(m: &Modifiers) -> bool {
    if is_mac() {
        m.alt && !m.command && !m.ctrl
    } else {
        m.ctrl && !m.alt
    }
}

/// Process keyboard / clipboard / IME events in order. Handled events are
/// removed from egui's input; everything else is left for the application.
pub(crate) fn handle_keyboard(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    events_out: &mut Vec<EditorEvent>,
) -> KeyboardOutput {
    let mut out = KeyboardOutput::default();
    let events = ctx.input_mut(|i| std::mem::take(&mut i.events));
    let mut unhandled = Vec::with_capacity(events.len());
    for event in events {
        if matches!(
            event,
            Event::Text(_) | Event::Paste(_) | Event::Key { pressed: true, .. }
        ) {
            st.ime_cleared_at = None;
        }
        let handled = match &event {
            Event::Text(t) => {
                if t == "\n" || t == "\r" || t == "\t" {
                    true
                } else {
                    type_text(st, lang, options, t, &mut out);
                    true
                }
            }
            Event::Paste(t) => {
                if !options.read_only {
                    let t = normalize_newlines(t).into_owned();
                    completion_close(st);
                    st.apply_plan(ops::replace_selection(st.sel, &t), EditKind::Other);
                    st.undo.close_group();
                    st.pending_scroll = Some(ScrollRequest::Cursor);
                }
                true
            }
            Event::Copy => {
                copy(st, ctx, options, false);
                true
            }
            Event::Cut => {
                copy(st, ctx, options, !options.read_only);
                true
            }
            Event::Ime(ime) => {
                handle_ime(st, lang, options, ime, &mut out);
                true
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => handle_key(st, ctx, lang, options, *key, *modifiers, events_out),
            _ => false,
        };
        if !handled {
            unhandled.push(event);
        }
    }
    ctx.input_mut(|i| {
        let later = std::mem::take(&mut i.events);
        unhandled.extend(later);
        i.events = unhandled;
    });
    out
}

fn completion_close(st: &mut EditorState) {
    st.completion.close();
}

/// Insert typed text (with auto-close handling for single characters).
fn type_text(
    st: &mut EditorState,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    t: &str,
    out: &mut KeyboardOutput,
) {
    if options.read_only || t.is_empty() {
        return;
    }
    let mut chars = t.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    if let (Some(ch), true) = (single, options.auto_close_brackets) {
        let in_literal = st
            .layout
            .style_inside(&st.index, st.sel.cursor)
            .is_some_and(TokenStyle::is_literal);
        let pairs = st.auto_pairs.clone();
        match ops::typed_char(
            &st.text,
            st.sel,
            ch,
            in_literal,
            |c| lang.is_word_char(c),
            |o| pairs.iter().any(|p| p.close == o),
        ) {
            TypedAction::Skip(c) => {
                st.set_selection(Selection::caret(c));
                st.undo.close_group();
                st.pending_scroll = Some(ScrollRequest::Cursor);
                out.typed.push(ch);
                return;
            }
            TypedAction::Plan(plan, pair) => {
                st.apply_plan(plan, EditKind::Typing);
                if let Some(p) = pair {
                    st.auto_pairs.push(p);
                }
                st.pending_scroll = Some(ScrollRequest::Cursor);
                out.typed.push(ch);
                return;
            }
            TypedAction::Insert => {}
        }
    }
    st.apply_plan(ops::replace_selection(st.sel, t), EditKind::Typing);
    st.pending_scroll = Some(ScrollRequest::Cursor);
    out.typed.extend(t.chars());
}

fn copy(st: &mut EditorState, ctx: &Context, options: &EditorOptions, cut: bool) {
    let range = if st.sel.is_empty() {
        if !options.copy_line_without_selection {
            return;
        }
        let line = st.index.line_of_offset(st.sel.cursor);
        st.index.line_range_with_newline(line)
    } else {
        st.sel.range()
    };
    let mut s = st.text[range.clone()].to_owned();
    if st.sel.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    ctx.copy_text(s);
    if cut && !range.is_empty() {
        let caret = range.start;
        st.apply_plan(
            EditPlan::single(range, "", Selection::caret(caret)),
            EditKind::Other,
        );
        st.undo.close_group();
        st.pending_scroll = Some(ScrollRequest::Cursor);
    }
}

fn handle_ime(
    st: &mut EditorState,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    ime: &ImeEvent,
    out: &mut KeyboardOutput,
) {
    if options.read_only {
        return;
    }
    match ime {
        ImeEvent::Preedit {
            text,
            active_range_chars,
        } => {
            if (text.is_empty() && st.preedit.is_none()) || text == "\n" || text == "\r" {
                return;
            }
            completion_close(st);
            let before = st.sel;
            let range = st.preedit.clone().unwrap_or_else(|| st.sel.range());
            let rec = st.raw_edit(range.clone(), text);
            if text.is_empty() {
                st.preedit = None;
                st.set_selection(Selection::caret(range.start));
                // macOS clears the pre-edit right before committing.
                st.ime_cleared_at = Some(range.start);
            } else {
                st.preedit = Some(range.start..range.start + text.len());
                let caret = match active_range_chars {
                    Some(r) => {
                        range.start
                            + text
                                .char_indices()
                                .nth(r.end)
                                .map_or(text.len(), |(i, _)| i)
                    }
                    None => range.start + text.len(),
                };
                st.set_selection(Selection::caret(caret));
            }
            st.undo
                .record(EditKind::Ime, vec![rec], before, st.sel, st.now);
            st.pending_scroll = Some(ScrollRequest::Cursor);
        }
        ImeEvent::Commit(text) => {
            if text == "\n" || text == "\r" {
                return;
            }
            let composed = st.preedit.take().or_else(|| {
                st.ime_cleared_at
                    .filter(|&p| p == st.sel.cursor && st.sel.is_empty())
                    .map(|p| p..p)
            });
            st.ime_cleared_at = None;
            if let Some(p) = composed {
                let before = st.sel;
                let rec = st.raw_edit(p.clone(), text);
                st.set_selection(Selection::caret(p.start + text.len()));
                st.undo
                    .record(EditKind::Ime, vec![rec], before, st.sel, st.now);
                st.undo.close_group();
                out.typed.extend(text.chars());
                st.pending_scroll = Some(ScrollRequest::Cursor);
            } else if !text.is_empty() {
                type_text(st, lang, options, text, out);
            }
        }
        ImeEvent::DeleteSurrounding {
            before_chars,
            after_chars,
        } => {
            let r = st.sel.range();
            let start_line = st.index.line_start(st.index.line_of_offset(r.start));
            let mut start = r.start;
            for _ in 0..*before_chars {
                if start <= start_line {
                    break;
                }
                start = crate::text::prev_char_boundary(&st.text, start);
            }
            let mut end = r.end;
            for _ in 0..*after_chars {
                end = crate::text::next_char_boundary(&st.text, end);
            }
            let before = st.sel;
            let mut recs = Vec::new();
            if end > r.end {
                recs.push(st.raw_edit(r.end..end, ""));
            }
            if start < r.start {
                recs.push(st.raw_edit(start..r.start, ""));
            }
            st.set_selection(Selection::new(
                before.anchor - (r.start - start),
                before.cursor - (r.start - start),
            ));
            st.undo.record(EditKind::Ime, recs, before, st.sel, st.now);
        }
        #[allow(deprecated)]
        ImeEvent::Enabled | ImeEvent::Disabled => {}
    }
}

/// Handle one key press; returns `true` if it was consumed.
fn handle_key(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    key: Key,
    m: Modifiers,
    events: &mut Vec<EditorEvent>,
) -> bool {
    let keys = &options.keys;
    let ro = options.read_only;
    let plain = m.matches_exact(Modifiers::NONE);

    // Completion popup navigation.
    if st.completion.is_open() && plain {
        match key {
            Key::ArrowUp => {
                st.completion.move_selection(-1);
                return true;
            }
            Key::ArrowDown => {
                st.completion.move_selection(1);
                return true;
            }
            Key::PageUp => {
                st.completion
                    .move_selection(-(completion::VISIBLE_ROWS as isize - 1));
                return true;
            }
            Key::PageDown => {
                st.completion
                    .move_selection(completion::VISIBLE_ROWS as isize - 1);
                return true;
            }
            Key::Enter | Key::Tab => {
                if let Some(entry) = completion::accept(st) {
                    events.push(EditorEvent::CompletionAccepted(entry));
                }
                return true;
            }
            Key::Escape => {
                st.completion.close();
                return true;
            }
            _ => {}
        }
    }
    if key == Key::Escape && plain {
        if st.goto.open {
            st.goto.open = false;
            return true;
        }
        if st.find.open {
            st.close_find();
            return true;
        }
        if !st.sel.is_empty() {
            let c = st.sel.cursor;
            st.set_selection(Selection::caret(c));
            return true;
        }
        return false;
    }

    // Configurable commands.
    if any_matches(&keys.undo, key, m) {
        if !ro {
            completion_close(st);
            st.undo_internal();
        }
        return true;
    }
    if any_matches(&keys.redo, key, m) {
        if !ro {
            completion_close(st);
            st.redo_internal();
        }
        return true;
    }
    if any_matches(&keys.select_all, key, m) {
        let len = st.text.len();
        st.set_selection(Selection::new(0, len));
        st.undo.close_group();
        return true;
    }
    if any_matches(&keys.find, key, m) {
        st.open_find(false);
        return true;
    }
    if any_matches(&keys.replace, key, m) {
        st.open_find(true);
        return true;
    }
    if any_matches(&keys.find_next, key, m) {
        find_bar::find_step(st, lang, true);
        return true;
    }
    if any_matches(&keys.find_previous, key, m) {
        find_bar::find_step(st, lang, false);
        return true;
    }
    if any_matches(&keys.goto_line, key, m) {
        st.open_goto_line();
        return true;
    }
    if any_matches(&keys.save, key, m) {
        events.push(EditorEvent::SaveRequested);
        return true;
    }
    if any_matches(&keys.trigger_completion, key, m) {
        if !ro {
            st.completion.request_manual();
        }
        return true;
    }
    if any_matches(&keys.select_word, key, m) {
        if let Some(r) = word_range_at(&st.text, st.sel.cursor, |c| lang.is_word_char(c)) {
            st.set_selection(Selection::new(r.start, r.end));
        }
        return true;
    }
    if any_matches(&keys.select_line, key, m) {
        let (first, last) = ops::selected_lines(&st.index, st.sel);
        let start = st.index.line_start(first);
        let end = st.index.line_range_with_newline(last).end;
        st.set_selection(Selection::new(start, end));
        return true;
    }
    let edit_commands: [(&[egui::KeyboardShortcut], ContextAction); 4] = [
        (&keys.toggle_comment, ContextAction::ToggleComment),
        (
            &keys.toggle_block_comment,
            ContextAction::ToggleBlockComment,
        ),
        (&keys.upper_case, ContextAction::UpperCase),
        (&keys.lower_case, ContextAction::LowerCase),
    ];
    for (list, action) in edit_commands {
        if any_matches(list, key, m) {
            apply_context_action(st, ctx, lang, options, &action);
            return true;
        }
    }
    if any_matches(&keys.duplicate, key, m) {
        if !ro {
            let plan = ops::duplicate(&st.text, &st.index, st.sel);
            apply_command(st, plan);
        }
        return true;
    }
    if any_matches(&keys.delete_line, key, m) {
        if !ro {
            let plan = ops::delete_lines(&st.text, &st.index, st.sel);
            apply_command(st, plan);
        }
        return true;
    }
    if any_matches(&keys.move_line_up, key, m) || any_matches(&keys.move_line_down, key, m) {
        if !ro
            && let Some(plan) = ops::move_lines(
                &st.text,
                &st.index,
                st.sel,
                any_matches(&keys.move_line_up, key, m),
            )
        {
            apply_command(st, plan);
        }
        return true;
    }

    // Editing keys.
    match key {
        Key::Enter if plain || m.matches_exact(Modifiers::SHIFT) => {
            if !ro {
                completion_close(st);
                let plan = ops::newline(&st.text, &st.index, st.sel, &options.indent_unit());
                st.apply_plan(plan, EditKind::Typing);
                st.pending_scroll = Some(ScrollRequest::Cursor);
            }
            return true;
        }
        Key::Tab if plain => {
            if !ro {
                // A selection touching more than one line (including a whole
                // line selected up to the next line start) indents lines.
                let multi = !st.sel.is_empty()
                    && st.index.line_of_offset(st.sel.start())
                        != st.index.line_of_offset(st.sel.end());
                let plan = if multi {
                    ops::indent_lines(&st.text, &st.index, st.sel, &options.indent_unit())
                } else {
                    Some(ops::insert_indent(
                        &st.text,
                        &st.index,
                        st.sel,
                        options.tab_size,
                        options.insert_spaces,
                    ))
                };
                if let Some(plan) = plan {
                    apply_command(st, plan);
                }
            }
            return true;
        }
        Key::Tab if m.matches_exact(Modifiers::SHIFT) => {
            if !ro
                && let Some(plan) =
                    ops::outdent_lines(&st.text, &st.index, st.sel, options.tab_size)
            {
                apply_command(st, plan);
            }
            return true;
        }
        Key::Backspace => {
            if ro {
                return true;
            }
            let plan = if is_mac() && m.mac_cmd || (!is_mac() && m.ctrl && m.shift) {
                ops::delete_to_line_start(&st.text, &st.index, st.sel)
            } else if word_mod(&m) {
                ops::delete_word_back(&st.text, st.sel, |c| lang.is_word_char(c))
            } else {
                let c = st.sel.cursor;
                let pair = st.sel.is_empty()
                    && st
                        .auto_pairs
                        .iter()
                        .any(|p| p.close == c && p.open + p.open_char.len_utf8() == c);
                ops::backspace(
                    &st.text,
                    &st.index,
                    st.sel,
                    options.tab_size,
                    options.insert_spaces,
                    pair,
                )
            };
            if let Some(plan) = plan {
                st.apply_plan(plan, EditKind::Backspace);
                st.pending_scroll = Some(ScrollRequest::Cursor);
            }
            return true;
        }
        Key::Delete => {
            if ro {
                return true;
            }
            let plan = if is_mac() && m.mac_cmd {
                ops::delete_to_line_end(&st.text, &st.index, st.sel)
            } else if word_mod(&m) {
                ops::delete_word_forward(&st.text, st.sel, |c| lang.is_word_char(c))
            } else {
                ops::delete_forward(&st.text, st.sel)
            };
            if let Some(plan) = plan {
                st.apply_plan(plan, EditKind::Delete);
                st.pending_scroll = Some(ScrollRequest::Cursor);
            }
            return true;
        }
        Key::H | Key::K | Key::D if is_mac() && m.ctrl && !m.alt && !m.shift && !m.mac_cmd => {
            if ro {
                return true;
            }
            let plan = match key {
                Key::H => {
                    ops::backspace(&st.text, &st.index, st.sel, options.tab_size, false, false)
                }
                Key::K => ops::delete_to_line_end(&st.text, &st.index, st.sel),
                _ => ops::delete_forward(&st.text, st.sel),
            };
            if let Some(plan) = plan {
                st.apply_plan(
                    plan,
                    if key == Key::H {
                        EditKind::Backspace
                    } else {
                        EditKind::Delete
                    },
                );
                st.pending_scroll = Some(ScrollRequest::Cursor);
            }
            return true;
        }
        _ => {}
    }
    navigate(st, ctx, lang, key, m)
}

fn apply_command(st: &mut EditorState, plan: EditPlan) {
    st.completion.close();
    st.apply_plan(plan, EditKind::Other);
    st.undo.close_group();
    st.pending_scroll = Some(ScrollRequest::Cursor);
}

/// Cursor movement keys. Returns `true` if `key` was a navigation key.
fn navigate(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    key: Key,
    m: Modifiers,
) -> bool {
    let shift = m.shift;
    let mac = is_mac();
    let is_word = |c: char| lang.is_word_char(c);
    let c = st.sel.cursor;
    let target: Option<(usize, bool, bool)> = match key {
        Key::ArrowLeft | Key::ArrowRight => {
            let left = key == Key::ArrowLeft;
            if (mac && m.mac_cmd) && !m.alt {
                Some(row_edge(st, ctx, !left, true))
            } else if word_mod(&m) {
                let to = if left {
                    prev_word_boundary(&st.text, c, is_word)
                } else {
                    next_word_boundary(&st.text, c, is_word)
                };
                Some((to, true, false))
            } else if m.alt || m.ctrl || m.mac_cmd {
                None
            } else if !shift && !st.sel.is_empty() {
                Some((
                    if left { st.sel.start() } else { st.sel.end() },
                    true,
                    false,
                ))
            } else {
                let to = if left {
                    crate::text::prev_char_boundary(&st.text, c)
                } else {
                    crate::text::next_char_boundary(&st.text, c)
                };
                Some((to, true, false))
            }
        }
        Key::ArrowUp | Key::ArrowDown => {
            let up = key == Key::ArrowUp;
            if m.command && !m.alt {
                Some((if up { 0 } else { st.text.len() }, true, false))
            } else if m.alt || m.ctrl {
                None
            } else {
                let (to, aff) = vertical(st, ctx, if up { -1 } else { 1 });
                Some((to, aff, true))
            }
        }
        Key::PageUp | Key::PageDown => {
            if m.alt || m.command {
                None
            } else {
                let rows =
                    ((st.viewport_size.y / st.layout.row_height()).floor() as isize - 1).max(1);
                let delta = if key == Key::PageUp { -rows } else { rows };
                let (to, aff) = vertical(st, ctx, delta);
                st.scroll_offset.y =
                    (st.scroll_offset.y + delta as f32 * st.layout.row_height()).max(0.0);
                st.restore_scroll = Some(st.scroll_offset);
                Some((to, aff, true))
            }
        }
        Key::Home | Key::End => {
            let home = key == Key::Home;
            if m.ctrl && !mac || (mac && m.mac_cmd) {
                Some((if home { 0 } else { st.text.len() }, true, false))
            } else if m.alt {
                None
            } else if home {
                Some(row_edge(st, ctx, false, true))
            } else {
                Some(row_edge(st, ctx, true, true))
            }
        }
        Key::A | Key::E | Key::B | Key::F | Key::N | Key::P
            if mac && m.ctrl && !m.mac_cmd && !m.alt =>
        {
            match key {
                Key::A => Some(row_edge(st, ctx, false, false)),
                Key::E => Some(row_edge(st, ctx, true, false)),
                Key::B => Some((crate::text::prev_char_boundary(&st.text, c), true, false)),
                Key::F => Some((crate::text::next_char_boundary(&st.text, c), true, false)),
                _ => {
                    let (to, aff) = vertical(st, ctx, if key == Key::P { -1 } else { 1 });
                    Some((to, aff, true))
                }
            }
        }
        _ => None,
    };
    let Some((to, affinity, keep_h)) = target else {
        return false;
    };
    let sel = if shift {
        Selection::new(st.sel.anchor, to)
    } else {
        Selection::caret(to)
    };
    st.set_selection(sel);
    st.prefer_next_row = affinity;
    if !keep_h {
        st.h_pos = None;
    }
    st.undo.close_group();
    st.pending_scroll = Some(ScrollRequest::Cursor);
    true
}

/// Start (`end == false`) or end of the visual row of the cursor. With
/// `smart`, Home toggles between the first non-blank char and the row start.
fn row_edge(st: &mut EditorState, ctx: &Context, end: bool, smart: bool) -> (usize, bool, bool) {
    let c = st.sel.cursor;
    let line = st.index.line_of_offset(c);
    let col = st.index.char_col(&st.text, c);
    let g = st.layout.galley(ctx, &st.text, &st.index, line);
    let lc = g.layout_from_cursor(CCursor {
        index: CharIndex(col),
        prefer_next_row: st.prefer_next_row,
    });
    let row_start: usize = g.rows[..lc.row.min(g.rows.len())]
        .iter()
        .map(|r| r.glyphs.len())
        .sum();
    let row_len = g.rows.get(lc.row).map_or(0, |r| r.glyphs.len());
    let last_row = lc.row + 1 >= g.rows.len();
    if end {
        // The last row ends at the real line end (also for lines whose
        // layout is truncated); at a soft-wrap boundary keep the cursor on
        // this row.
        let target = if last_row {
            st.index.line_end(line)
        } else {
            st.index
                .offset_of_char_col(&st.text, line, row_start + row_len)
        };
        (target, last_row, false)
    } else {
        let target = st.index.offset_of_char_col(&st.text, line, row_start);
        if smart && lc.row == 0 {
            (ops::smart_home(&st.text, &st.index, c), true, false)
        } else {
            (target, true, false)
        }
    }
}

/// Move the cursor `rows` visual rows (negative = up), keeping the
/// preferred x position. Returns the offset and its row affinity.
fn vertical(st: &mut EditorState, ctx: &Context, rows: isize) -> (usize, bool) {
    let c = st.sel.cursor;
    let line = st.index.line_of_offset(c);
    let col = st.index.char_col(&st.text, c);
    let g = st.layout.galley(ctx, &st.text, &st.index, line);
    let cur = CCursor {
        index: CharIndex(col),
        prefer_next_row: st.prefer_next_row,
    };
    let lc = g.layout_from_cursor(cur);
    let x = st.h_pos.unwrap_or_else(|| g.pos_from_cursor(cur).min.x);
    st.h_pos = Some(x);
    let global = st.layout.first_row(line) as isize + lc.row as isize + rows;
    let total = st.layout.total_rows() as isize;
    if global < 0 {
        return (0, true);
    }
    if global >= total {
        return (st.text.len(), true);
    }
    let target_line = st.layout.line_of_row(global as u32);
    let row_in_line = (global - st.layout.first_row(target_line) as isize).max(0) as usize;
    let tg = st.layout.galley(ctx, &st.text, &st.index, target_line);
    let row_idx = row_in_line.min(tg.rows.len().saturating_sub(1));
    let before: usize = tg.rows[..row_idx].iter().map(|r| r.glyphs.len()).sum();
    let (in_row, row_len) = match tg.rows.get(row_idx) {
        Some(row) => (row.char_at(x - row.pos.x).0, row.glyphs.len()),
        None => (0, 0),
    };
    let last_row = row_idx + 1 >= tg.rows.len();
    let affinity = in_row < row_len || last_row;
    (
        st.index
            .offset_of_char_col(&st.text, target_line, before + in_row),
        affinity,
    )
}

/// Execute an editor-local action; returns `false` for application actions.
pub(crate) fn apply_context_action(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    action: &ContextAction,
) -> bool {
    let ro = options.read_only;
    match action {
        ContextAction::Cut => copy(st, ctx, options, !ro),
        ContextAction::Copy => copy(st, ctx, options, false),
        ContextAction::Paste => {
            if !ro {
                ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
                st.pending_focus = true;
            }
        }
        ContextAction::SelectAll => {
            let len = st.text.len();
            st.set_selection(Selection::new(0, len));
        }
        ContextAction::Undo => {
            if !ro {
                st.undo_internal();
            }
        }
        ContextAction::Redo => {
            if !ro {
                st.redo_internal();
            }
        }
        ContextAction::ToggleComment => {
            if !ro
                && let Some(plan) =
                    ops::toggle_line_comment(&st.text, &st.index, st.sel, lang.line_comment())
            {
                apply_command(st, plan);
            }
        }
        ContextAction::ToggleBlockComment => {
            if !ro
                && let Some((open, close)) = lang.block_comment()
                && let Some(plan) =
                    ops::toggle_block_comment(&st.text, &st.index, st.sel, open, close)
            {
                apply_command(st, plan);
            }
        }
        ContextAction::UpperCase | ContextAction::LowerCase => {
            let upper = matches!(action, ContextAction::UpperCase);
            if !ro
                && let Some(plan) =
                    ops::change_case(&st.text, st.sel, upper, |c| lang.is_word_char(c))
            {
                apply_command(st, plan);
            }
        }
        ContextAction::ReplaceWord { range, with } => {
            if !ro {
                let range = crate::text::clamp_range(&st.text, range.clone());
                let caret = range.start + with.len();
                apply_command(
                    st,
                    EditPlan::single(range, with.clone(), Selection::caret(caret)),
                );
            }
        }
        ContextAction::Find => st.open_find(false),
        ContextAction::Replace => st.open_find(true),
        ContextAction::GotoLine => st.open_goto_line(),
        ContextAction::Execute
        | ContextAction::ExecuteScript
        | ContextAction::ExecuteNewTab
        | ContextAction::Explain
        | ContextAction::Format
        | ContextAction::AddSpellingWord(_) => return false,
    }
    true
}
