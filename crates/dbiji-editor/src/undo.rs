//! Operation-based undo / redo history.
//!
//! Every text change is recorded as an [`EditRecord`] (what was removed and
//! inserted at which offset, in the coordinates of the text at the time the
//! edit was applied). Records are grouped into undo steps; consecutive
//! typing, backspacing or IME composition coalesce into one step (a new step
//! starts at word boundaries, after a pause or when the cursor moves). Memory
//! use is proportional to the edited text, not to the document size.

use std::collections::VecDeque;
use std::ops::Range;

use crate::text::Selection;

/// One primitive change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    /// Byte offset of the change.
    pub start: usize,
    /// Text that was removed at `start`.
    pub removed: String,
    /// Text that was inserted at `start`.
    pub inserted: String,
}

/// How an edit was produced; decides coalescing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// Typed characters.
    Typing,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// IME composition / commit.
    Ime,
    /// Everything else (paste, commands, programmatic edits); never coalesced.
    Other,
}

#[derive(Clone, Debug)]
struct Group {
    id: u64,
    edits: Vec<EditRecord>,
    before: Selection,
    after: Selection,
    kind: EditKind,
    time: f64,
    open: bool,
    bytes: usize,
    /// Records are non-overlapping and stored in descending start order
    /// (recorded from one multi-edit plan); replayed as one batch.
    batch: bool,
}

/// Edits to apply for an undo or redo, plus the selection to restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    /// `(range, replacement)` pairs. Applied in order, unless `batch`.
    pub edits: Vec<(Range<usize>, String)>,
    /// Selection after replaying.
    pub selection: Selection,
    /// The edits are sorted, non-overlapping and expressed in the
    /// coordinates of the text before the replay (apply them in one pass).
    pub batch: bool,
}

/// Seconds after which typing starts a new undo step.
const COALESCE_SECONDS: f64 = 1.5;

/// Undo / redo history.
#[derive(Clone, Debug)]
pub struct UndoStack {
    undo: VecDeque<Group>,
    redo: Vec<Group>,
    limit: usize,
    max_bytes: usize,
    bytes: usize,
    next_id: u64,
    /// Id of the state at the bottom of the undo stack.
    base_id: u64,
    saved: Option<u64>,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new(1000)
    }
}

