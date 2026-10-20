//! Completion in the filter bar: the column names (and a few keywords) that
//! match the word being typed in the WHERE / ORDER BY box.

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{Key, Modifiers, TextEdit, Ui};
use egui_phosphor::regular as icon;
use std::ops::Range;

/// Most suggestions shown.
const MAX_ITEMS: usize = 8;

/// State of one box's suggestions.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Suggest {
    selected: usize,
    /// Shown at the last frame: its keys are taken before the text field.
    open: bool,
    /// Closed with Escape (or by a pick) until the text changes.
    dismissed: bool,
}

/// One suggestion: the text and whether it is a column (else a keyword).
pub(crate) type Item = (String, bool);

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word at `cursor` (a char index) and what can replace it: columns
/// starting with it, then columns containing it, then keywords starting
/// with it. Nothing inside a string literal, for an empty word, or when the
/// word already is the only match.
pub(crate) fn suggestions(text: &str, cursor: usize, columns: &[String], keywords: &[&str]) -> (Range<usize>, Vec<Item>) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..cursor].iter().collect::<String>().to_lowercase();
    let in_string = chars[..start].iter().filter(|c| **c == '\'').count() % 2 == 1;
    if word.is_empty() || in_string {
        return (start..end, Vec::new());
    }
    let mut items: Vec<Item> = Vec::new();
    let push = |s: &str, column: bool, items: &mut Vec<Item>| {
        if !items.iter().any(|(t, _)| t == s) {
            items.push((s.to_string(), column));
        }
    };
    for c in columns.iter().filter(|c| c.to_lowercase().starts_with(&word)) {
        push(c, true, &mut items);
    }
    for c in columns.iter().filter(|c| c.to_lowercase().contains(&word)) {
        push(c, true, &mut items);
    }
    for k in keywords.iter().filter(|k| k.to_lowercase().starts_with(&word)) {
        push(k, false, &mut items);
    }
    let whole: String = chars[start..end].iter().collect::<String>().to_lowercase();
    if items.len() == 1 && items[0].0.to_lowercase() == whole {
        items.clear();
    }
    items.truncate(MAX_ITEMS);
    (start..end, items)
}

fn byte_at(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(b, _)| b)
}

/// A single-line text field with column suggestions under it. ↑ / ↓ pick,
/// Tab or Enter take the suggestion (Enter applies the filter when none
/// is shown), Escape closes them. `quote` writes a column name as SQL.
#[allow(clippy::too_many_arguments)]
pub(crate) fn suggest_edit(
    ui: &mut Ui,
    id: egui::Id,
    text: &mut String,
    edit: impl FnOnce(TextEdit<'_>) -> TextEdit<'_>,
    columns: &[String],
    keywords: &[&str],
    quote: &dyn Fn(&str) -> String,
    s: &mut Suggest,
) -> egui::Response {
    let (mut down, mut up, mut accept, mut escape) = (false, false, false, false);
    if s.open && ui.memory(|m| m.has_focus(id)) {
        ui.input_mut(|i| {
            down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
            up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
            accept = i.consume_key(Modifiers::NONE, Key::Tab) || i.consume_key(Modifiers::NONE, Key::Enter);
            escape = i.consume_key(Modifiers::NONE, Key::Escape);
        });
    }
    let resp = ui.add(edit(TextEdit::singleline(text).id(id)));
    if resp.changed() {
        s.dismissed = false;
        s.selected = 0;
    }
    if escape {
        s.dismissed = true;
    }
    let cursor = TextEditState::load(ui.ctx(), id).and_then(|st| st.cursor.char_range()).map_or(text.chars().count(), |r| r.primary.index.into());
    let (range, items) = suggestions(text, cursor, columns, keywords);
    s.open = resp.has_focus() && !s.dismissed && !items.is_empty();
    if !s.open {
        return resp;
    }
    // Tab, the arrows and Escape stay with this field while the list is shown
    // (egui would move the focus on Tab at the start of the next frame).
    ui.memory_mut(|m| m.set_focus_lock_filter(id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
    s.selected = s.selected.min(items.len() - 1);
    if down {
        s.selected = (s.selected + 1).min(items.len() - 1);
    }
    if up {
        s.selected = s.selected.saturating_sub(1);
    }
    let mut chosen = accept.then_some(s.selected);
    egui::Area::new(id.with("suggest")).order(egui::Order::Foreground).fixed_pos(resp.rect.left_bottom() + egui::vec2(0.0, 2.0)).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(resp.rect.width().clamp(160.0, 320.0));
            for (i, (label, column)) in items.iter().enumerate() {
                let glyph = if *column { icon::COLUMNS } else { icon::TEXT_AA };
                let r = ui.selectable_label(i == s.selected, (dbiji_icons::ic(glyph), label.as_str()));
                // On press: the text field keeps the focus.
                if r.hovered() && ui.input(|i| i.pointer.primary_pressed()) {
                    chosen = Some(i);
                }
            }
        });
    });
    if let Some(i) = chosen {
        let (label, column) = &items[i];
        let insert = if *column { quote(label) } else { label.clone() };
        let (a, b) = (byte_at(text, range.start), byte_at(text, range.end));
        text.replace_range(a..b, &insert);
        let mut st = TextEditState::load(ui.ctx(), id).unwrap_or_default();
        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(range.start + insert.chars().count()))));
        st.store(ui.ctx(), id);
        resp.request_focus();
        s.open = false;
        s.dismissed = true;
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::suggestions;

    fn cols() -> Vec<String> {
        ["id", "created_at", "unsub_at", "unsubs_from", "msisdn", "cycle"].iter().map(|s| s.to_string()).collect()
    }

    fn labels(text: &str) -> Vec<String> {
        suggestions(text, text.chars().count(), &cols(), &["AND", "OR", "IS NULL"]).1.into_iter().map(|(l, _)| l).collect()
    }

    #[test]
    fn columns_then_keywords() {
        assert_eq!(labels("uns"), ["unsub_at", "unsubs_from"]);
        // starts-with first, then contains
        assert_eq!(labels("at"), ["created_at", "unsub_at"]);
        assert_eq!(labels("cycle = 'daily' an"), ["AND"]);
        assert_eq!(labels("msisdn = '1' and cr"), ["created_at"]);
        assert_eq!(labels("UNS"), ["unsub_at", "unsubs_from"], "case-insensitive");
    }

    #[test]
    fn nothing_to_suggest() {
        assert!(labels("").is_empty());
        assert!(labels("cycle = 'uns").is_empty(), "inside a string");
        assert!(labels("msisdn").is_empty(), "already the only match");
        assert!(labels("id = 1 ").is_empty(), "after a space");
    }

    #[test]
    fn replaces_the_whole_word() {
        let (range, items) = suggestions("unsub = 1", 3, &cols(), &[]);
        assert_eq!(range, 0..5);
        assert_eq!(items[0].0, "unsub_at");
    }
}
