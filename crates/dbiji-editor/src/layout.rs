//! Virtualized document layout.
//!
//! Only lines that are (or were recently) visible own an egui [`Galley`];
//! every line keeps its highlight spans and its row geometry (row count and
//! width). Row heights are uniform, so the y position of any line is a
//! prefix sum lookup. This keeps memory proportional to the visible text and
//! makes an edit cost proportional to the edited lines (plus `O(lines)`
//! bookkeeping on plain integer arrays), independent of document size.
//!
//! Highlighting comes from [`EditorLanguage::highlight`] over the whole
//! document. Small documents are re-highlighted synchronously after every
//! edit; large ones get a per-line re-highlight of the edited lines while
//! typing and a full pass once typing pauses.

use std::ops::Range;
use std::sync::Arc;

use egui::epaint::text::{CharIndex, FontsImpl};
use egui::text::{CCursor, LayoutJob, TextFormat, TextWrapping};
use egui::{Align, Color32, Context, FontId, Galley, Rect, Vec2};

use crate::language::{EditorLanguage, TokenStyle};
use crate::text::{LineIndex, ceil_char_boundary};
use crate::theme::EditorTheme;

/// Layout width used when word wrap is off (effectively unlimited: a line
/// never lays out more than [`MAX_LAYOUT_CHARS`] characters, which bounds
/// the cost of a single row).
pub const LONG_LINE_WRAP: f32 = 4_000_000.0;

/// Upper bound of cached line galleys before far-away ones are evicted
/// (about a dozen screens; a cached line costs ~170 bytes per char).
const MAX_CACHED_GALLEYS: usize = 600;

/// Lines measured per frame while refining wrap-mode estimates.
const REFINE_BATCH: usize = 120;

/// Most lines re-highlighted individually in one frame (large documents).
const LOCAL_HIGHLIGHT_LINES: usize = 500;

/// Longest time (seconds) a large document goes without a full highlight
/// pass while the user keeps typing.
const MAX_HIGHLIGHT_STALENESS: f64 = 2.0;

/// Characters of a single line that are laid out; longer lines (minified
/// dumps) show this prefix followed by an ellipsis. Editing still operates
/// on the whole line.
pub const MAX_LAYOUT_CHARS: usize = 20_000;

/// The part of `line` that is laid out (see [`MAX_LAYOUT_CHARS`]).
pub fn display_prefix(line: &str) -> (&str, bool) {
    if line.len() <= MAX_LAYOUT_CHARS {
        return (line, false);
    }
    match line.char_indices().nth(MAX_LAYOUT_CHARS) {
        Some((i, _)) => (&line[..i], true),
        None => (line, false),
    }
}

/// A highlighted range inside one line (byte offsets relative to the line start).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Start (inclusive).
    pub start: u32,
    /// End (exclusive).
    pub end: u32,
    /// Style.
    pub style: TokenStyle,
}

#[derive(Clone, Debug, Default)]
struct LineInfo {
    spans: Vec<Span>,
    /// Style of a token continuing from the previous line.
    cont: Option<TokenStyle>,
    spans_valid: bool,
    galley: Option<Arc<Galley>>,
    rows: u32,
    width: f32,
    exact: bool,
    /// Inside a collapsed fold: no rows, not painted.
    hidden: bool,
}

/// Inputs that affect geometry or appearance of laid out lines.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutParams {
    /// Font of all text.
    pub font_id: FontId,
    /// Height of one row (font height + line spacing), pixel aligned.
    pub row_height: f32,
    /// Advance of a typical char, for width estimates.
    pub char_width: f32,
    /// `Some(width)` in word-wrap mode.
    pub wrap_width: Option<f32>,
    /// Display scale.
    pub pixels_per_point: f32,
}

impl LayoutParams {
    fn effective_wrap(&self) -> f32 {
        self.wrap_width
            .unwrap_or(LONG_LINE_WRAP)
            .max(self.char_width * 4.0)
    }

    fn same_geometry(&self, other: &LayoutParams) -> bool {
        self.font_id == other.font_id
            && self.row_height == other.row_height
            && self.char_width == other.char_width
            && self.wrap_width == other.wrap_width
            && self.pixels_per_point == other.pixels_per_point
    }
}

/// When to run the (expensive) full-document highlight.
#[derive(Clone, Copy, Debug)]
pub struct HighlightPolicy {
    /// Highlighting enabled for this document (size limit not exceeded).
    pub enabled: bool,
    /// Documents up to this many bytes are highlighted synchronously.
    pub sync_limit: usize,
    /// Debounce (seconds) of the full pass for larger documents.
    pub debounce: f64,
}

