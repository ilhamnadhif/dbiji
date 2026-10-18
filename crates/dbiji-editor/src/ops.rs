//! Pure text editing operations.
//!
//! Every operation takes the document text, its [`LineIndex`] and the
//! current [`Selection`] and returns an [`EditPlan`]: a list of
//! non-overlapping edits in **original-text byte coordinates** plus the
//! selection after the edits. The state applies plans as one undo step.

use std::ops::Range;

use crate::text::{
    LineIndex, Selection, char_at, char_before, floor_char_boundary, leading_whitespace,
    next_char_boundary, next_word_boundary, prev_char_boundary, prev_word_boundary, visual_width,
    word_range_at,
};

/// Replace `range` with `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Byte range in the text the edit applies to.
    pub range: Range<usize>,
    /// Replacement text.
    pub text: String,
}

impl Edit {
    /// Construct an edit.
    pub fn new(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }
}

/// A set of edits (sorted, non-overlapping, original coordinates) and the
/// resulting selection (coordinates of the edited text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditPlan {
    /// The edits.
    pub edits: Vec<Edit>,
    /// Selection after applying the edits.
    pub selection: Selection,
}

impl EditPlan {
    /// A plan with a single edit.
    pub fn single(range: Range<usize>, text: impl Into<String>, selection: Selection) -> Self {
        Self {
            edits: vec![Edit::new(range, text)],
            selection,
        }
    }

    /// Apply the plan to `text`, returning the new string.
    #[cfg(test)]
    pub fn apply_to(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for e in self.edits.iter().rev() {
            out.replace_range(e.range.clone(), &e.text);
        }
        out
    }

    /// Length change caused by the plan.
    #[cfg(test)]
    pub fn len_delta(&self) -> isize {
        self.edits
            .iter()
            .map(|e| e.text.len() as isize - e.range.len() as isize)
            .sum()
    }
}

/// Map `offset` of the original text through `edits` (sorted, non-overlapping).
///
/// An offset strictly inside a replaced range maps to the start of the
/// replacement (or its end with `stick_right`). An offset exactly at an
/// insertion point stays before the inserted text unless `stick_right`.
pub fn map_offset(offset: usize, edits: &[Edit], stick_right: bool) -> usize {
    let mut delta: isize = 0;
    for e in edits {
        let before = e.range.end < offset
            || (e.range.end == offset && (e.range.start < offset || stick_right));
        if before {
            delta += e.text.len() as isize - e.range.len() as isize;
        } else if e.range.start < offset {
            let base = e.range.start as isize + delta;
            return (base
                + if stick_right {
                    e.text.len() as isize
                } else {
                    0
                })
            .max(0) as usize;
        } else {
            break;
        }
    }
    (offset as isize + delta).max(0) as usize
}

/// Map a selection through edits: the lower end sticks left (so text inserted
/// at the start of a selection becomes part of it), the upper end and carets
/// stick right.
pub fn map_selection(sel: Selection, edits: &[Edit]) -> Selection {
    if sel.is_empty() {
        let c = map_offset(sel.cursor, edits, true);
        return Selection::caret(c);
    }
    let map = |o: usize| map_offset(o, edits, o != sel.start());
    Selection::new(map(sel.anchor), map(sel.cursor))
}

/// First and last line touched by the selection. A selection ending at
/// column 0 of a line does not include that line.
pub fn selected_lines(index: &LineIndex, sel: Selection) -> (usize, usize) {
    let first = index.line_of_offset(sel.start());
    let mut last = index.line_of_offset(sel.end());
    if !sel.is_empty() && last > first && index.line_start(last) == sel.end() {
        last -= 1;
    }
    (first, last)
}

/// Replace the selection with `s` (typing, paste, IME commit).
pub fn replace_selection(sel: Selection, s: &str) -> EditPlan {
    let range = sel.range();
    let end = range.start + s.len();
    EditPlan::single(range, s, Selection::caret(end))
}

/// Toggle a line comment (`prefix`, e.g. `--`) on the selected lines.
///
/// If every non-blank line starts with the prefix the prefix (and one
/// following space) is removed; otherwise `prefix + " "` is inserted at the
/// smallest indentation of the non-blank lines.
pub fn toggle_line_comment(
    text: &str,
    index: &LineIndex,
    sel: Selection,
    prefix: &str,
) -> Option<EditPlan> {
    if prefix.is_empty() {
        return None;
    }
    struct L {
        start: usize,
        ws: usize,
        commented: bool,
        blank: bool,
    }
    let (first, last) = selected_lines(index, sel);
    let lines: Vec<L> = (first..=last)
        .map(|l| {
            let r = index.line_range(l);
            let line = &text[r.clone()];
            let ws = leading_whitespace(line).len();
            let rest = &line[ws..];
            L {
                start: r.start,
                ws,
                commented: rest.starts_with(prefix),
                blank: rest.is_empty(),
            }
        })
        .collect();
    let non_blank: Vec<&L> = lines.iter().filter(|l| !l.blank).collect();
    let mut edits = Vec::new();
    if !non_blank.is_empty() && non_blank.iter().all(|l| l.commented) {
        for l in non_blank {
            let start = l.start + l.ws;
            let mut end = start + prefix.len();
            if text[end..].starts_with(' ') {
                end += 1;
            }
            edits.push(Edit::new(start..end, ""));
        }
    } else {
        let targets: Vec<&L> = if non_blank.is_empty() {
            lines.iter().collect()
        } else {
            non_blank
        };
        let min_ws = targets.iter().map(|l| l.ws).min().unwrap_or(0);
        let insert = format!("{prefix} ");
        for l in targets {
            let at = l.start + min_ws;
            edits.push(Edit::new(at..at, insert.clone()));
        }
    }
    let selection = map_selection(sel, &edits);
    Some(EditPlan { edits, selection })
}

