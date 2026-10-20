//! [`EditorState`]: the document, selection, history and all per-editor UI
//! state. The application owns one per editor tab and passes it to
//! [`crate::SqlEditor`] every frame.

use std::hash::Hash;
use std::ops::Range;

use egui::{Context, Id, Vec2};

use crate::completion::CompletionState;
use crate::find_bar::{FindState, GotoState};
use crate::language::EditorLanguage;
use crate::layout::DocLayout;
use crate::ops::{self, AutoPair, Edit, EditPlan};
use crate::options::EditorOptions;
use crate::text::{
    LineEnding, LineIndex, Selection, clamp_range, common_prefix_len, common_suffix_len,
    floor_char_boundary, normalize_newlines, word_range_at,
};
use crate::undo::{EditKind, EditRecord, Replay, UndoStack};
use crate::widget::ContextAction;

/// Severity of a [`Diagnostic`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Severity {
    /// Informational hint (blue).
    Info,
    /// Warning (yellow squiggle).
    Warning,
    /// Error (red squiggle).
    Error,
}

/// A problem reported by the application (parse error, server error
/// position, lint ...). Shown as a squiggle, a gutter marker and a tooltip.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Byte range in the document (may be empty for a position).
    pub range: Range<usize>,
    /// Severity.
    pub severity: Severity,
    /// Message shown on hover.
    pub message: String,
}

impl Diagnostic {
    /// Construct a diagnostic.
    pub fn new(range: Range<usize>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            range,
            severity,
            message: message.into(),
        }
    }
}

/// Cursor, selection and scroll position, for persisting editor tabs across
/// sessions (FR-S1).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ViewState {
    /// Selection anchor (byte offset).
    pub anchor: usize,
    /// Cursor (byte offset).
    pub cursor: usize,
    /// Scroll offset in points.
    pub scroll: [f32; 2],
}

/// How the widget should scroll on its next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollRequest {
    /// Minimal scroll to make the cursor visible.
    Cursor,
    /// Center the cursor line vertically.
    Center,
}

/// State of one editor: text, selection, undo history, diagnostics and
/// transient UI state (find bar, completion popup ...).
#[derive(Clone)]
pub struct EditorState {
    pub(crate) id: Id,
    pub(crate) text: String,
    pub(crate) index: LineIndex,
    version: u64,
    line_ending: LineEnding,
    pub(crate) sel: Selection,
    /// Preferred x position for vertical cursor movement.
    pub(crate) h_pos: Option<f32>,
    /// Cursor affinity at soft-wrap boundaries.
    pub(crate) prefer_next_row: bool,
    pub(crate) undo: UndoStack,
    pub(crate) layout: DocLayout,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) statement: Option<Range<usize>>,
    pub(crate) spell: Vec<Range<usize>>,
    pub(crate) find: FindState,
    pub(crate) goto: GotoState,
    pub(crate) completion: CompletionState,
    pub(crate) auto_pairs: Vec<AutoPair>,
    /// IME composition (pre-edit) range.
    pub(crate) preedit: Option<Range<usize>>,
    /// Where a composition was just cleared (a commit may follow).
    pub(crate) ime_cleared_at: Option<usize>,
    pub(crate) pending_focus: bool,
    pub(crate) pending_scroll: Option<ScrollRequest>,
    pub(crate) scroll_offset: Vec2,
    pub(crate) restore_scroll: Option<Vec2>,
    pub(crate) viewport_size: Vec2,
    pub(crate) last_reported_cursor: Option<Selection>,
    pub(crate) had_focus: bool,
    /// Time of the last edit or cursor move (cursor blink phase).
    pub(crate) blink_epoch: f64,
    /// Time of the last text change.
    pub(crate) last_edit_time: f64,
    /// When a debounced find rescan is due.
    pub(crate) pending_find_rescan: Option<f64>,
    pub(crate) edited_this_frame: bool,
    pub(crate) now: f64,
    pub(crate) context_offset: Option<usize>,
    pub(crate) spell_menu: Option<(Range<usize>, String, Vec<String>)>,
    pub(crate) hover_cache: Option<(u64, Range<usize>, Option<String>)>,
    pub(crate) link_cache: Option<(u64, usize, Option<Range<usize>>)>,
    pub(crate) hover_link: Option<Range<usize>>,
    pub(crate) drag_select: Option<DragMode>,
    /// Foldable regions and collapsed folds.
    pub(crate) folds: crate::fold::Folds,
    /// Visible occurrences of the word under the cursor (painted this frame).
    pub(crate) occurrences: Vec<Range<usize>>,
    /// Galley of the lines exposed to AccessKit last frame and the char
    /// index of its first character (maps screen-reader selections back).
    pub(crate) a11y_view: Option<(std::sync::Arc<egui::Galley>, usize)>,
}

/// What a primary-button drag extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragMode {
    /// Character-wise (text area).
    Chars,
    /// Whole lines (gutter), anchored at the given line.
    Lines(usize),
}