/// The virtualized layout model of one document.
#[derive(Clone, Debug, Default)]
pub struct DocLayout {
    lines: Vec<LineInfo>,
    row_starts: Vec<u32>,
    rows_dirty_from: usize,
    params: Option<LayoutParams>,
    theme_key: u64,
    formats: Vec<TextFormat>,
    max_width: f32,
    width_dirty: bool,
    cached: usize,
    probe: Option<Arc<Galley>>,
    hl_generation: Option<u64>,
    hl_enabled: bool,
    hl_pending: bool,
    hl_local_pending: bool,
    hl_due: Option<f64>,
    hl_pending_since: Option<f64>,
    hl_ever: bool,
    inexact: usize,
    refine_cursor: usize,
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

fn estimate_width(line: &str, char_width: f32) -> f32 {
    let cols: usize = line
        .chars()
        .map(|c| {
            if c == '\t' {
                4
            } else if is_wide(c) {
                2
            } else {
                1
            }
        })
        .sum();
    cols as f32 * char_width
}

/// Normalize raw highlighter output: clamp to the text, sort, drop overlaps
/// (earlier spans win), snap to char boundaries and drop `Plain` spans.
pub fn normalize_spans(
    text: &str,
    raw: Vec<(Range<usize>, TokenStyle)>,
) -> Vec<(Range<usize>, TokenStyle)> {
    // Everything happens in place: highlighter output for large documents
    // has hundreds of thousands of entries.
    let len = text.len();
    let mut v = raw;
    v.retain_mut(|(r, st)| {
        let (s, e) = (r.start.min(len), r.end.min(len));
        *r = s..e;
        s < e && *st != TokenStyle::Plain
    });
    if !v.windows(2).all(|w| w[0].0.start <= w[1].0.start) {
        v.sort_by_key(|(r, _)| r.start);
    }
    let mut write = 0usize;
    let mut pos = 0usize;
    for read in 0..v.len() {
        let (r, st) = (v[read].0.clone(), v[read].1);
        let s = ceil_char_boundary(text, r.start.max(pos));
        let e = ceil_char_boundary(text, r.end);
        if s < e {
            v[write] = (s..e, st);
            write += 1;
            pos = e;
        }
    }
    v.truncate(write);
    v
}

fn spans_for_line(line_text: &str, raw: Vec<(Range<usize>, TokenStyle)>) -> Vec<Span> {
    normalize_spans(line_text, raw)
        .into_iter()
        .map(|(r, style)| Span {
            start: r.start as u32,
            end: r.end as u32,
            style,
        })
        .collect()
}

impl DocLayout {
    /// A layout with no lines; call [`DocLayout::reset`] before use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything and size the model for `index`'s lines.
    pub fn reset(&mut self, text: &str, index: &LineIndex) {
        let n = index.line_count();
        self.lines.clear();
        self.lines.resize_with(n, LineInfo::default);
        self.cached = 0;
        self.hl_pending = true;
        self.hl_local_pending = false;
        self.hl_due = None;
        self.hl_ever = false;
        self.refine_cursor = 0;
        self.estimate_all(text, index);
    }

    fn estimate_line(&mut self, text: &str, index: &LineIndex, line: usize) {
        let Some(p) = self.params.as_ref() else {
            let info = &mut self.lines[line];
            info.rows = 1;
            info.width = 0.0;
            if info.exact {
                info.exact = false;
                self.inexact += 1;
            }
            return;
        };
        let w = estimate_width(
            display_prefix(&text[index.line_range(line)]).0,
            p.char_width,
        );
        let wrap = p.effective_wrap();
        let rows = ((w / wrap).ceil() as u32).max(1);
        // Without word wrap a short line certainly fits on one row.
        let certain = p.wrap_width.is_none() && w < wrap * 0.75;
        let info = &mut self.lines[line];
        info.rows = rows;
        info.width = w;
        let was_exact = info.exact;
        info.exact = certain;
        match (was_exact, certain) {
            (true, false) => self.inexact += 1,
            (false, true) => self.inexact = self.inexact.saturating_sub(1),
            _ => {}
        }
    }

    fn estimate_all(&mut self, text: &str, index: &LineIndex) {
        self.inexact = 0;
        for l in 0..self.lines.len() {
            self.lines[l].exact = true;
            self.estimate_line(text, index, l);
        }
        self.rows_dirty_from = 0;
        self.width_dirty = true;
    }

    /// Replace lines `first .. first + old_count` by `new_count` fresh lines
    /// (after an edit). `text`/`index` describe the text after the edit.
    pub fn splice(
        &mut self,
        text: &str,
        index: &LineIndex,
        first: usize,
        old_count: usize,
        new_count: usize,
    ) {
        if self.lines.is_empty() || first + old_count > self.lines.len() {
            self.reset(text, index);
            return;
        }
        let cont = self.lines[first].cont;
        let removed: Vec<LineInfo> = self
            .lines
            .splice(
                first..first + old_count,
                (0..new_count).map(|_| LineInfo {
                    exact: true,
                    ..Default::default()
                }),
            )
            .collect();
        for r in &removed {
            if r.galley.is_some() {
                self.cached = self.cached.saturating_sub(1);
            }
            if !r.exact {
                self.inexact = self.inexact.saturating_sub(1);
            }
        }
        for l in first..first + new_count {
            self.estimate_line(text, index, l);
            let info = &mut self.lines[l];
            info.cont = cont;
            if let Some(style) = cont {
                // Inside a multi-line token: keep its style until the full pass.
                let len = index.line_range(l).len() as u32;
                info.spans = if len > 0 {
                    vec![Span {
                        start: 0,
                        end: len,
                        style,
                    }]
                } else {
                    Vec::new()
                };
                info.spans_valid = true;
            } else {
                info.spans_valid = false;
            }
        }
        self.hl_pending = true;
        self.hl_local_pending = true;
        self.rows_dirty_from = self.rows_dirty_from.min(first);
        self.width_dirty = true;
        debug_assert_eq!(self.lines.len(), index.line_count());
    }