/// Toggle a block comment (`open` / `close`) around the selection, or around
/// the trimmed content of the cursor line when nothing is selected.
pub fn toggle_block_comment(
    text: &str,
    index: &LineIndex,
    sel: Selection,
    open: &str,
    close: &str,
) -> Option<EditPlan> {
    if open.is_empty() || close.is_empty() {
        return None;
    }
    let range = if sel.is_empty() {
        let r = index.line_range(index.line_of_offset(sel.cursor));
        let line = &text[r.clone()];
        let lead = line.len() - line.trim_start().len();
        let trail = line.len() - line.trim_end().len();
        if lead + trail >= line.len() {
            return None;
        }
        (r.start + lead)..(r.end - trail)
    } else {
        sel.range()
    };
    let s = &text[range.clone()];
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    let inner_start = range.start + lead;
    let inner_end = range.end - trail;
    let inner = &text[inner_start..inner_end.max(inner_start)];
    if inner.len() >= open.len() + close.len() && inner.starts_with(open) && inner.ends_with(close)
    {
        let mut open_end = inner_start + open.len();
        if text[open_end..].starts_with(' ') && open_end < inner_end - close.len() {
            open_end += 1;
        }
        let mut close_start = inner_end - close.len();
        if close_start > open_end && text[..close_start].ends_with(' ') {
            close_start -= 1;
        }
        let edits = vec![
            Edit::new(inner_start..open_end, ""),
            Edit::new(close_start..inner_end, ""),
        ];
        let new_start = range.start;
        let new_end = range.end - (open_end - inner_start) - (inner_end - close_start);
        let selection = if sel.is_empty() {
            Selection::caret(map_offset(sel.cursor, &edits, false))
        } else if sel.anchor <= sel.cursor {
            Selection::new(new_start, new_end)
        } else {
            Selection::new(new_end, new_start)
        };
        return Some(EditPlan { edits, selection });
    }
    let open_ins = format!("{open} ");
    let close_ins = format!(" {close}");
    let edits = vec![
        Edit::new(range.start..range.start, open_ins.clone()),
        Edit::new(range.end..range.end, close_ins.clone()),
    ];
    let new_end = range.end + open_ins.len() + close_ins.len();
    let selection = if sel.is_empty() {
        Selection::caret(sel.cursor + open_ins.len())
    } else if sel.anchor <= sel.cursor {
        Selection::new(range.start, new_end)
    } else {
        Selection::new(new_end, range.start)
    };
    Some(EditPlan { edits, selection })
}

/// Indent the selected lines by `unit` (empty lines are skipped).
pub fn indent_lines(text: &str, index: &LineIndex, sel: Selection, unit: &str) -> Option<EditPlan> {
    let (first, last) = selected_lines(index, sel);
    let edits: Vec<Edit> = (first..=last)
        .filter(|&l| !index.line_range(l).is_empty())
        .map(|l| {
            let at = index.line_start(l);
            Edit::new(at..at, unit)
        })
        .collect();
    let _ = text;
    if edits.is_empty() {
        return None;
    }
    let selection = map_selection(sel, &edits);
    Some(EditPlan { edits, selection })
}

/// Remove one indentation level (a tab, or up to `tab_size` spaces back to
/// the previous tab stop) from the selected lines.
pub fn outdent_lines(
    text: &str,
    index: &LineIndex,
    sel: Selection,
    tab_size: usize,
) -> Option<EditPlan> {
    let tab_size = tab_size.max(1);
    let (first, last) = selected_lines(index, sel);
    let mut edits = Vec::new();
    for l in first..=last {
        let r = index.line_range(l);
        let line = &text[r.clone()];
        let remove = if line.starts_with('\t') {
            1
        } else {
            let spaces = line.bytes().take_while(|&b| b == b' ').count();
            if spaces == 0 {
                0
            } else {
                let k = spaces % tab_size;
                if k == 0 { tab_size.min(spaces) } else { k }
            }
        };
        if remove > 0 {
            edits.push(Edit::new(r.start..r.start + remove, ""));
        }
    }
    if edits.is_empty() {
        return None;
    }
    let selection = map_selection(sel, &edits);
    Some(EditPlan { edits, selection })
}