impl UndoStack {
    /// History with at most `limit` steps (and at most 64 MiB of recorded text).
    pub fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: limit.max(1),
            max_bytes: 64 * 1024 * 1024,
            bytes: 0,
            next_id: 1,
            base_id: 0,
            saved: Some(0),
        }
    }

    /// Change the step limit (oldest steps are dropped if needed).
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        self.trim();
    }

    /// Forget all history (the current text becomes the "saved" state).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.bytes = 0;
        self.base_id = self.next_id;
        self.next_id += 1;
        self.saved = Some(self.base_id);
    }

    /// `true` if there is something to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// `true` if there is something to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn top_id(&self) -> u64 {
        self.undo.back().map_or(self.base_id, |g| g.id)
    }

    /// Mark the current state as saved.
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.top_id());
    }

    /// `true` if the text differs from the last saved state (undoing back to
    /// the saved state clears the flag).
    pub fn is_modified(&self) -> bool {
        self.saved != Some(self.top_id())
    }

    /// Close the current step so that the next edit starts a new one (call
    /// when the cursor is moved by the user).
    pub fn close_group(&mut self) {
        if let Some(g) = self.undo.back_mut() {
            g.open = false;
        }
    }

    /// Record edits (in application order) as one step or merge them into
    /// the current one.
    pub fn record(
        &mut self,
        kind: EditKind,
        edits: Vec<EditRecord>,
        before: Selection,
        after: Selection,
        now: f64,
    ) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        let bytes: usize = edits
            .iter()
            .map(|e| e.removed.len() + e.inserted.len())
            .sum();
        if let Some(last) = self.undo.back_mut()
            && last.open
            && last.kind == kind
            && kind != EditKind::Other
            && (kind == EditKind::Ime || now - last.time <= COALESCE_SECONDS)
            && Self::try_merge(last, &edits, kind)
        {
            last.after = after;
            last.time = now;
            last.bytes += bytes;
            self.bytes += bytes;
            self.trim();
            return;
        }
        if let Some(last) = self.undo.back_mut() {
            last.open = false;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push_back(Group {
            id,
            edits,
            before,
            after,
            kind,
            time: now,
            open: kind != EditKind::Other,
            bytes,
            batch: false,
        });
        self.bytes += bytes;
        self.trim();
    }

    /// Record a multi-edit step whose records are non-overlapping and in
    /// descending start order (as produced by applying a sorted edit plan
    /// back to front). Such steps are undone / redone in one pass.
    pub fn record_batch(
        &mut self,
        edits: Vec<EditRecord>,
        before: Selection,
        after: Selection,
        now: f64,
    ) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        if let Some(last) = self.undo.back_mut() {
            last.open = false;
        }
        let bytes: usize = edits
            .iter()
            .map(|e| e.removed.len() + e.inserted.len())
            .sum();
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push_back(Group {
            id,
            edits,
            before,
            after,
            kind: EditKind::Other,
            time: now,
            open: false,
            bytes,
            batch: true,
        });
        self.bytes += bytes;
        self.trim();
    }

    fn try_merge(last: &mut Group, edits: &[EditRecord], kind: EditKind) -> bool {
        if kind == EditKind::Ime {
            last.edits.extend_from_slice(edits);
            return true;
        }
        let [new] = edits else { return false };
        let Some(prev) = last.edits.last_mut() else {
            return false;
        };
        match kind {
            EditKind::Typing => {
                let contiguous =
                    new.removed.is_empty() && new.start == prev.start + prev.inserted.len();
                let starts_word = new.inserted.starts_with(char::is_whitespace)
                    && !prev.inserted.ends_with(char::is_whitespace);
                let newline = new.inserted.contains('\n');
                if contiguous && !starts_word && !newline {
                    prev.inserted.push_str(&new.inserted);
                    return true;
                }
                false
            }
            EditKind::Backspace => {
                if new.inserted.is_empty()
                    && prev.inserted.is_empty()
                    && new.start + new.removed.len() == prev.start
                {
                    prev.start = new.start;
                    prev.removed.insert_str(0, &new.removed);
                    return true;
                }
                false
            }
            EditKind::Delete => {
                if new.inserted.is_empty() && prev.inserted.is_empty() && new.start == prev.start {
                    prev.removed.push_str(&new.removed);
                    return true;
                }
                false
            }
            EditKind::Ime | EditKind::Other => false,
        }
    }

    fn trim(&mut self) {
        while self.undo.len() > self.limit || (self.bytes > self.max_bytes && self.undo.len() > 1) {
            if let Some(g) = self.undo.pop_front() {
                self.bytes -= g.bytes;
                self.base_id = g.id;
            }
        }
    }

    /// Pop the last step; returns the edits that revert it.
    pub fn undo(&mut self) -> Option<Replay> {
        let mut g = self.undo.pop_back()?;
        g.open = false;
        self.bytes -= g.bytes;
        let edits = if g.batch {
            // Ascending starts; each inserted text sits after the length
            // changes of all lower records.
            let mut shift = 0isize;
            g.edits
                .iter()
                .rev()
                .map(|e| {
                    let pos = (e.start as isize + shift).max(0) as usize;
                    shift += e.inserted.len() as isize - e.removed.len() as isize;
                    (pos..pos + e.inserted.len(), e.removed.clone())
                })
                .collect()
        } else {
            g.edits
                .iter()
                .rev()
                .map(|e| (e.start..e.start + e.inserted.len(), e.removed.clone()))
                .collect()
        };
        let selection = g.before;
        let batch = g.batch;
        self.redo.push(g);
        Some(Replay {
            edits,
            selection,
            batch,
        })
    }

    /// Re-apply the last undone step.
    pub fn redo(&mut self) -> Option<Replay> {
        let g = self.redo.pop()?;
        let edits = if g.batch {
            g.edits
                .iter()
                .rev()
                .map(|e| (e.start..e.start + e.removed.len(), e.inserted.clone()))
                .collect()
        } else {
            g.edits
                .iter()
                .map(|e| (e.start..e.start + e.removed.len(), e.inserted.clone()))
                .collect()
        };
        let selection = g.after;
        let batch = g.batch;
        self.bytes += g.bytes;
        self.undo.push_back(g);
        Some(Replay {
            edits,
            selection,
            batch,
        })
    }

    /// Number of undo steps.
    #[cfg(test)]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(start: usize, removed: &str, inserted: &str) -> EditRecord {
        EditRecord {
            start,
            removed: removed.into(),
            inserted: inserted.into(),
        }
    }

    fn apply(text: &mut String, r: &Replay) {
        for (range, s) in &r.edits {
            text.replace_range(range.clone(), s);
        }
    }

    #[test]
    fn typing_coalesces_per_word() {
        let mut u = UndoStack::new(100);
        let mut text = String::new();
        let mut t = 0.0;
        for (i, c) in "ab cd".chars().enumerate() {
            u.record(
                EditKind::Typing,
                vec![rec(i, "", &c.to_string())],
                Selection::caret(i),
                Selection::caret(i + 1),
                t,
            );
            text.push(c);
            t += 0.1;
        }
        assert_eq!(u.undo_len(), 2);
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "ab");
        assert_eq!(r.selection, Selection::caret(2));
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "");
        assert!(u.undo().is_none());
        let r = u.redo().expect("redo");
        apply(&mut text, &r);
        assert_eq!(text, "ab");
        let r = u.redo().expect("redo");
        apply(&mut text, &r);
        assert_eq!(text, "ab cd");
        assert!(u.redo().is_none());
    }

    #[test]
    fn pause_and_cursor_moves_split_steps() {
        let mut u = UndoStack::new(100);
        u.record(
            EditKind::Typing,
            vec![rec(0, "", "a")],
            Selection::caret(0),
            Selection::caret(1),
            0.0,
        );
        u.record(
            EditKind::Typing,
            vec![rec(1, "", "b")],
            Selection::caret(1),
            Selection::caret(2),
            5.0,
        );
        assert_eq!(u.undo_len(), 2);
        u.close_group();
        u.record(
            EditKind::Typing,
            vec![rec(2, "", "c")],
            Selection::caret(2),
            Selection::caret(3),
            5.1,
        );
        assert_eq!(u.undo_len(), 3);
        // Non-contiguous typing never merges.
        u.record(
            EditKind::Typing,
            vec![rec(0, "", "z")],
            Selection::caret(0),
            Selection::caret(1),
            5.2,
        );
        assert_eq!(u.undo_len(), 4);
    }

    #[test]
    fn backspace_and_delete_coalesce() {
        let mut u = UndoStack::new(100);
        let mut text = String::from("hello");
        for i in (2..5).rev() {
            let removed = text[i..i + 1].to_owned();
            text.replace_range(i..i + 1, "");
            u.record(
                EditKind::Backspace,
                vec![rec(i, &removed, "")],
                Selection::caret(i + 1),
                Selection::caret(i),
                0.0,
            );
        }
        assert_eq!(text, "he");
        assert_eq!(u.undo_len(), 1);
        for _ in 0..2 {
            let removed = text[0..1].to_owned();
            text.replace_range(0..1, "");
            u.record(
                EditKind::Delete,
                vec![rec(0, &removed, "")],
                Selection::caret(0),
                Selection::caret(0),
                0.1,
            );
        }
        assert_eq!(text, "");
        assert_eq!(u.undo_len(), 2);
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "he");
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "hello");
        assert_eq!(r.selection, Selection::caret(5));
    }

    #[test]
    fn multi_edit_group_reverts_in_reverse_order() {
        let mut u = UndoStack::new(100);
        let mut text = String::from("a\nb\nc");
        // Comment lines 2 and 0 (applied high offsets first).
        let edits = vec![rec(4, "", "-- "), rec(2, "", "-- "), rec(0, "", "-- ")];
        for e in &edits {
            text.replace_range(e.start..e.start, &e.inserted);
        }
        assert_eq!(text, "-- a\n-- b\n-- c");
        u.record(
            EditKind::Other,
            edits,
            Selection::caret(0),
            Selection::caret(3),
            0.0,
        );
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "a\nb\nc");
        let r = u.redo().expect("redo");
        apply(&mut text, &r);
        assert_eq!(text, "-- a\n-- b\n-- c");
    }

    #[test]
    fn saved_state_tracking() {
        let mut u = UndoStack::new(100);
        assert!(!u.is_modified());
        u.record(
            EditKind::Other,
            vec![rec(0, "", "x")],
            Selection::caret(0),
            Selection::caret(1),
            0.0,
        );
        assert!(u.is_modified());
        u.mark_saved();
        assert!(!u.is_modified());
        u.undo();
        assert!(u.is_modified());
        u.redo();
        assert!(!u.is_modified());
        u.undo();
        u.record(
            EditKind::Other,
            vec![rec(0, "", "y")],
            Selection::caret(0),
            Selection::caret(1),
            1.0,
        );
        assert!(u.is_modified());
        u.undo();
        assert!(
            u.is_modified(),
            "saved state is unreachable after a divergent edit"
        );
    }

    #[test]
    fn limits_drop_oldest() {
        let mut u = UndoStack::new(3);
        for i in 0..10 {
            u.record(
                EditKind::Other,
                vec![rec(i, "", "x")],
                Selection::caret(i),
                Selection::caret(i + 1),
                i as f64,
            );
        }
        assert_eq!(u.undo_len(), 3);
        u.set_limit(1);
        assert_eq!(u.undo_len(), 1);
        u.clear();
        assert!(!u.can_undo() && !u.can_redo() && !u.is_modified());
    }

    #[test]
    fn ime_groups_merge_regardless_of_time() {
        let mut u = UndoStack::new(10);
        u.record(
            EditKind::Ime,
            vec![rec(0, "", "n")],
            Selection::caret(0),
            Selection::new(0, 1),
            0.0,
        );
        u.record(
            EditKind::Ime,
            vec![rec(0, "n", "ni")],
            Selection::new(0, 1),
            Selection::new(0, 2),
            9.0,
        );
        u.record(
            EditKind::Ime,
            vec![rec(0, "ni", "你")],
            Selection::new(0, 2),
            Selection::caret(3),
            10.0,
        );
        assert_eq!(u.undo_len(), 1);
        let mut text = String::from("你");
        let r = u.undo().expect("undo");
        apply(&mut text, &r);
        assert_eq!(text, "");
        assert_eq!(r.selection, Selection::caret(0));
    }
}