    fn drop_all_galleys(&mut self) {
        for l in &mut self.lines {
            l.galley = None;
        }
        self.cached = 0;
    }

    fn build_formats(&mut self, theme: &EditorTheme) {
        let Some(p) = &self.params else { return };
        self.formats = TokenStyle::ALL
            .iter()
            .map(|&style| TextFormat {
                font_id: p.font_id.clone(),
                color: theme.token_color(style),
                italics: style == TokenStyle::Comment && theme.italic_comments,
                line_height: Some(p.row_height),
                valign: Align::BOTTOM,
                ..Default::default()
            })
            .collect();
    }

    fn format(&self, style: TokenStyle) -> TextFormat {
        let i = TokenStyle::ALL
            .iter()
            .position(|s| *s == style)
            .unwrap_or(TokenStyle::ALL.len() - 1);
        self.formats.get(i).cloned().unwrap_or_default()
    }

    /// Update parameters and theme; invalidates what they affect. Also
    /// detects a recreated font atlas (cached galleys would reference stale
    /// glyph positions) by probing egui's galley cache.
    pub fn set_params(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        params: LayoutParams,
        theme: &EditorTheme,
    ) {
        let probe = ctx.fonts_mut(|f| {
            f.layout_job(LayoutJob::simple_singleline(
                "dbiji_editor_probe".to_owned(),
                FontId::monospace(7.0),
                Color32::WHITE,
            ))
        });
        if self
            .probe
            .as_ref()
            .is_some_and(|old| !Arc::ptr_eq(old, &probe))
        {
            self.drop_all_galleys();
        }
        self.probe = Some(probe);

        let geometry_changed = self
            .params
            .as_ref()
            .is_none_or(|old| !old.same_geometry(&params));
        let theme_key = theme.layout_key();
        let appearance_changed =
            geometry_changed || theme_key != self.theme_key || self.formats.is_empty();
        self.params = Some(params);
        self.theme_key = theme_key;
        if appearance_changed {
            self.build_formats(theme);
            self.drop_all_galleys();
        }
        if self.lines.len() != index.line_count() {
            self.reset(text, index);
        } else if geometry_changed {
            self.estimate_all(text, index);
        }
    }

    /// Current parameters.
    #[cfg(test)]
    pub fn params(&self) -> Option<&LayoutParams> {
        self.params.as_ref()
    }

    /// Row height in points.
    pub fn row_height(&self) -> f32 {
        self.params.as_ref().map_or(16.0, |p| p.row_height)
    }

    /// Bring highlighting up to date. Returns the delay (seconds) after which
    /// a repaint is needed to run a pending debounced full pass.
    pub fn update_highlight(
        &mut self,
        text: &str,
        index: &LineIndex,
        lang: &dyn EditorLanguage,
        policy: HighlightPolicy,
        now: f64,
        edited_this_frame: bool,
    ) -> Option<f64> {
        if !policy.enabled {
            if self.hl_enabled || !self.hl_ever {
                tracing::debug!(
                    bytes = text.len(),
                    "document exceeds max_highlight_bytes: syntax highlighting disabled"
                );
                for l in &mut self.lines {
                    if !l.spans.is_empty() && l.galley.take().is_some() {
                        self.cached = self.cached.saturating_sub(1);
                    }
                    l.spans.clear();
                    l.cont = None;
                    l.spans_valid = true;
                }
                self.hl_enabled = false;
                self.hl_ever = true;
            }
            if self.hl_local_pending {
                for l in &mut self.lines {
                    if !l.spans_valid {
                        l.spans.clear();
                        l.cont = None;
                        l.spans_valid = true;
                    }
                }
                self.hl_local_pending = false;
            }
            self.hl_pending = false;
            return None;
        }
        if !self.hl_enabled {
            self.hl_enabled = true;
            self.hl_pending = true;
        }
        let generation = lang.highlight_generation();
        let gen_changed = self.hl_generation != Some(generation);
        if self.hl_pending && self.hl_pending_since.is_none() {
            self.hl_pending_since = Some(now);
        }
        let due = self.hl_due.is_some_and(|t| now >= t) && !edited_this_frame;
        // While typing continuously, still refresh multi-line constructs now
        // and then.
        let stale = self
            .hl_pending_since
            .is_some_and(|t| now - t >= MAX_HIGHLIGHT_STALENESS);
        let run_full = gen_changed
            || (self.hl_pending
                && (!self.hl_ever || text.len() <= policy.sync_limit || due || stale));
        if run_full {
            self.full_highlight(text, index, lang);
            self.hl_generation = Some(generation);
            self.hl_pending = false;
            self.hl_local_pending = false;
            self.hl_due = None;
            self.hl_pending_since = None;
            self.hl_ever = true;
            return None;
        }
        if self.hl_local_pending {
            self.local_highlight(text, index, lang);
            self.hl_local_pending = false;
        }
        if self.hl_pending {
            if edited_this_frame || self.hl_due.is_none() {
                self.hl_due = Some(now + policy.debounce);
            }
            return self.hl_due.map(|t| (t - now).max(0.0));
        }
        None
    }

