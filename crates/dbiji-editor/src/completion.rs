//! Completion popup: triggering, incremental filtering and ranking,
//! keyboard / mouse selection and insertion.

use egui::{Align2, Color32, Context, FontFamily, FontId, Id, Rect, Sense, vec2};

use crate::language::{CompletionEntry, CompletionList, EditorLanguage};
use crate::ops::EditPlan;
use crate::options::EditorOptions;
use crate::state::EditorState;
use crate::text::{Selection, floor_char_boundary};
use crate::theme::EditorTheme;
use crate::undo::EditKind;

/// Rows visible in the popup before it scrolls.
pub(crate) const VISIBLE_ROWS: usize = 10;

#[derive(Clone, Debug)]
struct Pending {
    due: f64,
    version: u64,
    cursor: usize,
}

/// Completion popup state (part of [`EditorState`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionState {
    list: Option<CompletionList>,
    anchor: usize,
    replace_end: usize,
    filtered: Vec<usize>,
    selected: usize,
    filter: String,
    pub(crate) scroll_to_selected: bool,
    list_top: usize,
    force_scroll: bool,
    manual: bool,
    pending: Option<Pending>,
}

impl CompletionState {
    pub(crate) fn is_open(&self) -> bool {
        self.list.is_some() && !self.filtered.is_empty()
    }

    pub(crate) fn close(&mut self) {
        self.list = None;
        self.filtered.clear();
        self.selected = 0;
        self.pending = None;
    }

    pub(crate) fn request_manual(&mut self) {
        self.manual = true;
    }

    /// Keep the replace range attached to the text across edits (`map`
    /// maps an offset of the text before an edit, see `EditorState`).
    pub(crate) fn shift(&mut self, map: &dyn Fn(usize, bool) -> usize) {
        if self.list.is_none() {
            return;
        }
        self.anchor = map(self.anchor, false);
        self.replace_end = map(self.replace_end, true).max(self.anchor);
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        let cur = self.selected as isize;
        let next = if delta.abs() == 1 {
            (cur + delta).rem_euclid(n as isize)
        } else {
            (cur + delta).clamp(0, n as isize - 1)
        };
        self.selected = next as usize;
        self.scroll_to_selected = true;
    }