/// Replace the selection with a tab, or with spaces up to the next tab stop.
pub fn insert_indent(
    text: &str,
    index: &LineIndex,
    sel: Selection,
    tab_size: usize,
    insert_spaces: bool,
) -> EditPlan {
    let start = sel.start();
    if !insert_spaces {
        return replace_selection(sel, "\t");
    }
    let tab_size = tab_size.max(1);
    let ls = index.line_start(index.line_of_offset(start));
    let col = visual_width(&text[ls..start], tab_size);
    let n = tab_size - col % tab_size;
    replace_selection(sel, &" ".repeat(n))
}

/// Insert a newline keeping the indentation of the current line; adds one
/// level after an opening `(` and splits `(|)` onto three lines.
pub fn newline(text: &str, index: &LineIndex, sel: Selection, unit: &str) -> EditPlan {
    let range = sel.range();
    let line = index.line_of_offset(range.start);
    let ls = index.line_start(line);
    let le = index.line_end(line);
    let indent_full = leading_whitespace(&text[ls..le]);
    let indent = &indent_full[..indent_full.len().min(range.start - ls)];
    let before = text[ls..range.start].trim_end();
    let opens = before.ends_with('(') || before.ends_with('[') || before.ends_with('{');
    let after = text[range.end..].trim_start_matches([' ', '\t']);
    let closes =
        opens && (after.starts_with(')') || after.starts_with(']') || after.starts_with('}'));
    if closes {
        let first = format!("\n{indent}{unit}");
        let ins = format!("{first}\n{indent}");
        // Drop the spaces between the cursor and the closing bracket.
        let ws_after = text[range.end..].len() - after.len();
        let cursor = range.start + first.len();
        return EditPlan::single(
            range.start..range.end + ws_after,
            ins,
            Selection::caret(cursor),
        );
    }
    let ins = if opens {
        format!("\n{indent}{unit}")
    } else {
        format!("\n{indent}")
    };
    replace_selection(sel, &ins)
}

/// Swap the selected lines with the line above (`up`) or below.
pub fn move_lines(text: &str, index: &LineIndex, sel: Selection, up: bool) -> Option<EditPlan> {
    let (first, last) = selected_lines(index, sel);
    let block_start = index.line_start(first);
    let block_end = index.line_end(last);
    let block = &text[block_start..block_end];
    let (range, new_text, delta): (Range<usize>, String, isize) = if up {
        if first == 0 {
            return None;
        }
        let prev = index.line_range(first - 1);
        let prev_text = &text[prev.clone()];
        (
            prev.start..block_end,
            format!("{block}\n{prev_text}"),
            -(prev_text.len() as isize + 1),
        )
    } else {
        if last + 1 >= index.line_count() {
            return None;
        }
        let next = index.line_range(last + 1);
        let next_text = &text[next.clone()];
        (
            block_start..next.end,
            format!("{next_text}\n{block}"),
            next_text.len() as isize + 1,
        )
    };
    let shift = |o: usize| ((o as isize + delta).max(0) as usize).min(text.len());
    let selection = Selection::new(shift(sel.anchor), shift(sel.cursor));
    Some(EditPlan::single(range, new_text, selection))
}

/// Duplicate the selection (inserted after it and selected) or, without a
/// selection, the cursor line (inserted below; the cursor moves with it).
pub fn duplicate(text: &str, index: &LineIndex, sel: Selection) -> EditPlan {
    if sel.is_empty() {
        let r = index.line_range(index.line_of_offset(sel.cursor));
        let line = &text[r.clone()];
        let ins = format!("\n{line}");
        let cursor = sel.cursor + ins.len();
        return EditPlan::single(r.end..r.end, ins, Selection::caret(cursor));
    }
    let range = sel.range();
    let copy = text[range.clone()].to_owned();
    let len = copy.len();
    let selection = if sel.anchor <= sel.cursor {
        Selection::new(range.end, range.end + len)
    } else {
        Selection::new(range.end + len, range.end)
    };
    EditPlan::single(range.end..range.end, copy, selection)
}

/// Delete the selected lines (including their newline); the cursor keeps its
/// column on the line that moves up.
pub fn delete_lines(text: &str, index: &LineIndex, sel: Selection) -> EditPlan {
    let (first, last) = selected_lines(index, sel);
    let start = index.line_start(first);
    let col = index.char_col(text, sel.cursor);
    if last + 1 < index.line_count() {
        let end = index.line_start(last + 1);
        let next = index.line_range(last + 1);
        let next_line = &text[next];
        let byte_col = next_line
            .char_indices()
            .nth(col)
            .map_or(next_line.len(), |(i, _)| i);
        EditPlan::single(start..end, "", Selection::caret(start + byte_col))
    } else if first > 0 {
        let prev_end = start - 1;
        let prev = index.line_range(first - 1);
        let prev_line = &text[prev.clone()];
        let byte_col = prev_line
            .char_indices()
            .nth(col)
            .map_or(prev_line.len(), |(i, _)| i);
        EditPlan::single(
            prev_end..text.len(),
            "",
            Selection::caret(prev.start + byte_col),
        )
    } else {
        EditPlan::single(0..text.len(), "", Selection::caret(0))
    }
}