    fn set_spans(&mut self, line: usize, spans: &[Span], cont: Option<TokenStyle>) {
        let info = &mut self.lines[line];
        info.cont = cont;
        info.spans_valid = true;
        if info.spans != spans {
            info.spans.clear();
            info.spans.extend_from_slice(spans);
            if info.galley.take().is_some() {
                self.cached = self.cached.saturating_sub(1);
            }
        }
    }

    fn local_highlight(&mut self, text: &str, index: &LineIndex, lang: &dyn EditorLanguage) {
        // After a huge paste only the first lines get a per-line pass; the
        // rest stay plain until the debounced full pass.
        let mut budget = LOCAL_HIGHLIGHT_LINES;
        for l in 0..self.lines.len() {
            if self.lines[l].spans_valid {
                continue;
            }
            if budget == 0 {
                let cont = self.lines[l].cont;
                self.set_spans(l, &[], cont);
                continue;
            }
            budget -= 1;
            let line_text = &text[index.line_range(l)];
            let spans = spans_for_line(line_text, lang.highlight(line_text));
            let cont = self.lines[l].cont;
            self.set_spans(l, &spans, cont);
        }
    }

    fn full_highlight(&mut self, text: &str, index: &LineIndex, lang: &dyn EditorLanguage) {
        let spans = normalize_spans(text, lang.highlight(text));
        let mut si = 0usize;
        let mut carry: Option<(TokenStyle, usize)> = None;
        let mut scratch: Vec<Span> = Vec::new();
        for l in 0..self.lines.len() {
            let ls = index.line_start(l);
            let le = index.line_end(l);
            scratch.clear();
            let cont = carry.map(|(st, _)| st);
            if let Some((st, end)) = carry {
                let e = end.min(le);
                if e > ls {
                    scratch.push(Span {
                        start: 0,
                        end: (e - ls) as u32,
                        style: st,
                    });
                }
                if end > le + 1 {
                    self.set_spans(l, &scratch, cont);
                    continue;
                }
                carry = None;
            }
            while si < spans.len() && spans[si].0.start <= le {
                let (r, st) = (spans[si].0.clone(), spans[si].1);
                si += 1;
                let s = r.start.max(ls);
                let e = r.end.min(le);
                if e > s {
                    scratch.push(Span {
                        start: (s - ls) as u32,
                        end: (e - ls) as u32,
                        style: st,
                    });
                }
                if r.end > le + 1 {
                    carry = Some((st, r.end));
                    break;
                }
            }
            self.set_spans(l, &scratch, cont);
        }
    }

    /// Spans of `line`.
    #[cfg(test)]
    pub fn spans(&self, line: usize) -> &[Span] {
        self.lines.get(line).map_or(&[], |l| l.spans.as_slice())
    }

    /// Style of the token containing byte `offset` strictly inside it.
    pub fn style_inside(&self, index: &LineIndex, offset: usize) -> Option<TokenStyle> {
        let line = index.line_of_offset(offset);
        let rel = (offset - index.line_start(line)) as u32;
        let info = self.lines.get(line)?;
        if rel == 0 {
            // At a line start we are inside a token only if it continues from above.
            return info.cont.filter(|_| {
                info.spans
                    .first()
                    .is_some_and(|s| s.start == 0 && s.end > 0)
            });
        }
        info.spans
            .iter()
            .find(|s| s.start < rel && rel < s.end)
            .map(|s| s.style)
    }

    /// Style of the token covering the char at byte `offset`.
    pub fn style_at(&self, index: &LineIndex, offset: usize) -> Option<TokenStyle> {
        let line = index.line_of_offset(offset);
        let rel = (offset - index.line_start(line)) as u32;
        self.lines
            .get(line)?
            .spans
            .iter()
            .find(|s| s.start <= rel && rel < s.end)
            .map(|s| s.style)
    }

    fn line_job(&self, line_text: &str, spans: &[Span], wrap: f32) -> LayoutJob {
        let mut job = LayoutJob {
            wrap: TextWrapping {
                max_width: wrap,
                max_rows: usize::MAX,
                break_anywhere: false,
                overflow_character: None,
            },
            keep_trailing_whitespace: true,
            ..Default::default()
        };
        job.text.reserve(line_text.len());
        let plain = self.format(TokenStyle::Plain);
        let mut pos = 0usize;
        for sp in spans {
            let (s, e) = (sp.start as usize, (sp.end as usize).min(line_text.len()));
            if s < pos || e <= s || !line_text.is_char_boundary(s) || !line_text.is_char_boundary(e)
            {
                continue;
            }
            if s > pos {
                job.append(&line_text[pos..s], 0.0, plain.clone());
            }
            job.append(&line_text[s..e], 0.0, self.format(sp.style));
            pos = e;
        }
        if pos < line_text.len() || job.sections.is_empty() {
            job.append(&line_text[pos..], 0.0, plain);
        }
        job
    }

