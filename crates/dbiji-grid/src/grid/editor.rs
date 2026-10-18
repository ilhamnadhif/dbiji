//! Inline cell editor and the dictionary / enum dropdown.

use super::frame::{GridFrame, PAD};
use dbiji_core::{tr, trf};
use egui::text::{CCursor, CCursorRange};
use egui::{Align, EventFilter, Frame, Key, Layout, Margin, Popup, PopupCloseBehavior, Rect, ScrollArea, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2};

impl GridFrame<'_> {
    /// Show the inline text editor when a cell is being edited.
    pub fn show_editor(&mut self, ui: &mut Ui) {
        let Some(session) = self.state.rt.editing.as_ref() else { return };
        let (row, col, has_error) = (session.row, session.col, session.error.is_some());
        let (Some(pos), true) = (self.pos_of(col), row < self.geo.nrows) else {
            self.state.rt.editing = None;
            return;
        };
        let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
        let cell = self.geo.cell_rect(row, pos, sx, sy);
        let rect = cell.intersect(self.geo.body);
        if rect.height() < 4.0 || rect.width() < 4.0 {
            // Scrolled out of view: commit.
            self.commit_edit(false, 0, 0);
            return;
        }
        let theme = self.theme;
        let editor_id = self.id.with("cell_editor");
        let painter = ui.painter_at(self.geo.body);
        painter.rect_filled(rect, 0.0, theme.bg);
        let numeric = self.model.column_kind(col).is_numeric();
        let font = self.font.clone();
        let mut text = match self.state.rt.editing.as_mut() {
            Some(s) => std::mem::take(&mut s.text),
            None => return,
        };
        let output = {
            let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)).id_salt("cell_editor_ui"));
            child.set_clip_rect(rect);
            TextEdit::singleline(&mut text)
                .id(editor_id)
                .font(font)
                .frame(Frame::NONE)
                .margin(Margin::symmetric(PAD as i8, 0))
                .desired_width(rect.width())
                .min_size(vec2(rect.width(), rect.height()))
                .horizontal_align(if numeric { Align::Max } else { Align::Min })
                .vertical_align(Align::Center)
                .event_filter(EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true })
                .return_key(None)
                .show(&mut child)
        };
        let border = if has_error { theme.error } else { theme.active_border };
        painter.rect_stroke(rect, 0.0, Stroke::new(2.0, border), StrokeKind::Inside);
        let Some(session) = self.state.rt.editing.as_mut() else { return };
        session.text = text;
        if session.just_started {
            session.just_started = false;
            output.response.request_focus();
            let mut st = output.state.clone();
            let n = session.text.chars().count();
            let range = if session.typed { CCursorRange::one(CCursor::new(n)) } else { CCursorRange::two(CCursor::new(0), CCursor::new(n)) };
            st.cursor.set_char_range(Some(range));
            st.store(ui.ctx(), editor_id);
            ui.ctx().request_repaint();
        }
        if output.response.changed() {
            session.error = None;
        }
        let (enter, tab, shift, esc) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Tab), i.modifiers.shift, i.key_pressed(Key::Escape)));
        let focused = output.response.has_focus();
        if esc && (focused || session.error.is_some()) {
            self.state.rt.editing = None;
            ui.memory_mut(|m| m.request_focus(self.id));
            return;
        }
        if focused && enter {
            let down = if shift { -1 } else { 0 };
            self.commit_edit(true, down, 0);
            return;
        }
        if focused && tab {
            self.commit_edit(true, 0, if shift { -1 } else { 1 });
            return;
        }
        if output.response.lost_focus() {
            self.commit_edit(false, 0, 0);
            return;
        }
        if let Some(err) = self.state.rt.editing.as_ref().and_then(|s| s.error.clone()) {
            let tip = Rect::from_min_size(pos2(rect.left(), rect.bottom() + 2.0), vec2(rect.width().max(240.0), 10.0));
            egui::Area::new(self.id.with("edit_error")).order(egui::Order::Tooltip).fixed_pos(tip.min).show(ui.ctx(), |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(320.0);
                    ui.colored_label(theme.error, err);
                    ui.weak(tr("Enter to retry, Esc to cancel"));
                });
            });
        }
    }

    /// Dropdown with search for dictionary / enum columns.
    pub fn show_dropdown(&mut self, ui: &mut Ui) {
        let Some(mut dd) = self.state.rt.dropdown.clone() else { return };
        let Some(pos) = self.pos_of(dd.col) else {
            self.state.rt.dropdown = None;
            return;
        };
        if dd.row >= self.geo.nrows {
            self.state.rt.dropdown = None;
            return;
        }
        let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
        let cell = self.geo.cell_rect(dd.row, pos, sx, sy);
        let entries: Vec<(String, Option<String>)> = match (self.model.dictionary(dd.col), self.model.enum_values(dd.col)) {
            (Some(d), _) => d.entries().iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect(),
            (None, Some(e)) => e.iter().map(|v| (v.clone(), None)).collect(),
            (None, None) => Vec::new(),
        };
        let current = self.model.cell(dd.row, dd.col).map(|v| v.to_text().into_owned()).unwrap_or_default();
        let nullable = self.model.column_meta(dd.col).is_none_or(|m| m.nullable != Some(false));
        let mut chosen: Option<Option<String>> = None;
        let search_id = self.id.with("dropdown_search");
        let needle = dd.search.to_lowercase();
        let filtered: Vec<&(String, Option<String>)> = entries
            .iter()
            .filter(|(k, d)| needle.is_empty() || k.to_lowercase().contains(&needle) || d.as_ref().is_some_and(|d| d.to_lowercase().contains(&needle)))
            .take(1000)
            .collect();
        let just_opened = dd.just_opened;
        let resp = Popup::new(self.id.with("dropdown"), ui.ctx().clone(), cell, ui.layer_id())
            .open(true)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .width(cell.width().max(240.0))
            .show(|ui| {
                let te = ui.add(TextEdit::singleline(&mut dd.search).id(search_id).hint_text(tr("Search…")).return_key(None).desired_width(f32::INFINITY));
                if just_opened {
                    te.request_focus();
                }
                // The Enter that opened the dropdown must not pick an entry.
                if !just_opened
                    && te.has_focus()
                    && ui.input(|i| i.key_pressed(Key::Enter))
                    && let Some((k, _)) = filtered.first()
                {
                    chosen = Some(Some(k.clone()));
                }
                ui.separator();
                ScrollArea::vertical().max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                    if nullable && ui.selectable_label(false, egui::RichText::new(tr("[NULL]")).weak()).clicked() {
                        chosen = Some(None);
                    }
                    for (k, d) in &filtered {
                        let label = match d {
                            Some(d) => format!("{k} — {d}"),
                            None => k.clone(),
                        };
                        if ui.selectable_label(*k == current, label).clicked() {
                            chosen = Some(Some(k.clone()));
                        }
                    }
                    if filtered.is_empty() {
                        ui.weak(tr("No matching values"));
                    }
                });
                if entries.len() > filtered.len() && !needle.is_empty() {
                    ui.weak(trf("{} of {} values", &[&filtered.len(), &entries.len()]));
                }
            });
        dd.just_opened = false;
        let close_requested = resp.as_ref().is_none_or(|r| r.response.should_close()) || ui.input(|i| i.key_pressed(Key::Escape));
        if let Some(choice) = chosen {
            let result = match choice {
                Some(k) => self.model.set_cell(dd.row, dd.col, &k),
                None => self.model.set_null(dd.row, dd.col),
            };
            if let Err(e) = result {
                self.notify(e, true);
            }
            self.state.rt.dropdown = None;
            ui.memory_mut(|m| m.request_focus(self.id));
        } else if close_requested {
            self.state.rt.dropdown = None;
            ui.memory_mut(|m| m.request_focus(self.id));
        } else {
            self.state.rt.dropdown = Some(dd);
        }
    }
}