/// Upper- or lower-case the selection (or the word at the cursor).
pub fn change_case(
    text: &str,
    sel: Selection,
    upper: bool,
    is_word: impl Fn(char) -> bool,
) -> Option<EditPlan> {
    let range = if sel.is_empty() {
        word_range_at(text, sel.cursor, is_word)?
    } else {
        sel.range()
    };
    let s = &text[range.clone()];
    let new = if upper {
        s.to_uppercase()
    } else {
        s.to_lowercase()
    };
    if new == s {
        return None;
    }
    let new_end = range.start + new.len();
    let selection = if sel.is_empty() {
        let rel = (sel.cursor - range.start).min(new.len());
        Selection::caret(range.start + floor_char_boundary(&new, rel))
    } else if sel.anchor <= sel.cursor {
        Selection::new(range.start, new_end)
    } else {
        Selection::new(new_end, range.start)
    };
    Some(EditPlan::single(range, new, selection))
}

/// Delete backwards: the selection, an auto-inserted bracket pair around the
/// cursor (`delete_pair`), spaces back to the previous tab stop inside the
/// indentation (soft tabs), or the previous char.
pub fn backspace(
    text: &str,
    index: &LineIndex,
    sel: Selection,
    tab_size: usize,
    soft_tabs: bool,
    delete_pair: bool,
) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let c = sel.cursor;
    if c == 0 {
        return None;
    }
    if delete_pair {
        let end = next_char_boundary(text, c);
        let start = prev_char_boundary(text, c);
        return Some(EditPlan::single(start..end, "", Selection::caret(start)));
    }
    let ls = index.line_start(index.line_of_offset(c));
    let before = &text[ls..c];
    if soft_tabs && !before.is_empty() && before.bytes().all(|b| b == b' ') {
        let tab_size = tab_size.max(1);
        let n = (before.len() - 1) % tab_size + 1;
        return Some(EditPlan::single(c - n..c, "", Selection::caret(c - n)));
    }
    let start = prev_char_boundary(text, c);
    Some(EditPlan::single(start..c, "", Selection::caret(start)))
}

/// Delete forwards: the selection or the next char.
pub fn delete_forward(text: &str, sel: Selection) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let c = sel.cursor;
    if c >= text.len() {
        return None;
    }
    let end = next_char_boundary(text, c);
    Some(EditPlan::single(c..end, "", Selection::caret(c)))
}

/// Delete the selection or back to the previous word boundary.
pub fn delete_word_back(
    text: &str,
    sel: Selection,
    is_word: impl Fn(char) -> bool,
) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let start = prev_word_boundary(text, sel.cursor, is_word);
    (start < sel.cursor).then(|| EditPlan::single(start..sel.cursor, "", Selection::caret(start)))
}

/// Delete the selection or forward to the next word boundary.
pub fn delete_word_forward(
    text: &str,
    sel: Selection,
    is_word: impl Fn(char) -> bool,
) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let end = next_word_boundary(text, sel.cursor, is_word);
    (end > sel.cursor).then(|| EditPlan::single(sel.cursor..end, "", Selection::caret(sel.cursor)))
}

/// Delete the selection or from the line start to the cursor (joins with the
/// previous line when the cursor is at the line start).
pub fn delete_to_line_start(text: &str, index: &LineIndex, sel: Selection) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let c = sel.cursor;
    let ls = index.line_start(index.line_of_offset(c));
    let start = if ls == c {
        prev_char_boundary(text, c)
    } else {
        ls
    };
    (start < c).then(|| EditPlan::single(start..c, "", Selection::caret(start)))
}

/// Delete the selection or from the cursor to the line end (joins with the
/// next line when the cursor is at the line end).
pub fn delete_to_line_end(text: &str, index: &LineIndex, sel: Selection) -> Option<EditPlan> {
    if !sel.is_empty() {
        return Some(replace_selection(sel, ""));
    }
    let c = sel.cursor;
    let le = index.line_end(index.line_of_offset(c));
    let end = if le == c {
        next_char_boundary(text, c)
    } else {
        le
    };
    (end > c).then(|| EditPlan::single(c..end, "", Selection::caret(c)))
}

/// "Smart home": first non-blank char of the line, or the line start if the
/// cursor is already there.
pub fn smart_home(text: &str, index: &LineIndex, cursor: usize) -> usize {
    let r = index.line_range(index.line_of_offset(cursor));
    let first_non_blank = r.start + leading_whitespace(&text[r.clone()]).len();
    if cursor == first_non_blank {
        r.start
    } else {
        first_non_blank
    }
}