impl std::fmt::Debug for EditorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorState")
            .field("id", &self.id)
            .field("len", &self.text.len())
            .field("version", &self.version)
            .field("selection", &self.sel)
            .finish_non_exhaustive()
    }
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new("dbiji_editor_default", String::new())
    }
}

/// Stable 64-bit hash of an id source (deterministic across runs so egui's
/// persisted scroll state keeps working).
pub(crate) fn hash_id(source: &impl Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

/// Maps offsets through several non-overlapping edits (sorted by start, in
/// the coordinates of the text before the edits) in `O(log edits)`.
pub(crate) struct EditMapper<'a> {
    edits: &'a [Edit],
    /// `cum[k]` = total length change of `edits[..k]`.
    cum: Vec<isize>,
}

impl<'a> EditMapper<'a> {
    pub(crate) fn new(edits: &'a [Edit]) -> Self {
        let mut cum = Vec::with_capacity(edits.len() + 1);
        let mut acc = 0isize;
        cum.push(0);
        for e in edits {
            acc += e.text.len() as isize - e.range.len() as isize;
            cum.push(acc);
        }
        Self { edits, cum }
    }

    /// New position of `offset` (`stick_right` as in [`map_through`]).
    pub(crate) fn map(&self, offset: usize, stick_right: bool) -> usize {
        let k = self.edits.partition_point(|e| e.range.end < offset);
        let before = self.cum[k];
        let local = match self.edits.get(k) {
            Some(e) if offset >= e.range.start => {
                map_through(offset, &e.range, e.text.len(), stick_right)
            }
            _ => offset,
        };
        (local as isize + before).max(0) as usize
    }
}

/// Map `offset` through replacing `range` by `ins_len` bytes.
fn map_through(offset: usize, range: &Range<usize>, ins_len: usize, stick_right: bool) -> usize {
    if offset < range.start || (offset == range.start && !(range.is_empty() && stick_right)) {
        offset
    } else if offset > range.end || (offset == range.end && !range.is_empty()) {
        offset + ins_len - range.len()
    } else if range.is_empty() {
        offset + ins_len
    } else {
        range.start + if stick_right { ins_len } else { 0 }
    }
}

impl EditorState {
    /// Create an editor for `text`. `id` must be unique among the editors
    /// shown at the same time (e.g. the script path or tab id).
    pub fn new(id: impl Hash, text: String) -> Self {
        let line_ending = LineEnding::detect(&text);
        let text = normalize_newlines(&text).into_owned();
        let index = LineIndex::new(&text);
        let mut layout = DocLayout::new();
        layout.reset(&text, &index);
        Self {
            id: Id::new(("dbiji_editor", hash_id(&id))),
            text,
            index,
            version: 0,
            line_ending,
            sel: Selection::caret(0),
            h_pos: None,
            prefer_next_row: true,
            undo: UndoStack::default(),
            layout,
            diagnostics: Vec::new(),
            statement: None,
            spell: Vec::new(),
            find: FindState::default(),
            goto: GotoState::default(),
            completion: CompletionState::default(),
            auto_pairs: Vec::new(),
            preedit: None,
            ime_cleared_at: None,
            pending_focus: false,
            pending_scroll: None,
            scroll_offset: Vec2::ZERO,
            restore_scroll: None,
            viewport_size: Vec2::new(600.0, 400.0),
            last_reported_cursor: None,
            had_focus: false,
            blink_epoch: 0.0,
            last_edit_time: f64::NEG_INFINITY,
            pending_find_rescan: None,
            edited_this_frame: false,
            now: 0.0,
            context_offset: None,
            spell_menu: None,
            hover_cache: None,
            link_cache: None,
            hover_link: None,
            drag_select: None,
            folds: Default::default(),
            occurrences: Vec::new(),
            a11y_view: None,
        }
    }

    /// egui id of the text area (focus, AccessKit node).
    pub fn id(&self) -> Id {
        self.id
    }