    fn layout_now(
        &mut self,
        fonts: &mut FontsImpl,
        text: &str,
        index: &LineIndex,
        line: usize,
    ) -> Arc<Galley> {
        let (wrap, ppp) = match &self.params {
            Some(p) => (p.effective_wrap(), p.pixels_per_point),
            None => (LONG_LINE_WRAP, 1.0),
        };
        let (line_text, truncated) = display_prefix(&text[index.line_range(line)]);
        let mut job = self.line_job(line_text, &self.lines[line].spans, wrap);
        if truncated {
            job.append("…", 0.0, self.format(TokenStyle::Comment));
        }
        let galley = Arc::new(egui::epaint::text::layout(fonts, ppp, Arc::new(job)));
        let rows = galley.rows.len().max(1) as u32;
        let width = galley.rows.iter().map(|r| r.size.x).fold(0.0, f32::max);
        let info = &mut self.lines[line];
        if info.rows != rows {
            self.rows_dirty_from = self.rows_dirty_from.min(line);
        }
        if (info.width - width).abs() > 0.01 {
            self.width_dirty = true;
        }
        info.rows = rows;
        info.width = width;
        if !info.exact {
            info.exact = true;
            self.inexact = self.inexact.saturating_sub(1);
        }
        if info.galley.is_none() {
            self.cached += 1;
        }
        info.galley = Some(Arc::clone(&galley));
        galley
    }

    /// Galley of `line`, laid out on demand.
    pub fn galley(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        line: usize,
    ) -> Arc<Galley> {
        if self.lines.len() != index.line_count() {
            self.reset(text, index);
        }
        let line = line.min(self.lines.len().saturating_sub(1));
        if let Some(g) = self.lines.get(line).and_then(|l| l.galley.clone()) {
            return g;
        }
        ctx.fonts_mut(|f| self.layout_now(f.fonts, text, index, line))
    }

    /// Lay out every line in `lines` that has no galley yet (one font lock).
    pub fn ensure_galleys(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        lines: Range<usize>,
    ) {
        let end = lines.end.min(self.lines.len());
        let missing: Vec<usize> = (lines.start..end)
            .filter(|&l| self.lines[l].galley.is_none())
            .collect();
        if missing.is_empty() {
            return;
        }
        ctx.fonts_mut(|f| {
            for l in missing {
                self.layout_now(f.fonts, text, index, l);
            }
        });
    }

    /// Cached galley of `line`, if laid out.
    #[cfg(test)]
    pub fn cached_galley(&self, line: usize) -> Option<&Arc<Galley>> {
        self.lines.get(line).and_then(|l| l.galley.as_ref())
    }

    /// Measure lines whose geometry is only estimated (word-wrap mode),
    /// a batch per call. Returns `true` if more work remains.
    pub fn refine(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        keep: Range<usize>,
    ) -> bool {
        if self.inexact == 0 || self.lines.is_empty() {
            return false;
        }
        let n = self.lines.len();
        let mut todo = Vec::with_capacity(REFINE_BATCH);
        let mut scanned = 0usize;
        while todo.len() < REFINE_BATCH && scanned < n {
            let l = self.refine_cursor % n;
            if !self.lines[l].exact {
                todo.push(l);
            }
            self.refine_cursor = (self.refine_cursor + 1) % n;
            scanned += 1;
        }
        ctx.fonts_mut(|f| {
            for &l in &todo {
                self.layout_now(f.fonts, text, index, l);
                if !keep.contains(&l) && self.lines[l].galley.take().is_some() {
                    self.cached = self.cached.saturating_sub(1);
                }
            }
        });
        self.inexact > 0
    }

    /// Drop galleys of lines far away from `keep` when too many are cached.
    pub fn evict(&mut self, keep: Range<usize>) {
        if self.cached <= MAX_CACHED_GALLEYS {
            return;
        }
        let margin = MAX_CACHED_GALLEYS / 4;
        let lo = keep.start.saturating_sub(margin);
        let hi = keep.end + margin;
        for (l, info) in self.lines.iter_mut().enumerate() {
            if (l < lo || l >= hi) && info.galley.take().is_some() {
                self.cached = self.cached.saturating_sub(1);
            }
        }
    }

    /// Number of cached galleys.
    #[cfg(test)]
    pub fn cached_galleys(&self) -> usize {
        self.cached
    }

    fn ensure_rows(&mut self) {
        let n = self.lines.len();
        if self.row_starts.len() != n + 1 {
            self.row_starts.resize(n + 1, 0);
            self.rows_dirty_from = 0;
        }
        if self.rows_dirty_from > n {
            return;
        }
        let mut acc = if self.rows_dirty_from == 0 {
            0
        } else {
            self.row_starts[self.rows_dirty_from]
        };
        let from = self.rows_dirty_from;
        if from == 0 {
            self.row_starts[0] = 0;
        }
        for l in from..n {
            let info = &self.lines[l];
            acc += if info.hidden { 0 } else { info.rows.max(1) };
            self.row_starts[l + 1] = acc;
        }
        self.rows_dirty_from = n + 1;
    }