/// Bracket pairs considered by matching and auto-close.
pub const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
/// Quote characters considered by auto-close.
pub const QUOTES: [char; 3] = ['\'', '"', '`'];

/// Position of the bracket matching the one just before or at `cursor`.
/// Returns `(bracket, matching)` byte offsets. `is_code(offset)` must return
/// `false` for offsets inside strings / comments; scanning stops after
/// `limit` bytes.
pub fn matching_bracket(
    text: &str,
    cursor: usize,
    is_code: impl Fn(usize) -> bool,
    limit: usize,
) -> Option<(usize, usize)> {
    let candidates = [
        cursor.checked_sub(1).filter(|&p| text.is_char_boundary(p)),
        Some(cursor),
    ];
    for pos in candidates.into_iter().flatten() {
        let Some(c) = char_at(text, pos) else {
            continue;
        };
        if !is_code(pos) {
            continue;
        }
        for (open, close) in BRACKETS {
            if c == open {
                let mut depth = 0usize;
                for (i, ch) in text[pos..].char_indices().take_while(|(i, _)| *i <= limit) {
                    let at = pos + i;
                    if (ch == open || ch == close) && !is_code(at) {
                        continue;
                    }
                    if ch == open {
                        depth += 1;
                    } else if ch == close {
                        depth -= 1;
                        if depth == 0 {
                            return Some((pos, at));
                        }
                    }
                }
                return None;
            }
            if c == close {
                let mut depth = 0usize;
                let floor = pos.saturating_sub(limit);
                for (i, ch) in text[..pos + c.len_utf8()].char_indices().rev() {
                    if i < floor {
                        break;
                    }
                    if (ch == open || ch == close) && !is_code(i) {
                        continue;
                    }
                    if ch == close {
                        depth += 1;
                    } else if ch == open {
                        depth -= 1;
                        if depth == 0 {
                            return Some((pos, i));
                        }
                    }
                }
                return None;
            }
        }
    }
    None
}

/// An auto-inserted closing character the user may type over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoPair {
    /// Offset of the opening char.
    pub open: usize,
    /// Offset of the auto-inserted closing char.
    pub close: usize,
    /// The opening char.
    pub open_char: char,
    /// The closing char.
    pub close_char: char,
}

impl AutoPair {
    /// Still valid for `text` (both chars still at their offsets)?
    pub fn is_valid(&self, text: &str) -> bool {
        self.open < self.close
            && char_at(text, self.open) == Some(self.open_char)
            && char_at(text, self.close) == Some(self.close_char)
    }
}

/// What typing a character should do (auto-close support).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedAction {
    /// Apply this plan (pair insertion or wrapping); optionally track the
    /// inserted pair for over-typing.
    Plan(EditPlan, Option<AutoPair>),
    /// Move the caret over the existing closing char.
    Skip(usize),
    /// Insert the char normally.
    Insert,
}

fn closer_for(c: char) -> Option<char> {
    BRACKETS
        .iter()
        .find(|(o, _)| *o == c)
        .map(|(_, cl)| *cl)
        .or_else(|| QUOTES.contains(&c).then_some(c))
}