    /// The document text (`\n` line endings).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text with the line endings it was loaded with (for saving).
    pub fn text_with_original_line_endings(&self) -> std::borrow::Cow<'_, str> {
        self.line_ending.apply(&self.text)
    }

    /// Line ending detected when the text was loaded.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Monotonically increasing counter, bumped on every text change.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// `true` if the text differs from the state last marked with
    /// [`EditorState::mark_saved`] (undoing back to it clears the flag).
    pub fn is_modified(&self) -> bool {
        self.undo.is_modified()
    }

    /// Mark the current text as saved.
    pub fn mark_saved(&mut self) {
        self.undo.mark_saved();
    }

    /// Number of lines (at least 1).
    pub fn line_count(&self) -> usize {
        self.index.line_count()
    }

    /// Byte range of a 0-based line (without its `\n`).
    pub fn line_range(&self, line: usize) -> Range<usize> {
        self.index.line_range(line)
    }

    /// 0-based line containing `offset`.
    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.index.line_of_offset(offset.min(self.text.len()))
    }

    /// Character index of byte `offset` (e.g. to report positions to tools
    /// that count characters). `O(log lines + line length)`.
    pub fn byte_to_char(&self, offset: usize) -> usize {
        self.index.byte_to_char(&self.text, offset)
    }

    /// Byte offset of character index `ch`, clamped to the text length
    /// (PostgreSQL error positions, for example, count characters).
    pub fn char_to_byte(&self, ch: usize) -> usize {
        self.index.char_to_byte(&self.text, ch)
    }

    /// Replace the whole text as one undoable edit (only the changed middle
    /// part is recorded; the selection is preserved where possible). Use it
    /// for formatting or other whole-document transformations.
    pub fn set_text(&mut self, ctx: &Context, text: String) {
        let text = normalize_newlines(&text).into_owned();
        let prefix = common_prefix_len(&self.text, &text);
        let suffix = common_suffix_len(&self.text, &text, prefix);
        if prefix == self.text.len() && prefix == text.len() {
            return;
        }
        let range = prefix..self.text.len() - suffix;
        let middle = &text[prefix..text.len() - suffix];
        let before = self.sel;
        let rec = self.raw_edit(range.clone(), middle);
        let map =
            |o: usize| floor_char_boundary(&self.text, map_through(o, &range, middle.len(), false));
        let sel = Selection::new(map(before.anchor), map(before.cursor));
        self.set_selection(sel);
        self.undo
            .record(EditKind::Other, vec![rec], before, self.sel, self.now);
        ctx.request_repaint();
    }

    /// Load new content: replaces the text, clears undo history, diagnostics
    /// and selection, scrolls to the top and marks the text as saved.
    pub fn reset_text(&mut self, ctx: &Context, text: String) {
        self.line_ending = LineEnding::detect(&text);
        self.text = normalize_newlines(&text).into_owned();
        self.index = LineIndex::new(&self.text);
        self.layout.reset(&self.text, &self.index);
        self.version += 1;
        self.edited_this_frame = true;
        self.undo.clear();
        self.diagnostics.clear();
        self.statement = None;
        self.spell.clear();
        self.auto_pairs.clear();
        self.preedit = None;
        self.completion.close();
        self.hover_cache = None;
        self.link_cache = None;
        self.sel = Selection::caret(0);
        self.h_pos = None;
        self.restore_scroll = Some(Vec2::ZERO);
        ctx.request_repaint();
    }

    /// Byte offset of the cursor (moving end of the selection).
    pub fn cursor(&self) -> usize {
        self.sel.cursor
    }

    /// Selected byte range, `None` if the selection is empty.
    pub fn selection(&self) -> Option<Range<usize>> {
        (!self.sel.is_empty()).then(|| self.sel.range())
    }

    /// Selection anchor and cursor (byte offsets).
    pub fn selection_ends(&self) -> (usize, usize) {
        (self.sel.anchor, self.sel.cursor)
    }

    /// The selected text, `None` if nothing is selected.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|r| &self.text[r])
    }

    /// Move the cursor to `offset` (clamped, snapped to a char boundary) and
    /// scroll it into view.
    pub fn set_cursor(&mut self, ctx: &Context, offset: usize) {
        self.set_selection(Selection::caret(offset));
        self.undo.close_group();
        self.pending_scroll = Some(ScrollRequest::Cursor);
        ctx.request_repaint();
    }

    /// Select `range` (cursor at its end) and scroll it into view.
    pub fn select(&mut self, ctx: &Context, range: Range<usize>) {
        self.set_selection(Selection::new(range.start, range.end));
        self.undo.close_group();
        self.pending_scroll = Some(ScrollRequest::Cursor);
        ctx.request_repaint();
    }

    /// Replace `range` with `text` (one undo step). The selection follows the
    /// edit.
    pub fn replace_range(&mut self, ctx: &Context, range: Range<usize>, text: &str) {
        let text = normalize_newlines(text);
        let range = clamp_range(&self.text, range);
        let before = self.sel;
        let rec = self.raw_edit(range.clone(), &text);
        let map = |o: usize, right: bool| map_through(o, &range, text.len(), right);
        let sel = if before.is_empty() {
            Selection::caret(map(before.cursor, true))
        } else {
            Selection::new(
                map(before.anchor, before.anchor > before.cursor),
                map(before.cursor, before.cursor >= before.anchor),
            )
        };
        self.set_selection(sel);
        self.undo
            .record(EditKind::Other, vec![rec], before, self.sel, self.now);
        ctx.request_repaint();
    }

    /// Insert `text` at the cursor, replacing the selection; the cursor ends
    /// up after the inserted text.
    pub fn insert_at_cursor(&mut self, ctx: &Context, text: &str) {
        let text = normalize_newlines(text);
        let plan = ops::replace_selection(self.sel, &text);
        self.apply_plan(plan, EditKind::Other);
        self.pending_scroll = Some(ScrollRequest::Cursor);
        ctx.request_repaint();
    }

    /// 1-based `(line, column)` of the cursor; the column counts characters.
    pub fn line_col(&self) -> (usize, usize) {
        let line = self.index.line_of_offset(self.sel.cursor);
        (
            line + 1,
            self.index.char_col(&self.text, self.sel.cursor) + 1,
        )
    }

    /// Move the cursor to the start of 1-based `line` (clamped) and center it.
    pub fn goto_line(&mut self, ctx: &Context, line: usize) {
        self.goto_line_col(ctx, line, 1);
    }

    /// Move the cursor to 1-based `line` and character `column` (clamped).
    pub fn goto_line_col(&mut self, ctx: &Context, line: usize, column: usize) {
        let l = line.saturating_sub(1).min(self.index.line_count() - 1);
        let offset = self
            .index
            .offset_of_char_col(&self.text, l, column.saturating_sub(1));
        self.set_selection(Selection::caret(offset));
        self.undo.close_group();
        self.pending_scroll = Some(ScrollRequest::Center);
        ctx.request_repaint();
    }

    /// Replace all diagnostics. Ranges move with subsequent edits until the
    /// application sets new ones.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        let mut clamped = 0usize;
        self.diagnostics = diagnostics
            .into_iter()
            .map(|mut d| {
                let range = clamp_range(&self.text, d.range.clone());
                clamped += usize::from(range != d.range);
                d.range = range;
                d
            })
            .collect();
        self.diagnostics.sort_by_key(|d| d.range.start);
        if clamped > 0 {
            tracing::debug!(
                clamped,
                text_len = self.text.len(),
                "diagnostic ranges outside the text or off char boundaries were clamped"
            );
        }
    }

    /// Current diagnostics (adjusted for edits since they were set).
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Highlight the statement at `range` (e.g. the statement under the
    /// cursor that "execute" would run), or clear it with `None`.
    pub fn set_statement_highlight(&mut self, range: Option<Range<usize>>) {
        self.statement = range.map(|r| clamp_range(&self.text, r));
    }

    /// Current statement highlight.
    pub fn statement_highlight(&self) -> Option<Range<usize>> {
        self.statement.clone()
    }

    /// Mark misspelled words (dotted underline; right click offers
    /// [`EditorLanguage::spell_suggestions`] and "add to dictionary").
    pub fn set_spell_issues(&mut self, ranges: Vec<Range<usize>>) {
        let mut clamped = 0usize;
        let mut v: Vec<Range<usize>> = ranges
            .into_iter()
            .map(|r| {
                let c = clamp_range(&self.text, r.clone());
                clamped += usize::from(c != r);
                c
            })
            .filter(|r| !r.is_empty())
            .collect();
        v.sort_by_key(|r| r.start);
        self.spell = v;
        if clamped > 0 {
            tracing::debug!(
                clamped,
                text_len = self.text.len(),
                "spell-check ranges outside the text or off char boundaries were clamped"
            );
        }
    }

    /// Current spell-check ranges.
    pub fn spell_issues(&self) -> &[Range<usize>] {
        &self.spell
    }

    /// Give keyboard focus to the editor (takes effect this or next frame).
    pub fn request_focus(&self, ctx: &Context) {
        ctx.memory_mut(|m| m.request_focus(self.id));
        ctx.request_repaint();
    }

    /// Does the text area have keyboard focus?
    pub fn has_focus(&self, ctx: &Context) -> bool {
        ctx.memory(|m| m.has_focus(self.id))
    }

    /// Open the find bar (with the replace row when `replace`), prefilled
    /// with the selection if it is a single line.
    pub fn open_find(&mut self, replace: bool) {
        if let Some(sel) = self.selected_text()
            && !sel.contains('\n')
            && !sel.is_empty()
        {
            self.find.query = sel.to_owned();
        }
        self.find.open = true;
        self.find.replace_open = replace;
        self.find.focus_query = true;
        self.find.dirty = true;
    }

    /// Close the find bar.
    pub fn close_find(&mut self) {
        self.find.open = false;
        self.pending_focus = true;
    }

    /// Is the find bar open?
    pub fn is_find_open(&self) -> bool {
        self.find.open
    }

    /// Open the go-to-line popup.
    pub fn open_goto_line(&mut self) {
        self.goto.open = true;
        self.goto.input.clear();
        self.goto.focus = true;
    }

    /// Open the completion popup at the cursor on the next frame.
    pub fn trigger_completion(&mut self) {
        self.completion.request_manual();
    }

    /// Re-query an open completion popup (e.g. after metadata finished loading).
    pub fn refresh_completion(&mut self) {
        if self.completion.is_open() {
            self.completion.request_manual();
        }
    }

    /// Close the completion popup.
    pub fn close_completion(&mut self) {
        self.completion.close();
    }

    /// Is the completion popup open?
    pub fn is_completion_open(&self) -> bool {
        self.completion.is_open()
    }

    /// Undo the last change.
    pub fn undo(&mut self, ctx: &Context) -> bool {
        let done = self.undo_internal();
        ctx.request_repaint();
        done
    }

    /// Redo the last undone change.
    pub fn redo(&mut self, ctx: &Context) -> bool {
        let done = self.redo_internal();
        ctx.request_repaint();
        done
    }

    /// Is there anything to undo?
    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    /// Is there anything to redo?
    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Cursor, selection and scroll position for persistence.
    pub fn view_state(&self) -> ViewState {
        ViewState {
            anchor: self.sel.anchor,
            cursor: self.sel.cursor,
            scroll: [self.scroll_offset.x, self.scroll_offset.y],
        }
    }

    /// Restore what [`EditorState::view_state`] returned (clamped to the text).
    pub fn restore_view_state(&mut self, view: ViewState) {
        self.set_selection(Selection::new(view.anchor, view.cursor));
        let scroll = Vec2::new(view.scroll[0].max(0.0), view.scroll[1].max(0.0));
        self.restore_scroll = Some(scroll);
        self.scroll_offset = scroll;
    }

    /// Byte range of the identifier-like word at `offset`.
    pub fn word_at(&self, lang: &dyn EditorLanguage, offset: usize) -> Option<Range<usize>> {
        word_range_at(&self.text, offset.min(self.text.len()), |c| {
            lang.is_word_char(c)
        })
    }

    /// Apply an editor action (the editor-local [`ContextAction`]s: cut,
    /// copy, paste, select all, comments, case changes, word replacement).
    /// Application actions (execute, explain, format, add word) are returned
    /// as `false` and must be handled by the caller.
    pub fn apply_action(
        &mut self,
        ctx: &Context,
        lang: &dyn EditorLanguage,
        options: &EditorOptions,
        action: &ContextAction,
    ) -> bool {
        let handled = crate::input::apply_context_action(self, ctx, lang, options, action);
        ctx.request_repaint();
        handled
    }

    // ----------------------------------------------------------------------
    // Folding

    /// Recompute the foldable regions (after edits) and apply the collapsed
    /// ones to the layout; expands folds hiding the cursor.
    pub(crate) fn sync_folds(&mut self, lang: &dyn EditorLanguage) {
        let version = self.version;
        // Big documents: recompute once typing pauses (regions are shifted meanwhile).
        let idle = self.now - self.last_edit_time > 0.4;
        if self.folds.is_stale(version) && (self.text.len() <= 200_000 || idle) {
            self.folds.update(&self.text, &self.index, version, lang);
        }
        let cursor_line = self.index.line_of_offset(self.sel.cursor);
        self.folds.reveal(cursor_line);
        let hidden = self.folds.hidden();
        if hidden != self.folds.applied {
            self.layout.set_hidden(&hidden);
            self.folds.applied = hidden;
        }
    }

    /// Forget all folds (folding switched off).
    pub(crate) fn clear_folds(&mut self) {
        if !self.folds.applied.is_empty() || !self.folds.collapsed.is_empty() {
            self.folds.collapsed.clear();
            self.layout.set_hidden(&[]);
            self.folds.applied.clear();
        }
    }

    /// Collapse every foldable region.
    pub fn fold_all(&mut self) {
        self.folds.collapsed = self.folds.regions.iter().map(|r| r.0).collect();
    }

    /// Expand every folded region.
    pub fn unfold_all(&mut self) {
        self.folds.collapsed.clear();
    }

    /// Collapse or expand the innermost region at the cursor.
    pub fn toggle_fold_at_cursor(&mut self) -> bool {
        let line = self.index.line_of_offset(self.sel.cursor);
        match self.folds.region_containing(line) {
            Some((start, _)) => {
                self.folds.toggle(start);
                if self.folds.collapsed.contains(&start) && line != start {
                    // Keep the cursor visible: on the fold's first line.
                    self.sel = Selection::caret(self.index.line_start(start));
                }
                true
            }
            None => false,
        }
    }

    /// First lines of the collapsed regions.
    pub fn folded_lines(&self) -> Vec<usize> {
        self.folds.collapsed.iter().copied().collect()
    }

    /// Foldable regions as (first line, last line).
    pub fn fold_regions(&self) -> Vec<(usize, usize)> {
        self.folds.regions.clone()
    }

    /// The line is hidden in a collapsed region.
    pub fn is_line_hidden(&self, line: usize) -> bool {
        self.layout.is_hidden(line)
    }

    // Internal mutation API

    /// Replace `range` (current coordinates) with `text` and update every
    /// derived structure. Returns the undo record (not yet recorded).
    pub(crate) fn raw_edit(&mut self, range: Range<usize>, text: &str) -> EditRecord {
        let range = clamp_range(&self.text, range);
        let removed = self.text[range.clone()].to_owned();
        let first = self.index.line_of_offset(range.start);
        let old_last = self.index.line_of_offset(range.end);
        self.index.edit(&self.text, range.clone(), text);
        self.text.replace_range(range.clone(), text);
        let new_last = self.index.line_of_offset(range.start + text.len());
        self.layout.splice(
            &self.text,
            &self.index,
            first,
            old_last - first + 1,
            new_last - first + 1,
        );
        self.folds
            .on_splice(first, old_last - first + 1, new_last - first + 1);
        let ins_len = text.len();
        self.shift_markers(&|o, right| map_through(o, &range, ins_len, right));
        self.after_edit();
        EditRecord {
            start: range.start,
            removed,
            inserted: text.to_owned(),
        }
    }

    fn after_edit(&mut self) {
        self.version += 1;
        self.edited_this_frame = true;
        self.last_edit_time = self.now;
        self.blink_epoch = self.now;
        self.hover_cache = None;
        self.link_cache = None;
    }

    /// Apply many non-overlapping edits (sorted by start, current
    /// coordinates) in one `O(text + edits)` pass: one text rebuild, one line
    /// index rebuild, one layout splice and one marker pass, instead of an
    /// `O(text)` memmove per edit. Returns the undo records in application
    /// order and whether they form a batch (see `UndoStack::record_batch`).
    pub(crate) fn apply_batch(&mut self, edits: &[Edit]) -> (Vec<EditRecord>, bool) {
        let clean: Vec<Edit> = edits
            .iter()
            .map(|e| Edit::new(clamp_range(&self.text, e.range.clone()), e.text.clone()))
            .collect();
        let sorted = clean.windows(2).all(|w| w[0].range.end <= w[1].range.start);
        let (Some(first_edit), Some(last_edit), true) =
            (clean.first(), clean.last(), sorted && clean.len() > 2)
        else {
            let records = clean
                .iter()
                .rev()
                .map(|e| self.raw_edit(e.range.clone(), &e.text))
                .collect();
            return (records, false);
        };
        let records: Vec<EditRecord> = clean
            .iter()
            .rev()
            .map(|e| EditRecord {
                start: e.range.start,
                removed: self.text[e.range.clone()].to_owned(),
                inserted: e.text.clone(),
            })
            .collect();
        let first_line = self.index.line_of_offset(first_edit.range.start);
        let old_last_line = self.index.line_of_offset(last_edit.range.end);
        let delta: isize = clean
            .iter()
            .map(|e| e.text.len() as isize - e.range.len() as isize)
            .sum();
        let mut new = String::with_capacity((self.text.len() as isize + delta).max(0) as usize);
        let mut pos = 0usize;
        for e in &clean {
            new.push_str(&self.text[pos..e.range.start]);
            new.push_str(&e.text);
            pos = e.range.end;
        }
        new.push_str(&self.text[pos..]);
        let mapper = EditMapper::new(&clean);
        let last_new_end = mapper.map(last_edit.range.end, true);
        self.text = new;
        self.index = LineIndex::new(&self.text);
        let new_last_line = self.index.line_of_offset(last_new_end);
        self.layout.splice(
            &self.text,
            &self.index,
            first_line,
            old_last_line - first_line + 1,
            new_last_line - first_line + 1,
        );
        self.folds.on_splice(
            first_line,
            old_last_line - first_line + 1,
            new_last_line - first_line + 1,
        );
        self.shift_markers(&|o, right| mapper.map(o, right));
        self.after_edit();
        (records, true)
    }

    /// Move every offset-based marker through an edit; `map(offset,
    /// stick_right)` maps an offset of the text before the edit.
    fn shift_markers(&mut self, map: &dyn Fn(usize, bool) -> usize) {
        let len = self.text.len();
        let fix = |o: usize| o.min(len);
        self.diagnostics.retain_mut(|d| {
            let was_empty = d.range.is_empty();
            let s = fix(map(d.range.start, true));
            let e = fix(map(d.range.end, false)).max(s);
            d.range = s..e;
            was_empty || !d.range.is_empty()
        });
        if let Some(r) = &self.statement {
            let s = fix(map(r.start, false));
            let e = fix(map(r.end, true)).max(s);
            self.statement = Some(s..e);
        }
        self.spell.retain_mut(|r| {
            let s = fix(map(r.start, true));
            let e = fix(map(r.end, false)).max(s);
            *r = s..e;
            s < e
        });
        for p in &mut self.auto_pairs {
            p.open = fix(map(p.open, true));
            p.close = fix(map(p.close, true));
        }
        let text = &self.text;
        self.auto_pairs.retain(|p| p.is_valid(text));
        self.completion.shift(&|o, right| fix(map(o, right)));
        if self.find.open && !self.find.matches.is_empty() {
            self.find.matches.retain_mut(|m| {
                let was_empty = m.start == m.end;
                let s = fix(map(m.start, true));
                let e = fix(map(m.end, false)).max(s);
                *m = s..e;
                was_empty || s < e
            });
        }
        if let Some(p) = &self.preedit {
            let s = fix(map(p.start, false));
            let e = fix(map(p.end, true)).max(s);
            self.preedit = Some(s..e);
        }
    }

    /// Set the selection (clamped); resets the cursor blink and drops
    /// auto-close pairs the cursor left.
    pub(crate) fn set_selection(&mut self, sel: Selection) {
        let sel = sel.clamped(&self.text);
        if sel != self.sel {
            self.blink_epoch = self.now;
        }
        self.sel = sel;
        let c = sel.cursor;
        self.auto_pairs.retain(|p| p.open < c && c <= p.close);
    }

    /// Apply an edit plan as one undo step of `kind`.
    pub(crate) fn apply_plan(&mut self, plan: EditPlan, kind: EditKind) {
        let before = self.sel;
        let (records, batch) = self.apply_batch(&plan.edits);
        self.set_selection(plan.selection);
        self.h_pos = None;
        self.prefer_next_row = true;
        if batch {
            self.undo.record_batch(records, before, self.sel, self.now);
        } else {
            self.undo.record(kind, records, before, self.sel, self.now);
        }
    }

    fn replay(&mut self, replay: Replay) {
        if replay.batch {
            let edits: Vec<Edit> = replay
                .edits
                .into_iter()
                .map(|(r, t)| Edit::new(r, t))
                .collect();
            self.apply_batch(&edits);
        } else {
            for (range, text) in replay.edits {
                self.raw_edit(range, &text);
            }
        }
        self.set_selection(replay.selection);
        self.h_pos = None;
        self.pending_scroll = Some(ScrollRequest::Cursor);
    }

    pub(crate) fn undo_internal(&mut self) -> bool {
        let Some(replay) = self.undo.undo() else {
            return false;
        };
        self.replay(replay);
        true
    }

    pub(crate) fn redo_internal(&mut self) -> bool {
        let Some(replay) = self.undo.redo() else {
            return false;
        };
        self.replay(replay);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context::default()
    }

    #[test]
    fn construction_normalizes_newlines() {
        let s = EditorState::new(1, "a\r\nb\r\n".into());
        assert_eq!(s.text(), "a\nb\n");
        assert_eq!(s.line_ending(), LineEnding::CrLf);
        assert_eq!(s.text_with_original_line_endings(), "a\r\nb\r\n");
        assert_eq!(s.line_count(), 3);
        assert!(!s.is_modified());
    }

    #[test]
    fn replace_range_and_undo() {
        let c = ctx();
        let mut s = EditorState::new(1, "select 1".into());
        s.set_cursor(&c, 8);
        s.replace_range(&c, 7..8, "42");
        assert_eq!(s.text(), "select 42");
        assert_eq!(s.cursor(), 9);
        assert!(s.is_modified());
        assert!(s.undo(&c));
        assert_eq!(s.text(), "select 1");
        assert!(!s.is_modified());
        assert!(s.redo(&c));
        assert_eq!(s.text(), "select 42");
        let v = s.version();
        s.insert_at_cursor(&c, ";");
        assert_eq!(s.text(), "select 42;");
        assert!(s.version() > v);
    }

    #[test]
    fn set_text_records_minimal_diff_and_keeps_cursor() {
        let c = ctx();
        let mut s = EditorState::new(1, "select a from t where x".into());
        s.set_cursor(&c, 23);
        s.set_text(&c, "SELECT a FROM t where x".into());
        assert_eq!(s.cursor(), 23);
        s.undo(&c);
        assert_eq!(s.text(), "select a from t where x");
        s.set_text(&c, "select a from t where x".into());
        assert!(!s.can_redo() || s.text() == "select a from t where x");
    }

    #[test]
    fn markers_follow_edits() {
        let c = ctx();
        let mut s = EditorState::new(1, "select x from t".into());
        s.set_diagnostics(vec![
            Diagnostic::new(7..8, Severity::Error, "unknown column"),
            Diagnostic::new(100..200, Severity::Info, "clamped"),
        ]);
        s.set_statement_highlight(Some(0..15));
        s.set_spell_issues(vec![14..15, 3..3]);
        s.replace_range(&c, 0..0, "  ");
        assert_eq!(s.diagnostics()[0].range, 9..10);
        assert_eq!(s.diagnostics()[1].range, 17..17);
        // Text typed at the start of the statement becomes part of it.
        assert_eq!(s.statement_highlight(), Some(0..17));
        assert_eq!(s.spell_issues().first(), Some(&(16..17)));
        assert_eq!(s.spell_issues().len(), 1);
        // Deleting the diagnosed text drops the (non-empty) diagnostic.
        s.replace_range(&c, 9..10, "");
        assert_eq!(s.diagnostics().len(), 1);
    }

    #[test]
    fn map_through_rules() {
        let r = 5..8;
        assert_eq!(map_through(4, &r, 1, true), 4);
        assert_eq!(map_through(5, &r, 1, true), 5);
        assert_eq!(map_through(6, &r, 1, false), 5);
        assert_eq!(map_through(6, &r, 1, true), 6);
        assert_eq!(map_through(8, &r, 1, false), 6);
        assert_eq!(map_through(9, &r, 1, false), 7);
        let ins = 5..5;
        assert_eq!(map_through(5, &ins, 3, false), 5);
        assert_eq!(map_through(5, &ins, 3, true), 8);
        assert_eq!(map_through(6, &ins, 3, false), 9);
    }

    #[test]
    fn goto_and_line_col() {
        let c = ctx();
        let mut s = EditorState::new(1, "ab\nçde\nf".into());
        s.goto_line_col(&c, 2, 3);
        assert_eq!(s.line_col(), (2, 3));
        assert_eq!(s.cursor(), 6);
        s.goto_line(&c, 99);
        assert_eq!(s.line_col(), (3, 1));
        s.set_cursor(&c, 4);
        assert_eq!(s.cursor(), 3, "snapped to char boundary");
    }

    #[test]
    fn view_state_roundtrip() {
        let c = ctx();
        let mut s = EditorState::new(1, "hello world".into());
        s.select(&c, 2..5);
        let v = s.view_state();
        let json = serde_json::to_string(&v).expect("json");
        let back: ViewState = serde_json::from_str(&json).expect("json");
        let mut t = EditorState::new(2, "hello world".into());
        t.restore_view_state(back);
        assert_eq!(t.selection(), Some(2..5));
        assert_eq!(t.selected_text(), Some("llo"));
    }

    #[test]
    fn batch_plans_match_reference_and_round_trip_through_undo() {
        let c = ctx();
        let base: String = (0..120).map(|i| format!("line {i} é\n")).collect();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let inserts = ["", "x", "\n", "éé\nz", "-- "];
        for _ in 0..60 {
            let mut s = EditorState::new(1, base.clone());
            s.set_diagnostics(vec![
                Diagnostic::new(40..45, Severity::Error, "e"),
                Diagnostic::new(300..310, Severity::Warning, "w"),
            ]);
            let mut edits = Vec::new();
            let mut pos = rnd(20);
            while pos < base.len() {
                let start = crate::text::floor_char_boundary(&base, pos);
                let end = crate::text::floor_char_boundary(&base, (start + rnd(6)).min(base.len()))
                    .max(start);
                edits.push(Edit::new(start..end, inserts[rnd(inserts.len())]));
                pos = end + 1 + rnd(60);
            }
            let plan = EditPlan {
                edits: edits.clone(),
                selection: Selection::caret(0),
            };
            let expected = plan.apply_to(&base);
            // Reference marker positions: map through the edits back to front.
            let reference = |o: usize, right: bool| {
                edits
                    .iter()
                    .rev()
                    .fold(o, |o, e| map_through(o, &e.range, e.text.len(), right))
            };
            let mapper = EditMapper::new(&edits);
            for o in [0, 1, 7, 40, 45, 99, 300, 310, base.len()] {
                assert_eq!(mapper.map(o, true), reference(o, true), "offset {o} right");
                assert_eq!(mapper.map(o, false), reference(o, false), "offset {o} left");
            }
            s.apply_plan(plan, EditKind::Other);
            assert_eq!(s.text(), expected);
            let fresh = LineIndex::new(&expected);
            assert_eq!(s.index.line_count(), fresh.line_count());
            for l in 0..fresh.line_count() {
                assert_eq!(s.index.line_range(l), fresh.line_range(l));
            }
            assert_eq!(s.layout.line_count(), fresh.line_count());
            if let Some(d) = s.diagnostics().first() {
                assert!(d.range.end <= expected.len() && expected.is_char_boundary(d.range.start));
            }
            assert!(s.undo(&c));
            assert_eq!(s.text(), base);
            assert_eq!(s.layout.line_count(), s.index.line_count());
            assert!(s.redo(&c));
            assert_eq!(s.text(), expected);
            assert!(s.undo(&c));
            assert_eq!(s.text(), base);
        }
    }

    #[test]
    fn public_types_are_send_sync_clone() {
        fn check<T: Send + Sync + Clone>() {}
        check::<EditorState>();
        check::<crate::EditorOptions>();
        check::<crate::EditorTheme>();
        check::<Diagnostic>();
        check::<crate::BasicSqlLanguage>();
    }

    #[test]
    fn reset_clears_history() {
        let c = ctx();
        let mut s = EditorState::new(1, "a".into());
        s.insert_at_cursor(&c, "b");
        s.reset_text(&c, "new\r\ncontent".into());
        assert_eq!(s.text(), "new\ncontent");
        assert!(!s.can_undo());
        assert!(!s.is_modified());
        assert_eq!(s.cursor(), 0);
    }
}