    /// Hide exactly the lines of `ranges` (collapsed folds), showing all others.
    pub fn set_hidden(&mut self, ranges: &[Range<usize>]) {
        let n = self.lines.len();
        let mut first_changed = n;
        let mut r = 0;
        for l in 0..n {
            while r < ranges.len() && ranges[r].end <= l {
                r += 1;
            }
            let hide = r < ranges.len() && ranges[r].contains(&l);
            if self.lines[l].hidden != hide {
                self.lines[l].hidden = hide;
                first_changed = first_changed.min(l);
            }
        }
        if first_changed < n {
            self.rows_dirty_from = self.rows_dirty_from.min(first_changed);
        }
    }

    /// The line is inside a collapsed fold.
    pub fn is_hidden(&self, line: usize) -> bool {
        self.lines.get(line).is_some_and(|l| l.hidden)
    }

    /// Total number of rows.
    pub fn total_rows(&mut self) -> u32 {
        self.ensure_rows();
        self.row_starts.last().copied().unwrap_or(0).max(1)
    }

    /// First global row of `line`.
    pub fn first_row(&mut self, line: usize) -> u32 {
        self.ensure_rows();
        self.row_starts
            .get(line.min(self.lines.len()))
            .copied()
            .unwrap_or(0)
    }

    /// Row count of `line`.
    pub fn line_rows(&self, line: usize) -> u32 {
        self.lines.get(line).map_or(1, |l| l.rows.max(1))
    }

    /// Top y (relative to the first row) of `line`.
    pub fn line_top(&mut self, line: usize) -> f32 {
        self.first_row(line) as f32 * self.row_height()
    }

    /// Height of all rows.
    pub fn total_height(&mut self) -> f32 {
        self.total_rows() as f32 * self.row_height()
    }

    /// Line containing global `row` (clamped).
    pub fn line_of_row(&mut self, row: u32) -> usize {
        self.ensure_rows();
        let n = self.lines.len();
        self.row_starts[..n]
            .partition_point(|&s| s <= row)
            .saturating_sub(1)
    }

    /// Line at y (relative to the first row), clamped to the document.
    pub fn line_at_y(&mut self, y: f32) -> usize {
        let rh = self.row_height();
        let row = if y <= 0.0 { 0 } else { (y / rh) as u32 };
        self.line_of_row(row)
    }

    /// Lines intersecting the vertical range `y` (relative to the first row).
    pub fn lines_in(&mut self, y: Range<f32>) -> Range<usize> {
        if self.lines.is_empty() {
            return 0..0;
        }
        let first = self.line_at_y(y.start);
        let last = self.line_at_y(y.end);
        first..(last + 1).min(self.lines.len())
    }

    /// Widest line (exact for laid out lines, estimated otherwise).
    pub fn max_width(&mut self) -> f32 {
        if self.width_dirty {
            self.max_width = self.lines.iter().map(|l| l.width).fold(0.0, f32::max);
            self.width_dirty = false;
        }
        self.max_width
    }

    /// Number of lines in the model.
    #[cfg(test)]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// `true` while estimated geometry remains (word wrap refinement).
    pub fn has_estimates(&self) -> bool {
        self.inexact > 0
    }

    /// Rectangle of the caret position at char column `col` of `line`,
    /// relative to the document origin (top of row 0, left of text).
    pub fn caret_rect(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        line: usize,
        col: usize,
        prefer_next_row: bool,
    ) -> Rect {
        let g = self.galley(ctx, text, index, line);
        let top = self.line_top(line);
        let r = g.pos_from_cursor(CCursor {
            index: CharIndex(col),
            prefer_next_row,
        });
        let rh = self.row_height();
        Rect::from_min_size(egui::pos2(r.min.x, top + r.min.y), Vec2::new(0.0, rh))
    }

    /// `(line, char column, prefer_next_row)` at document position `pos`.
    pub fn hit(
        &mut self,
        ctx: &Context,
        text: &str,
        index: &LineIndex,
        pos: Vec2,
    ) -> (usize, usize, bool) {
        let line = self.line_at_y(pos.y);
        let top = self.line_top(line);
        let g = self.galley(ctx, text, index, line);
        let max_y = ((g.rows.len().max(1) as f32) * self.row_height() - 0.5).max(0.0);
        let local = Vec2::new(pos.x, (pos.y - top).clamp(0.0, max_y));
        let c = g.cursor_from_pos(local);
        (line, c.index.0, c.prefer_next_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{CompletionList, PlainText};

    struct Words;
    impl EditorLanguage for Words {
        /// `k` words are keywords; `/* */` comments may span lines.
        fn highlight(&self, text: &str) -> Vec<(Range<usize>, TokenStyle)> {
            let mut out = Vec::new();
            let b = text.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    let end = text[i + 2..]
                        .find("*/")
                        .map_or(text.len(), |p| i + 2 + p + 2);
                    out.push((i..end, TokenStyle::Comment));
                    i = end;
                } else if b[i] == b'k' {
                    out.push((i..i + 1, TokenStyle::Keyword));
                    i += 1;
                } else {
                    i += 1;
                }
            }
            out
        }
        fn complete(&self, _: &str, _: usize) -> Option<CompletionList> {
            None
        }
    }

    fn ctx() -> Context {
        let ctx = Context::default();
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        ctx
    }