/// Decide how to handle typing `ch` (auto-closing brackets and quotes).
///
/// * `in_literal`: the caret is inside a string / comment token.
/// * `tracked_close_at(offset)`: an auto-inserted closer is at `offset`.
pub fn typed_char(
    text: &str,
    sel: Selection,
    ch: char,
    in_literal: bool,
    is_word: impl Fn(char) -> bool,
    tracked_close_at: impl Fn(usize) -> bool,
) -> TypedAction {
    let is_closer = BRACKETS.iter().any(|(_, c)| *c == ch) || QUOTES.contains(&ch);
    if sel.is_empty()
        && is_closer
        && char_at(text, sel.cursor) == Some(ch)
        && tracked_close_at(sel.cursor)
    {
        return TypedAction::Skip(sel.cursor + ch.len_utf8());
    }
    let Some(close) = closer_for(ch) else {
        return TypedAction::Insert;
    };
    if !sel.is_empty() {
        let r = sel.range();
        let edits = vec![
            Edit::new(r.start..r.start, ch.to_string()),
            Edit::new(r.end..r.end, close.to_string()),
        ];
        let (a, c) = (r.start + ch.len_utf8(), r.end + ch.len_utf8());
        let selection = if sel.anchor <= sel.cursor {
            Selection::new(a, c)
        } else {
            Selection::new(c, a)
        };
        return TypedAction::Plan(EditPlan { edits, selection }, None);
    }
    if in_literal {
        return TypedAction::Insert;
    }
    let next_ok = match char_at(text, sel.cursor) {
        None => true,
        Some(n) => n.is_whitespace() || matches!(n, ')' | ']' | '}' | ';' | ','),
    };
    if !next_ok {
        return TypedAction::Insert;
    }
    if QUOTES.contains(&ch) && char_before(text, sel.cursor).is_some_and(|p| is_word(p) || p == ch)
    {
        return TypedAction::Insert;
    }
    let c = sel.cursor;
    let pair = AutoPair {
        open: c,
        close: c + ch.len_utf8(),
        open_char: ch,
        close_char: close,
    };
    let ins = format!("{ch}{close}");
    TypedAction::Plan(
        EditPlan::single(c..c, ins, Selection::caret(c + ch.len_utf8())),
        Some(pair),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::is_word_char;

    /// Parse `"ab[cd|ef"`: `|` is the cursor, `[` the anchor (optional).
    fn parse(marked: &str) -> (String, Selection) {
        let mut text = String::new();
        let (mut anchor, mut cursor) = (None, None);
        for c in marked.chars() {
            match c {
                '|' => cursor = Some(text.len()),
                '[' => anchor = Some(text.len()),
                c => text.push(c),
            }
        }
        let cursor = cursor.expect("cursor marker");
        (text, Selection::new(anchor.unwrap_or(cursor), cursor))
    }

    fn render(text: &str, sel: Selection) -> String {
        let mut out = String::new();
        for (i, c) in text.char_indices() {
            if i == sel.anchor && !sel.is_empty() {
                out.push('[');
            }
            if i == sel.cursor {
                out.push('|');
            }
            out.push(c);
        }
        if sel.anchor == text.len() && !sel.is_empty() {
            out.push('[');
        }
        if sel.cursor == text.len() {
            out.push('|');
        }
        out
    }

    fn run(marked: &str, f: impl Fn(&str, &LineIndex, Selection) -> Option<EditPlan>) -> String {
        let (text, sel) = parse(marked);
        let idx = LineIndex::new(&text);
        match f(&text, &idx, sel) {
            Some(plan) => {
                let new = plan.apply_to(&text);
                assert!(
                    plan.selection.anchor <= new.len() && plan.selection.cursor <= new.len(),
                    "selection out of range"
                );
                assert!(
                    new.is_char_boundary(plan.selection.cursor)
                        && new.is_char_boundary(plan.selection.anchor)
                );
                render(&new, plan.selection)
            }
            None => "<none>".to_owned(),
        }
    }

    #[test]
    fn map_offsets() {
        let edits = vec![Edit::new(2..2, "XX"), Edit::new(5..7, "y")];
        assert_eq!(map_offset(0, &edits, true), 0);
        assert_eq!(map_offset(2, &edits, false), 2);
        assert_eq!(map_offset(2, &edits, true), 4);
        assert_eq!(map_offset(4, &edits, true), 6);
        assert_eq!(map_offset(6, &edits, false), 7);
        assert_eq!(map_offset(6, &edits, true), 8);
        assert_eq!(map_offset(7, &edits, false), 8);
        assert_eq!(map_offset(9, &edits, false), 10);
    }

    #[test]
    fn line_comment_toggle() {
        let f = |t: &str, i: &LineIndex, s| toggle_line_comment(t, i, s, "--");
        assert_eq!(run("SELECT| 1", f), "-- SELECT| 1");
        assert_eq!(run("-- SELECT| 1", f), "SELECT| 1");
        assert_eq!(run("--SELECT 1|", f), "SELECT 1|");
        assert_eq!(run("|x", f), "-- |x");
        // Multi-line: aligned at the minimum indentation, blank lines skipped.
        assert_eq!(run("[  a\n\n    b\n|c", f), "[  -- a\n\n  --   b\n|c");
        assert_eq!(run("[  -- a\n\n  --   b|", f), "[  a\n\n    b|");
        // Mixed: comments everything.
        assert_eq!(run("[-- a\nb|", f), "[-- -- a\n-- b|");
        // Selection ending at column 0 excludes that line.
        assert_eq!(run("[a\nb\n|c", f), "[-- a\n-- b\n|c");
        // Only blank lines.
        assert_eq!(run("  |", f), "  -- |");
        // Unicode.
        assert_eq!(run("é|ü", f), "-- é|ü");
        assert_eq!(run("-- é|ü", f), "é|ü");
        assert_eq!(
            toggle_line_comment("x", &LineIndex::new("x"), Selection::caret(0), ""),
            None
        );
    }

    #[test]
    fn block_comment_toggle() {
        let f = |t: &str, i: &LineIndex, s| toggle_block_comment(t, i, s, "/*", "*/");
        assert_eq!(run("a [b c| d", f), "a [/* b c */| d");
        assert_eq!(run("a [/* b c */| d", f), "a [b c| d");
        assert_eq!(run("  sel|ect  ", f), "  /* sel|ect */  ");
        assert_eq!(run("  /* sel|ect */  ", f), "  sel|ect  ");
        assert_eq!(run("[/**/|", f), "|");
        assert_eq!(run("   |", f), "<none>");
    }

    #[test]
    fn indent_and_outdent() {
        let ind = |t: &str, i: &LineIndex, s| indent_lines(t, i, s, "    ");
        assert_eq!(run("[a\n\nb|c", ind), "[    a\n\n    b|c");
        assert_eq!(run("x[a\nb|", ind), "    x[a\n    b|");
        let out = |t: &str, i: &LineIndex, s| outdent_lines(t, i, s, 4);
        assert_eq!(run("[    a\n\tb\n  c|", out), "[a\nb\nc|");
        assert_eq!(run("      a|", out), "    a|");
        assert_eq!(run("a|", out), "<none>");
        assert_eq!(run("  [ a|", out), "[a|");
        let tab = |t: &str, i: &LineIndex, s| Some(insert_indent(t, i, s, 4, true));
        assert_eq!(run("ab|", tab), "ab  |");
        assert_eq!(run("|x", tab), "    |x");
        assert_eq!(run("a\tb|", tab), "a\tb   |");
        assert_eq!(run("a[bc|", tab), "a   |");
        let hard = |t: &str, i: &LineIndex, s| Some(insert_indent(t, i, s, 4, false));
        assert_eq!(run("ab|", hard), "ab\t|");
    }

    #[test]
    fn smart_newline() {
        let f = |t: &str, i: &LineIndex, s| Some(newline(t, i, s, "    "));
        assert_eq!(run("  select|", f), "  select\n  |");
        assert_eq!(run("  count(|)", f), "  count(\n      |\n  )");
        assert_eq!(run("  in (|  )", f), "  in (\n      |\n  )");
        assert_eq!(run("values (|", f), "values (\n    |");
        assert_eq!(run("a[bc|d", f), "a\n|d");
        assert_eq!(run(" | x", f), " \n | x");
    }

    #[test]
    fn move_lines_up_down() {
        let up = |t: &str, i: &LineIndex, s| move_lines(t, i, s, true);
        let down = |t: &str, i: &LineIndex, s| move_lines(t, i, s, false);
        assert_eq!(run("a\nb|b\nc", up), "b|b\na\nc");
        assert_eq!(run("a\nb|b\nc", down), "a\nc\nb|b");
        assert_eq!(run("a|\nb", up), "<none>");
        assert_eq!(run("a\nb|", down), "<none>");
        assert_eq!(run("a\n[b\n|c", down), "a\nc\n[b|");
        assert_eq!(run("a\n[b\nc|\nd", up), "[b\nc|\na\nd");
        assert_eq!(run("x\né|ü", up), "é|ü\nx");
    }

    #[test]
    fn duplicate_and_delete_lines() {
        let dup = |t: &str, i: &LineIndex, s| Some(duplicate(t, i, s));
        assert_eq!(run("ab|c\nd", dup), "abc\nab|c\nd");
        assert_eq!(run("a[bc|d", dup), "abc[bc|d");
        assert_eq!(run("a|bc[d", dup), "abc|bc[d");
        let del = |t: &str, i: &LineIndex, s| Some(delete_lines(t, i, s));
        assert_eq!(run("one\ntw|o\nthree", del), "one\nth|ree");
        assert_eq!(run("one\ntwo\nthr|ee", del), "one\ntwo|");
        assert_eq!(run("on|ly", del), "|");
        assert_eq!(run("[a\nb|\nc", del), "c|");
        assert_eq!(run("abcdef|\nx", del), "x|");
    }

    #[test]
    fn case_changes() {
        let up = |t: &str, _: &LineIndex, s| change_case(t, s, true, is_word_char);
        let low = |t: &str, _: &LineIndex, s| change_case(t, s, false, is_word_char);
        assert_eq!(run("select [from|", up), "select [FROM|");
        assert_eq!(run("sel|ect x", up), "SEL|ECT x");
        assert_eq!(run("SELECT| x", low), "select| x");
        assert_eq!(run("|straße[", up), "|STRASSE[");
        assert_eq!(run("ABC|", up), "<none>");
        assert_eq!(run(" | ", up), "<none>");
    }

    #[test]
    fn deletions() {
        let bs = |t: &str, i: &LineIndex, s| backspace(t, i, s, 4, true, false);
        assert_eq!(run("ab|c", bs), "a|c");
        assert_eq!(run("|abc", bs), "<none>");
        assert_eq!(run("a\n|b", bs), "a|b");
        assert_eq!(run("        |x", bs), "    |x");
        assert_eq!(run("      |x", bs), "    |x");
        assert_eq!(run("é👍|", bs), "é|");
        assert_eq!(run("a[bc|d", bs), "a|d");
        let pair = |t: &str, i: &LineIndex, s| backspace(t, i, s, 4, true, true);
        assert_eq!(run("f(|)", pair), "f|");
        let del = |t: &str, _: &LineIndex, s| delete_forward(t, s);
        assert_eq!(run("a|é", del), "a|");
        assert_eq!(run("a|", del), "<none>");
        let wb = |t: &str, _: &LineIndex, s| delete_word_back(t, s, is_word_char);
        assert_eq!(run("select foo_bar|", wb), "select |");
        assert_eq!(run("x  |", wb), "|");
        let wf = |t: &str, _: &LineIndex, s| delete_word_forward(t, s, is_word_char);
        assert_eq!(run("|select foo", wf), "| foo");
        let ls = |t: &str, i: &LineIndex, s| delete_to_line_start(t, i, s);
        assert_eq!(run("a\nbc|d", ls), "a\n|d");
        assert_eq!(run("a\n|d", ls), "a|d");
        let le = |t: &str, i: &LineIndex, s| delete_to_line_end(t, i, s);
        assert_eq!(run("a|bc\nd", le), "a|\nd");
        assert_eq!(run("abc|\nd", le), "abc|d");
    }

    #[test]
    fn smart_home_toggles() {
        let t = "    select";
        let i = LineIndex::new(t);
        assert_eq!(smart_home(t, &i, 8), 4);
        assert_eq!(smart_home(t, &i, 4), 0);
        assert_eq!(smart_home(t, &i, 0), 4);
    }

    #[test]
    fn bracket_matching() {
        let code = |_: usize| true;
        let t = "f(a, (b), [c])";
        assert_eq!(matching_bracket(t, 2, code, 10_000), Some((1, 13)));
        assert_eq!(matching_bracket(t, 1, code, 10_000), Some((1, 13)));
        assert_eq!(matching_bracket(t, 14, code, 10_000), Some((13, 1)));
        assert_eq!(matching_bracket(t, 8, code, 10_000), Some((7, 5)));
        assert_eq!(matching_bracket(t, 3, code, 10_000), None);
        assert_eq!(matching_bracket("(()", 0, code, 10_000), None);
        // Brackets inside a string are ignored.
        let t = "f(')', x)";
        let in_str = |i: usize| !(2..5).contains(&i);
        assert_eq!(matching_bracket(t, 2, in_str, 10_000), Some((1, 8)));
        // Scan limit.
        let long = format!("({})", "x".repeat(100));
        assert_eq!(matching_bracket(&long, 0, code, 50), None);
        assert_eq!(matching_bracket(&long, 0, code, 200), Some((0, 101)));
        assert_eq!(matching_bracket("é(x)", 3, code, 100), Some((2, 4)));
    }

    #[test]
    fn auto_close_decisions() {
        let none = |_: usize| false;
        let act = |marked: &str, ch: char, lit: bool| {
            let (t, s) = parse(marked);
            match typed_char(&t, s, ch, lit, is_word_char, none) {
                TypedAction::Plan(p, pair) => format!(
                    "{}{}",
                    render(&p.apply_to(&t), p.selection),
                    if pair.is_some() { " +pair" } else { "" }
                ),
                TypedAction::Skip(c) => format!("skip {c}"),
                TypedAction::Insert => "insert".to_owned(),
            }
        };
        assert_eq!(act("count|", '(', false), "count(|) +pair");
        assert_eq!(act("|x", '(', false), "insert");
        assert_eq!(act("f(|)", '(', false), "f((|)) +pair");
        assert_eq!(act("where a = |", '\'', false), "where a = '|' +pair");
        assert_eq!(act("don|", '\'', false), "insert");
        assert_eq!(act("'abc|", '\'', true), "insert");
        assert_eq!(act("x = [abc|", '\'', false), "x = '[abc|'");
        assert_eq!(act("x = |abc[", '(', false), "x = (|abc[)");
        assert_eq!(act("a|", ')', false), "insert");
        // Skip-over only for tracked (auto-inserted) closers.
        let (t, s) = parse("f(|)");
        assert_eq!(
            typed_char(&t, s, ')', false, is_word_char, |o| o == 2),
            TypedAction::Skip(3)
        );
        assert_eq!(
            typed_char(&t, s, ')', false, is_word_char, none),
            TypedAction::Insert
        );
        let pair = AutoPair {
            open: 1,
            close: 2,
            open_char: '(',
            close_char: ')',
        };
        assert!(pair.is_valid("f()"));
        assert!(!pair.is_valid("f(x"));
    }

    #[test]
    fn plan_helpers() {
        let p = replace_selection(Selection::new(1, 3), "xyz");
        assert_eq!(p.apply_to("abcd"), "axyzd");
        assert_eq!(p.selection, Selection::caret(4));
        assert_eq!(p.len_delta(), 1);
        let idx = LineIndex::new("a\nb\nc");
        assert_eq!(selected_lines(&idx, Selection::new(0, 4)), (0, 1));
        assert_eq!(selected_lines(&idx, Selection::new(0, 3)), (0, 1));
        assert_eq!(selected_lines(&idx, Selection::caret(4)), (2, 2));
    }
}