    pub(crate) fn selected_entry(&self) -> Option<&CompletionEntry> {
        let list = self.list.as_ref()?;
        list.items.get(*self.filtered.get(self.selected)?)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &CompletionEntry> {
        let list = self.list.as_ref();
        self.filtered
            .iter()
            .filter_map(move |&i| list.and_then(|l| l.items.get(i)))
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn len(&self) -> usize {
        self.filtered.len()
    }

    pub(crate) fn select(&mut self, i: usize) {
        if i < self.filtered.len() {
            self.selected = i;
        }
    }
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|n| it.any(|h| h == n))
}

/// Rank `items` against the typed `filter` (case-insensitive): exact-case
/// prefix, prefix, word-part prefix (after `_`, `.` or a space), then
/// subsequence matches. Keeps the provider's order within each class.
pub(crate) fn rank(items: &[CompletionEntry], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    let f = filter.to_lowercase();
    let mut scored: Vec<(u8, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let label = it.label.as_str();
            if label.starts_with(filter) {
                return Some((0, i));
            }
            let lower = label.to_lowercase();
            if lower.starts_with(&f) {
                return Some((1, i));
            }
            let word_part = lower
                .match_indices(['_', '.', ' '])
                .any(|(p, s)| lower[p + s.len()..].starts_with(&f));
            if word_part {
                return Some((2, i));
            }
            is_subsequence(&f, &lower).then_some((3, i))
        })
        .collect();
    scored.sort_by_key(|&(s, i)| (s, i));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Byte offset `chars` characters before the end of `s`.
fn offset_back(s: &str, chars: usize) -> usize {
    if chars == 0 {
        return s.len();
    }
    s.char_indices().rev().nth(chars - 1).map_or(0, |(i, _)| i)
}

/// Run the provider and open the popup if it returns anything matching.
fn query(st: &mut EditorState, lang: &dyn EditorLanguage) {
    let cursor = st.sel.cursor;
    let Some(list) = lang.complete(&st.text, cursor) else {
        st.completion.close();
        return;
    };
    if list.items.is_empty() {
        st.completion.close();
        return;
    }
    let anchor = floor_char_boundary(&st.text, list.replace.start.min(cursor));
    let replace_end = floor_char_boundary(&st.text, list.replace.end.clamp(cursor, st.text.len()));
    let c = &mut st.completion;
    c.list = Some(list);
    c.anchor = anchor;
    c.replace_end = replace_end;
    c.filter = st.text[anchor..cursor].to_owned();
    c.filtered = c
        .list
        .as_ref()
        .map(|l| rank(&l.items, &c.filter))
        .unwrap_or_default();
    c.selected = 0;
    c.list_top = 0;
    c.scroll_to_selected = true;
    if c.filtered.is_empty() {
        c.close();
    }
}

/// Per-frame completion logic, after keyboard input was processed.
/// `typed` holds the chars typed this frame (in order).
pub(crate) fn update(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    typed: &[char],
) {
    if options.read_only || st.preedit.is_some() {
        st.completion.close();
        st.completion.manual = false;
        return;
    }
    let now = st.now;
    if std::mem::take(&mut st.completion.manual) {
        query(st, lang);
        return;
    }
    let is_word = |c: char| lang.is_word_char(c);
    if let Some(&last) = typed.last() {
        if st.completion.is_open() && !is_word(last) {
            st.completion.close();
        }
        if !st.completion.is_open() && options.auto_complete {
            if lang.should_auto_complete(&st.text, st.sel.cursor, last) {
                query(st, lang);
                return;
            }
            if options.auto_complete_on_letters && is_word(last) {
                let start = crate::text::word_start_before(&st.text, st.sel.cursor, is_word);
                let prefix_chars = st.text[start..st.sel.cursor].chars().count();
                if prefix_chars >= options.auto_complete_min_chars.max(1) {
                    let delay = options.auto_complete_delay_ms as f64 / 1000.0;
                    st.completion.pending = Some(Pending {
                        due: now + delay,
                        version: st.version(),
                        cursor: st.sel.cursor,
                    });
                }
            }
        }
    }
    if let Some(p) = st.completion.pending.clone() {
        if p.version != st.version() || p.cursor != st.sel.cursor || !st.sel.is_empty() {
            st.completion.pending = None;
        } else if now >= p.due {
            st.completion.pending = None;
            query(st, lang);
            return;
        } else {
            ctx.request_repaint_after_secs((p.due - now) as f32);
        }
    }
    if st.completion.is_open() {
        let cursor = st.sel.cursor;
        let anchor = st.completion.anchor;
        let valid = st.sel.is_empty()
            && cursor >= anchor
            && cursor <= st.text.len()
            && st.text.is_char_boundary(anchor)
            && st.text[anchor..cursor].chars().all(is_word);
        if !valid {
            st.completion.close();
            return;
        }
        let filter = &st.text[anchor..cursor];
        if filter != st.completion.filter {
            let c = &mut st.completion;
            c.filter = filter.to_owned();
            let prev = c.selected_entry().map(|e| e.label.clone());
            c.filtered = c
                .list
                .as_ref()
                .map(|l| rank(&l.items, &c.filter))
                .unwrap_or_default();
            c.selected = prev
                .and_then(|label| {
                    c.filtered
                        .iter()
                        .position(|&i| c.list.as_ref().is_some_and(|l| l.items[i].label == label))
                })
                .unwrap_or(0);
            c.scroll_to_selected = true;
            if c.filtered.is_empty() {
                c.close();
            }
        }
    }
}

/// Insert the selected entry; returns it.
pub(crate) fn accept(st: &mut EditorState) -> Option<CompletionEntry> {
    let entry = st.completion.selected_entry()?.clone();
    let cursor = st.sel.cursor;
    let start = st.completion.anchor.min(cursor);
    let end = st.completion.replace_end.max(cursor).min(st.text.len());
    let caret = start + offset_back(&entry.insert, entry.cursor_back);
    let plan = EditPlan::single(start..end, entry.insert.clone(), Selection::caret(caret));
    st.completion.close();
    st.apply_plan(plan, EditKind::Other);
    st.undo.close_group();
    st.pending_scroll = Some(crate::state::ScrollRequest::Cursor);
    Some(entry)
}

/// Show the popup below (or above, if it does not fit) `caret` (screen
/// coordinates). Returns the filtered index of a clicked row and whether the
/// pointer interacted with the popup this frame.
///
/// Uses a plain [`egui::Area`] without fade-in or sizing pass so the list
/// appears in the same frame as the keystroke that opened it, and only the
/// visible rows are built.
pub(crate) fn show_popup(
    ctx: &Context,
    popup_id: Id,
    caret: Rect,
    st: &mut EditorState,
    theme: &EditorTheme,
    font_size: f32,
) -> (Option<usize>, bool) {
    let mut clicked = None;
    let row_h = (font_size + 8.0).round();
    let width = 380.0;
    let margin = 4.0;
    let text_font = FontId::new(font_size, FontFamily::Monospace);
    let detail_font = FontId::new((font_size - 1.0).max(8.0), FontFamily::Proportional);
    let icon_font = FontId::new(font_size + 1.0, FontFamily::Proportional);
    let n = st.completion.len();
    let visible = VISIBLE_ROWS.min(n.max(1));
    let height = visible as f32 * row_h + 2.0 * margin;
    let screen = ctx.content_rect();
    let below = caret.bottom() + 2.0;
    let y = if below + height <= screen.bottom() || caret.top() - 2.0 - height < screen.top() {
        below
    } else {
        caret.top() - 2.0 - height
    };
    let x = caret
        .left()
        .min(screen.right() - width - 2.0 * margin)
        .max(screen.left());

    // Keep the selected row inside the visible window.
    let c = &mut st.completion;
    if std::mem::take(&mut c.scroll_to_selected) {
        if c.selected < c.list_top {
            c.list_top = c.selected;
        } else if c.selected >= c.list_top + visible {
            c.list_top = c.selected + 1 - visible;
        }
        c.list_top = c.list_top.min(n.saturating_sub(visible));
        c.force_scroll = true;
    }
    let selected = c.selected;
    let force = std::mem::take(&mut c.force_scroll);
    let list_top = c.list_top;
    let entries: Vec<&CompletionEntry> = c.entries().collect();
    let mut new_top = list_top;
    let response = egui::Area::new(popup_id)
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(x, y))
        .fade_in(false)
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(margin)
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    let mut area = egui::ScrollArea::vertical()
                        .id_salt(popup_id.with("list"))
                        .max_height(visible as f32 * row_h)
                        .auto_shrink([false, true]);
                    if force {
                        area = area.vertical_scroll_offset(list_top as f32 * row_h);
                    }
                    let out = area.show_rows(ui, row_h, entries.len(), |ui, rows| {
                        for i in rows {
                            let Some(entry) = entries.get(i) else {
                                continue;
                            };
                            let (rect, resp) =
                                ui.allocate_exact_size(vec2(width, row_h), Sense::CLICK);
                            if i == selected {
                                ui.painter().rect_filled(rect, 3.0, theme.selection);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    3.0,
                                    ui.visuals().widgets.hovered.weak_bg_fill,
                                );
                            }
                            let painter = ui.painter_at(rect);
                            painter.text(
                                rect.left_center() + vec2(8.0, 0.0),
                                Align2::LEFT_CENTER,
                                entry.kind.icon(),
                                icon_font.clone(),
                                kind_color(theme, entry.kind),
                            );
                            let label_galley = painter.layout_no_wrap(
                                entry.label.clone(),
                                text_font.clone(),
                                theme.text,
                            );
                            let label_w = label_galley.size().x;
                            painter.galley(
                                rect.left_center() + vec2(30.0, -label_galley.size().y / 2.0),
                                label_galley,
                                theme.text,
                            );
                            if let Some(detail) = &entry.detail {
                                let avail = width - 30.0 - label_w - 24.0;
                                if avail > 30.0 {
                                    let weak = ui.visuals().weak_text_color();
                                    let job = egui::text::LayoutJob {
                                        wrap: egui::text::TextWrapping::truncate_at_width(avail),
                                        ..egui::text::LayoutJob::simple_singleline(
                                            detail.clone(),
                                            detail_font.clone(),
                                            weak,
                                        )
                                    };
                                    let g = painter.layout_job(job);
                                    let pos = rect.right_center()
                                        - vec2(10.0 + g.size().x, g.size().y / 2.0);
                                    painter.galley(pos, g, weak);
                                }
                            }
                            if resp.clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
                    new_top = (out.state.offset.y / row_h).round().max(0.0) as usize;
                });
        });
    st.completion.list_top = new_top;
    let interacted = response.response.contains_pointer()
        && ctx.input(|i| i.pointer.any_pressed() || i.pointer.any_click());
    (clicked, interacted)
}