    fn params(ctx: &Context, wrap: Option<f32>) -> LayoutParams {
        let font_id = FontId::monospace(13.0);
        let (row_height, char_width) =
            ctx.fonts_mut(|f| (f.row_height(&font_id), f.glyph_width(&font_id, '0')));
        LayoutParams {
            font_id,
            row_height: (row_height + 3.0).round(),
            char_width,
            wrap_width: wrap,
            pixels_per_point: 1.0,
        }
    }

    fn setup(text: &str, wrap: Option<f32>) -> (Context, LineIndex, DocLayout) {
        let ctx = ctx();
        let index = LineIndex::new(text);
        let mut layout = DocLayout::new();
        layout.reset(text, &index);
        layout.set_params(&ctx, text, &index, params(&ctx, wrap), &EditorTheme::dark());
        (ctx, index, layout)
    }

    const POLICY: HighlightPolicy = HighlightPolicy {
        enabled: true,
        sync_limit: 1 << 20,
        debounce: 0.25,
    };

    #[test]
    #[allow(clippy::reversed_empty_ranges)] // deliberately invalid input
    fn normalize_handles_garbage() {
        let t = "aé b";
        let raw = vec![
            (5..2, TokenStyle::Keyword),
            (0..2, TokenStyle::Keyword),
            (1..9, TokenStyle::String),
            (4..5, TokenStyle::Plain),
        ];
        assert_eq!(
            normalize_spans(t, raw),
            vec![(0..3, TokenStyle::Keyword), (3..5, TokenStyle::String)]
        );
    }

    #[test]
    fn full_highlight_splits_multiline_tokens() {
        let text = "k x\n/* a\nb */ k\nk";
        let (_ctx, index, mut layout) = setup(text, None);
        layout.update_highlight(text, &index, &Words, POLICY, 0.0, false);
        assert_eq!(
            layout.spans(0),
            &[Span {
                start: 0,
                end: 1,
                style: TokenStyle::Keyword
            }]
        );
        assert_eq!(
            layout.spans(1),
            &[Span {
                start: 0,
                end: 4,
                style: TokenStyle::Comment
            }]
        );
        assert_eq!(
            layout.spans(2),
            &[
                Span {
                    start: 0,
                    end: 4,
                    style: TokenStyle::Comment
                },
                Span {
                    start: 5,
                    end: 6,
                    style: TokenStyle::Keyword
                }
            ]
        );
        assert_eq!(
            layout.spans(3),
            &[Span {
                start: 0,
                end: 1,
                style: TokenStyle::Keyword
            }]
        );
        assert_eq!(layout.style_inside(&index, 6), Some(TokenStyle::Comment));
        assert_eq!(
            layout.style_inside(&index, index.line_start(2)),
            Some(TokenStyle::Comment)
        );
        assert_eq!(layout.style_inside(&index, 4), None);
        assert_eq!(layout.style_at(&index, 0), Some(TokenStyle::Keyword));
    }

    #[test]
    fn deferred_highlight_for_large_documents() {
        let mut text = String::from("x\nx\nx");
        let (_ctx, mut index, mut layout) = setup(&text, None);
        let policy = HighlightPolicy {
            sync_limit: 0,
            ..POLICY
        };
        // The very first pass is synchronous.
        assert_eq!(
            layout.update_highlight(&text, &index, &Words, policy, 0.0, false),
            None
        );
        // Edit line 1: local highlight now, full pass after the debounce.
        index.edit(&text, 2..3, "k k");
        text.replace_range(2..3, "k k");
        layout.splice(&text, &index, 1, 1, 1);
        let wait = layout.update_highlight(&text, &index, &Words, policy, 1.0, true);
        assert_eq!(wait, Some(0.25));
        assert_eq!(layout.spans(1).len(), 2);
        let wait = layout
            .update_highlight(&text, &index, &Words, policy, 1.1, false)
            .expect("pending");
        assert!((wait - 0.15).abs() < 1e-9);
        // Continuous typing still gets a full pass after the staleness limit.
        layout.splice(&text, &index, 1, 1, 1);
        assert!(
            layout
                .update_highlight(&text, &index, &Words, policy, 2.0, true)
                .is_some()
        );
        assert_eq!(
            layout.update_highlight(&text, &index, &Words, policy, 3.1, true),
            None
        );
        layout.splice(&text, &index, 1, 1, 1);
        assert!(
            layout
                .update_highlight(&text, &index, &Words, policy, 3.2, true)
                .is_some()
        );
        assert_eq!(
            layout.update_highlight(&text, &index, &Words, policy, 3.5, false),
            None
        );
    }

    #[test]
    fn rows_and_positions() {
        let text = "a\nbb\n\nccc";
        let (ctx, index, mut layout) = setup(text, None);
        let rh = layout.row_height();
        assert_eq!(layout.total_rows(), 4);
        assert_eq!(layout.line_top(2), 2.0 * rh);
        assert_eq!(layout.line_at_y(-5.0), 0);
        assert_eq!(layout.line_at_y(rh * 1.5), 1);
        assert_eq!(layout.line_at_y(rh * 100.0), 3);
        assert_eq!(layout.lines_in(0.0..rh * 2.5), 0..3);
        let r = layout.caret_rect(&ctx, text, &index, 3, 2, true);
        assert!(r.min.x > 0.0);
        assert_eq!(r.min.y, 3.0 * rh);
        let (line, col, _) =
            layout.hit(&ctx, text, &index, Vec2::new(r.min.x + 0.1, r.min.y + 1.0));
        assert_eq!((line, col), (3, 2));
        assert!(layout.max_width() > 0.0);
    }

    #[test]
    fn uniform_row_heights_with_fallback_glyphs() {
        let text = "plain\nemoji 👍 and 中文 text\n\tTab";
        let (ctx, index, mut layout) = setup(text, None);
        let rh = layout.row_height();
        for l in 0..index.line_count() {
            let g = layout.galley(&ctx, text, &index, l);
            for row in &g.rows {
                assert_eq!(row.size.y, rh, "line {l}");
            }
        }
    }

    #[test]
    fn splice_tracks_edits() {
        let mut text = String::from("a\nb\nc");
        let (ctx, mut index, mut layout) = setup(&text, None);
        layout.ensure_galleys(&ctx, &text, &index, 0..3);
        assert_eq!(layout.cached_galleys(), 3);
        // Insert two lines inside line 1.
        let first = index.line_of_offset(2);
        let old_last = index.line_of_offset(3);
        index.edit(&text, 2..3, "x\ny\nz");
        text.replace_range(2..3, "x\ny\nz");
        let new_last = index.line_of_offset(2 + 5);
        layout.splice(
            &text,
            &index,
            first,
            old_last - first + 1,
            new_last - first + 1,
        );
        assert_eq!(layout.line_count(), 5);
        assert_eq!(layout.cached_galleys(), 2);
        assert_eq!(layout.total_rows(), 5);
        layout.update_highlight(&text, &index, &PlainText, POLICY, 0.0, true);
        let g = layout.galley(&ctx, &text, &index, 3);
        assert_eq!(g.text(), "z");
    }

    #[test]
    fn word_wrap_measures_rows() {
        let long = "word ".repeat(80);
        let text = format!("short\n{long}\nend");
        let (ctx, index, mut layout) = setup(&text, Some(200.0));
        assert!(layout.has_estimates());
        while layout.refine(&ctx, &text, &index, 0..0) {}
        assert!(!layout.has_estimates());
        assert!(layout.line_rows(1) > 3);
        assert_eq!(layout.line_rows(0), 1);
        let total = layout.total_rows();
        assert_eq!(total, 2 + layout.line_rows(1));
        assert_eq!(layout.cached_galleys(), 0, "refine keeps only geometry");
        // Hit-test a wrapped row.
        let top = layout.line_top(1);
        let rh = layout.row_height();
        let (line, col, _) = layout.hit(&ctx, &text, &index, Vec2::new(0.0, top + rh * 1.5));
        assert_eq!(line, 1);
        assert!(col > 0);
    }

    #[test]
    fn atlas_probe_and_theme_invalidate_galleys() {
        let text = "a\nb";
        let (ctx, index, mut layout) = setup(text, None);
        layout.ensure_galleys(&ctx, text, &index, 0..2);
        assert_eq!(layout.cached_galleys(), 2);
        let p = layout.params().cloned().expect("params");
        layout.set_params(&ctx, text, &index, p.clone(), &EditorTheme::dark());
        assert_eq!(layout.cached_galleys(), 2, "same params keep galleys");
        layout.set_params(&ctx, text, &index, p, &EditorTheme::light());
        assert_eq!(layout.cached_galleys(), 0, "theme change drops galleys");
    }

    #[test]
    fn very_long_lines_are_laid_out_truncated() {
        let text = format!("short\n{}", "abc ".repeat(15_000));
        let (ctx, index, mut layout) = setup(&text, None);
        let g = layout.galley(&ctx, &text, &index, 1);
        let chars: usize = g.rows.iter().map(|r| r.glyphs.len()).sum();
        assert_eq!(chars, MAX_LAYOUT_CHARS + 1, "prefix plus ellipsis");
        // Positions beyond the laid out prefix clamp to its end.
        let far = layout.caret_rect(&ctx, &text, &index, 1, 59_000, true);
        assert!(far.min.x > 0.0);
        let (line, col, _) = layout.hit(&ctx, &text, &index, Vec2::new(1.0e7, far.center().y));
        assert_eq!(line, 1);
        assert!(col <= MAX_LAYOUT_CHARS + 1);
        assert_eq!(display_prefix("short"), ("short", false));
    }

    #[test]
    fn eviction_keeps_visible() {
        let text = "x\n".repeat(MAX_CACHED_GALLEYS + 500);
        let (ctx, index, mut layout) = setup(&text, None);
        layout.ensure_galleys(&ctx, &text, &index, 0..index.line_count());
        assert!(layout.cached_galleys() > MAX_CACHED_GALLEYS);
        layout.evict(0..10);
        assert!(layout.cached_galleys() <= MAX_CACHED_GALLEYS);
        assert!(layout.cached_galley(5).is_some());
        assert!(layout.cached_galley(index.line_count() - 1).is_none());
    }
}