fn kind_color(theme: &EditorTheme, kind: crate::language::EntryKind) -> Color32 {
    use crate::language::EntryKind as K;
    match kind {
        K::Keyword => theme.keyword,
        K::Table | K::View => theme.data_type,
        K::Column => theme.quoted_identifier,
        K::Function | K::Procedure => theme.function,
        K::Schema | K::Database => theme.number,
        K::DataType => theme.data_type,
        K::Alias | K::Variable => theme.parameter,
        K::Snippet => theme.client_command,
        K::Sequence | K::Index | K::Trigger | K::Other => theme.comment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::EntryKind;
    use std::ops::Range;

    fn items(labels: &[&str]) -> Vec<CompletionEntry> {
        labels
            .iter()
            .map(|l| CompletionEntry::new(*l, EntryKind::Table))
            .collect()
    }

    #[test]
    fn ranking_classes() {
        let it = items(&[
            "orders",
            "Order_items",
            "customer_order",
            "ORDERS_ARCHIVE",
            "ledger",
            "o_r_d",
        ]);
        assert_eq!(rank(&it, ""), vec![0, 1, 2, 3, 4, 5]);
        // exact-case prefix, case-insensitive prefix, word part, subsequence
        assert_eq!(rank(&it, "Ord"), vec![1, 0, 3, 2, 5]);
        assert_eq!(rank(&it, "ord"), vec![0, 1, 3, 2, 5]);
        assert_eq!(rank(&it, "zz"), Vec::<usize>::new());
        assert_eq!(rank(&it, "led"), vec![4]);
    }

    #[test]
    fn offset_back_counts_chars() {
        assert_eq!(offset_back("count()", 1), 6);
        assert_eq!(offset_back("f(é)", 1), 4);
        assert_eq!(offset_back("ab", 0), 2);
        assert_eq!(offset_back("ab", 9), 0);
    }

    #[test]
    fn selection_wraps_and_pages_clamp() {
        let mut c = CompletionState {
            filtered: vec![0, 1, 2],
            list: Some(CompletionList {
                replace: 0..0,
                items: items(&["a", "b", "c"]),
            }),
            ..Default::default()
        };
        c.move_selection(-1);
        assert_eq!(c.selected(), 2);
        c.move_selection(1);
        assert_eq!(c.selected(), 0);
        c.move_selection(10);
        assert_eq!(c.selected(), 2);
        c.move_selection(-10);
        assert_eq!(c.selected(), 0);
        assert_eq!(c.selected_entry().map(|e| e.label.as_str()), Some("a"));
    }

    #[test]
    fn replace_range_follows_typing() {
        fn through(range: Range<usize>, ins: usize) -> impl Fn(usize, bool) -> usize {
            move |o, right| {
                if o < range.start || (o == range.start && !(range.is_empty() && right)) {
                    o
                } else if o > range.end || (o == range.end && !range.is_empty()) {
                    o + ins - range.len()
                } else if range.is_empty() {
                    o + ins
                } else {
                    range.start + if right { ins } else { 0 }
                }
            }
        }
        let mut c = CompletionState {
            list: Some(CompletionList::default()),
            anchor: 5,
            replace_end: 7,
            ..Default::default()
        };
        c.shift(&through(7..7, 1));
        assert_eq!((c.anchor, c.replace_end), (5, 8));
        c.shift(&through(5..5, 2));
        assert_eq!((c.anchor, c.replace_end), (5, 10));
        c.shift(&through(0..1, 0));
        assert_eq!((c.anchor, c.replace_end), (4, 9));
    }
}
