//! Mouse and keyboard handling.

use super::frame::GridFrame;
use super::selection::CellPos;
use super::state::{Drag, EditSession, MIN_COLUMN_WIDTH};
use super::{GridAction, apply_sort};
use crate::copy::{CopyFormat, CopyOptions};
use crate::view::SortKey;
use dbiji_core::{DataKind, Value, tr, trf};
use egui::{CursorIcon, Event, EventFilter, Key, Response, Sense, Ui, vec2};

/// Width of the column resize grab zone (each side of the border).
const RESIZE_GRAB: f32 = 4.0;
/// Width of the sort indicator zone at the right of a header cell.
pub(super) const SORT_ZONE: f32 = 16.0;
/// Width of the column menu button (▾) at the right of a header.
pub(super) const MENU_ZONE: f32 = 16.0;

impl GridFrame<'_> {
    /// Handle all input. Returns the body, header and row number responses.
    pub fn interact(&mut self, ui: &mut Ui) -> (Response, Response, Response) {
        let id = self.id;
        let body = ui.interact(self.geo.body, id, Sense::click_and_drag());
        let header = ui.interact(self.geo.header, id.with("header"), Sense::click_and_drag());
        self.header_menu_buttons(ui);
        let rownums = ui.interact(self.geo.rownums, id.with("rownums"), Sense::click_and_drag());
        let corner = ui.interact(self.geo.corner, id.with("corner"), Sense::click());
        if std::mem::take(&mut self.state.rt.focus_request) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        self.focused = body.has_focus() || ui.memory(|m| m.has_focus(id));
        if self.focused {
            ui.memory_mut(|m| m.set_focus_lock_filter(id, EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
        }
        let any_hover = ui.rect_contains_pointer(self.geo.rect);
        self.hover_cell = None;
        if body.hovered()
            && let Some(p) = body.hover_pos()
        {
            let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
            if let (Some(row), Some(col)) = (self.geo.row_at(p.y, sy, false), self.geo.col_at(p.x, sx, false)) {
                self.hover_cell = Some(CellPos::new(row, col));
            }
        }
        if any_hover {
            self.wheel(ui);
        }
        self.scrollbars(ui);
        self.header_input(ui, &header);
        self.rownum_input(ui, &rownums);
        self.body_input(ui, &body);
        if corner.clicked() {
            self.state.rt.selection.select_all();
            ui.memory_mut(|m| m.request_focus(id));
        }
        if corner.hovered() {
            let tip = match self.model.read_only_reason() {
                Some(r) => r.message(),
                None => match self.model.editability() {
                    Ok(t) => trf("Editable: {}", &[&t.table.display_path()]),
                    Err(_) => String::new(),
                },
            };
            let tip = if tip.is_empty() { tr("Select all").to_string() } else { format!("{tip}\n{}", tr("Click to select all")) };
            corner.on_hover_text(tip);
        }
        if self.focused && self.state.rt.editing.is_none() && self.state.rt.dropdown.is_none() {
            self.keyboard(ui);
        }
        (body, header, rownums)
    }

    fn wheel(&mut self, ui: &mut Ui) {
        let delta = ui.input(|i| i.smooth_scroll_delta);
        if delta != egui::Vec2::ZERO {
            let before = (self.state.rt.scroll_x, self.state.rt.scroll_y);
            self.scroll_by(delta);
            if before != (self.state.rt.scroll_x, self.state.rt.scroll_y) {
                // Consume so parent scroll areas do not scroll too.
                ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
                if self.state.rt.editing.is_some() {
                    self.commit_edit(false, 0, 0);
                }
            }
        }
    }

    fn scrollbars(&mut self, ui: &mut Ui) {
        let id = self.id;
        if let Some(bar) = self.geo.vbar {
            let resp = ui.interact(bar, id.with("vbar"), Sense::click_and_drag());
            let (thumb, track) = self.v_thumb(bar);
            if (resp.drag_started() || resp.is_pointer_button_down_on() && matches!(self.state.rt.drag, Drag::None))
                && let Some(p) = resp.interact_pointer_pos()
            {
                if thumb.contains(p) {
                    self.state.rt.drag = Drag::VThumb { grab: p.y - thumb.top() };
                } else if resp.drag_started() || ui.input(|i| i.pointer.primary_pressed()) {
                    // Page towards the pointer.
                    let page = self.geo.body.height() as f64;
                    let dir = if p.y < thumb.top() { -1.0 } else { 1.0 };
                    self.state.rt.scroll_y = (self.state.rt.scroll_y + dir * page).clamp(0.0, self.geo.max_scroll_y());
                }
            }
            if let (Drag::VThumb { grab }, Some(p)) = (self.state.rt.drag, resp.interact_pointer_pos()) {
                let free = (track.height() - thumb.height()).max(1.0);
                let t = ((p.y - grab - track.top()) / free).clamp(0.0, 1.0) as f64;
                self.state.rt.scroll_y = t * self.geo.max_scroll_y();
            }
            if resp.hovered() || matches!(self.state.rt.drag, Drag::VThumb { .. }) {
                ui.ctx().set_cursor_icon(CursorIcon::Default);
            }
        }
        if let Some(bar) = self.geo.hbar {
            let resp = ui.interact(bar, id.with("hbar"), Sense::click_and_drag());
            let (thumb, track) = self.h_thumb(bar);
            if resp.is_pointer_button_down_on()
                && matches!(self.state.rt.drag, Drag::None)
                && let Some(p) = resp.interact_pointer_pos()
            {
                if thumb.contains(p) {
                    self.state.rt.drag = Drag::HThumb { grab: p.x - thumb.left() };
                } else if ui.input(|i| i.pointer.primary_pressed()) {
                    let page = self.geo.body.width();
                    let dir = if p.x < thumb.left() { -1.0 } else { 1.0 };
                    self.state.rt.scroll_x = (self.state.rt.scroll_x + dir * page).clamp(0.0, self.geo.max_scroll_x());
                }
            }
            if let (Drag::HThumb { grab }, Some(p)) = (self.state.rt.drag, resp.interact_pointer_pos()) {
                let free = (track.width() - thumb.width()).max(1.0);
                let t = ((p.x - grab - track.left()) / free).clamp(0.0, 1.0);
                self.state.rt.scroll_x = t * self.geo.max_scroll_x();
            }
        }
        if !ui.input(|i| i.pointer.primary_down()) && matches!(self.state.rt.drag, Drag::VThumb { .. } | Drag::HThumb { .. }) {
            self.state.rt.drag = Drag::None;
        }
    }

    /// Thumb and track rectangles of the vertical scrollbar.
    pub fn v_thumb(&self, bar: egui::Rect) -> (egui::Rect, egui::Rect) {
        let track = bar.shrink2(vec2(2.0, 2.0));
        let content = self.geo.content_h().max(1.0);
        let frac = (self.geo.body.height() as f64 / content).clamp(0.0, 1.0) as f32;
        let h = (track.height() * frac).max(24.0).min(track.height());
        let max = self.geo.max_scroll_y();
        let t = if max > 0.0 { (self.state.rt.scroll_y / max) as f32 } else { 0.0 };
        let top = track.top() + (track.height() - h) * t;
        (egui::Rect::from_min_size(egui::pos2(track.left(), top), vec2(track.width(), h)), track)
    }

    /// Thumb and track rectangles of the horizontal scrollbar.
    pub fn h_thumb(&self, bar: egui::Rect) -> (egui::Rect, egui::Rect) {
        let track = bar.shrink2(vec2(2.0, 2.0));
        let content = self.geo.content_w().max(1.0);
        let frac = (self.geo.body.width() / content).clamp(0.0, 1.0);
        let w = (track.width() * frac).max(24.0).min(track.width());
        let max = self.geo.max_scroll_x();
        let t = if max > 0.0 { self.state.rt.scroll_x / max } else { 0.0 };
        let left = track.left() + (track.width() - w) * t;
        (egui::Rect::from_min_size(egui::pos2(left, track.top()), vec2(w, track.height())), track)
    }

    /// The ▾ button of every visible header (left of the resize border):
    /// opens the column menu.
    fn header_menu_buttons(&mut self, ui: &mut Ui) {
        let sx = self.state.rt.scroll_x;
        let range = self.geo.visible_cols(sx);
        let (top, bottom) = (self.geo.header.top(), self.geo.header.bottom());
        for pos in range.start..range.end.min(self.geo.cols.len()) {
            let col = self.geo.cols[pos];
            let (left, right) = (self.geo.col_left(pos, sx), self.geo.col_left(pos + 1, sx));
            if right - left <= MENU_ZONE + 8.0 {
                continue;
            }
            let zone = egui::Rect::from_min_max(egui::pos2(right - MENU_ZONE, top), egui::pos2(right - RESIZE_GRAB - 1.0, bottom)).intersect(self.geo.header);
            if zone.width() <= 0.0 {
                continue;
            }
            let resp = ui.interact(zone, self.id.with(("column_menu", col)), Sense::click());
            let name = self.model.column_name(col).to_string();
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, dbiji_core::trf("Column menu: {}", &[&name])));
            if resp.clicked() {
                let anchor = egui::Rect::from_min_max(egui::pos2(right - MENU_ZONE, top), egui::pos2(right, bottom));
                self.state.rt.header_popup = Some((col, anchor, true));
            }
        }
    }

    fn header_input(&mut self, ui: &mut Ui, header: &Response) {
        let sx = self.state.rt.scroll_x;
        let hover = header.hover_pos().or_else(|| header.interact_pointer_pos());
        // Resize zone near column borders.
        let resize_target = |frame: &Self, x: f32| -> Option<usize> {
            let range = frame.geo.visible_cols(sx);
            for pos in range.start..range.end.min(frame.geo.cols.len()) {
                let right = frame.geo.col_left(pos + 1, sx);
                if (x - right).abs() <= RESIZE_GRAB {
                    return Some(pos);
                }
            }
            None
        };
        if let Some(p) = hover
            && matches!(self.state.rt.drag, Drag::None)
            && resize_target(self, p.x).is_some()
        {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        let pressed = ui.input(|i| i.pointer.primary_pressed());
        let mods = ui.input(|i| i.modifiers);
        if pressed
            && header.is_pointer_button_down_on()
            && let Some(p) = header.interact_pointer_pos()
        {
            ui.memory_mut(|m| m.request_focus(self.id));
            if let Some(pos) = resize_target(self, p.x) {
                let col = self.geo.cols[pos];
                self.state.rt.drag = Drag::Resize { col, start_width: self.state.column_width(col), start_x: p.x };
            } else if let Some(pos) = self.geo.col_at(p.x, sx, false) {
                let right = self.geo.col_left(pos + 1, sx);
                if p.x < right - SORT_ZONE - MENU_ZONE {
                    let sel = &mut self.state.rt.selection;
                    if mods.shift {
                        let anchor = sel.anchor().col;
                        sel.select_columns(anchor, pos, false);
                    } else {
                        sel.select_columns(pos, pos, mods.command);
                    }
                    self.state.rt.drag = Drag::Move { from: pos, start_x: p.x, active: false };
                }
            }
        }
        // Drag updates.
        match self.state.rt.drag {
            Drag::Resize { col, start_width, start_x } => {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                    self.state.set_column_width(col, (start_width + p.x - start_x).max(MIN_COLUMN_WIDTH));
                }
            }
            Drag::Move { from, start_x, active } => {
                if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                    if !active && (p.x - start_x).abs() > 6.0 {
                        self.state.rt.drag = Drag::Move { from, start_x, active: true };
                    }
                    if active {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                        self.autoscroll_x(p.x);
                    }
                }
            }
            _ => {}
        }
        if ui.input(|i| i.pointer.primary_released()) {
            match self.state.rt.drag {
                Drag::Move { from, active: true, .. } => {
                    if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                        let to = self.drop_position(p.x);
                        let to = if to > from { to - 1 } else { to };
                        if to != from {
                            self.state.move_visible_column(from, to);
                            self.state.rt.selection.select_columns(to, to, false);
                        }
                    }
                    self.state.rt.drag = Drag::None;
                }
                Drag::Move { .. } | Drag::Resize { .. } => self.state.rt.drag = Drag::None,
                _ => {}
            }
        }
        // Double click on a border: auto size.
        if header.double_clicked()
            && let Some(p) = header.interact_pointer_pos()
            && let Some(pos) = resize_target(self, p.x)
        {
            let col = self.geo.cols[pos];
            self.autosize_column(ui, col);
        }
        // A click on a header (not a drag, not on a resize border) cycles the
        // sort of that column: ascending → descending → unsorted. Shift / ⌘
        // add the column to the existing sort. (The ▾ buttons on top of the
        // header open the column menu.)
        if header.clicked()
            && let Some(p) = header.interact_pointer_pos()
            && let Some(pos) = self.geo.col_at(p.x, sx, false)
            && resize_target(self, p.x).is_none()
        {
            let col = self.geo.cols[pos];
            let keys = next_sort(&self.state.sort, col, mods.shift || mods.command);
            apply_sort(self.model, self.state, self.options, keys, &self.ctx, &mut self.actions);
        }
    }

    /// Display position where a dragged column would be inserted (0..=n).
    pub fn drop_position(&self, x: f32) -> usize {
        let sx = self.state.rt.scroll_x;
        let n = self.geo.cols.len();
        for pos in 0..n {
            let mid = (self.geo.col_left(pos, sx) + self.geo.col_left(pos + 1, sx)) / 2.0;
            if x < mid {
                return pos;
            }
        }
        n
    }

    fn autoscroll_x(&mut self, x: f32) {
        let b = self.geo.body;
        let dx = if x < b.left() + 20.0 {
            (b.left() + 20.0 - x).min(60.0)
        } else if x > b.right() - 20.0 {
            -(x - (b.right() - 20.0)).min(60.0)
        } else {
            0.0
        };
        if dx != 0.0 {
            self.scroll_by(vec2(dx * 0.5, 0.0));
            self.ctx.request_repaint();
        }
    }

    fn autoscroll(&mut self, p: egui::Pos2) {
        let b = self.geo.body;
        let dy = if p.y < b.top() {
            (b.top() - p.y).min(200.0)
        } else if p.y > b.bottom() {
            -(p.y - b.bottom()).min(200.0)
        } else {
            0.0
        };
        let dx = if p.x < b.left() {
            (b.left() - p.x).min(100.0)
        } else if p.x > b.right() {
            -(p.x - b.right()).min(100.0)
        } else {
            0.0
        };
        if dx != 0.0 || dy != 0.0 {
            self.scroll_by(vec2(dx * 0.5, dy * 0.5));
            self.ctx.request_repaint();
        }
    }

    fn rownum_input(&mut self, ui: &mut Ui, rownums: &Response) {
        let sy = self.state.rt.scroll_y;
        let mods = ui.input(|i| i.modifiers);
        if ui.input(|i| i.pointer.primary_pressed())
            && rownums.is_pointer_button_down_on()
            && let Some(p) = rownums.interact_pointer_pos()
        {
            ui.memory_mut(|m| m.request_focus(self.id));
            if let Some(row) = self.geo.row_at(p.y, sy, false) {
                let sel = &mut self.state.rt.selection;
                if mods.shift {
                    let anchor = sel.anchor().row;
                    sel.select_rows(anchor, row, false);
                } else {
                    sel.select_rows(row, row, mods.command);
                }
                self.state.rt.drag = Drag::Rows;
            }
        }
        if self.state.rt.drag == Drag::Rows {
            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                self.autoscroll(egui::pos2(self.geo.body.left() + 1.0, p.y));
                if let Some(row) = self.geo.row_at(p.y, self.state.rt.scroll_y, true) {
                    let sel = &mut self.state.rt.selection;
                    let anchor = sel.anchor().row;
                    let additive = mods.command;
                    if !additive {
                        sel.select_rows(anchor, row, false);
                    }
                }
            }
            if !ui.input(|i| i.pointer.primary_down()) {
                self.state.rt.drag = Drag::None;
            }
        }
        if rownums.secondary_clicked()
            && let Some(p) = rownums.interact_pointer_pos()
            && let Some(row) = self.geo.row_at(p.y, sy, false)
            && !self.state.rt.selection.row_has_selection(row)
        {
            self.state.rt.selection.select_rows(row, row, false);
        }
    }

    fn body_input(&mut self, ui: &mut Ui, body: &Response) {
        let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
        let (pressed, secondary, mods) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.secondary_pressed(), i.modifiers));
        // Clicks inside the open inline editor belong to the editor.
        let in_editor = |p: egui::Pos2| self.state.rt.editing.as_ref().is_some_and(|e| self.pos_of(e.col).is_some_and(|pos| self.geo.cell_rect(e.row, pos, sx, sy).contains(p)));
        if (pressed || secondary)
            && body.contains_pointer()
            && !matches!(self.state.rt.drag, Drag::VThumb { .. } | Drag::HThumb { .. })
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
            && self.geo.body.contains(p)
            && !in_editor(p)
        {
            ui.memory_mut(|m| m.request_focus(self.id));
            if let (Some(row), Some(pos)) = (self.geo.row_at(p.y, sy, false), self.geo.col_at(p.x, sx, false)) {
                let cell = CellPos::new(row, pos);
                if self.state.rt.editing.as_ref().is_some_and(|e| e.row != row || self.pos_of(e.col) != Some(pos)) {
                    self.commit_edit(false, 0, 0);
                }
                if secondary {
                    if !self.state.rt.selection.contains(row, pos) {
                        self.state.rt.selection.select_cell(cell);
                    }
                } else {
                    let col = self.geo.cols[pos];
                    let fk = self.model.foreign_key_for_column(col);
                    if mods.command && !mods.shift && fk.is_some() {
                        self.navigate_fk(row, col);
                    } else if mods.shift {
                        self.state.rt.selection.extend_to(cell);
                        self.state.rt.drag = Drag::Cells;
                    } else if mods.command {
                        self.state.rt.selection.toggle_cell(cell);
                        self.state.rt.drag = Drag::Cells;
                    } else {
                        self.state.rt.selection.select_cell(cell);
                        self.state.rt.drag = Drag::Cells;
                    }
                }
            }
        }
        if self.state.rt.drag == Drag::Cells {
            if ui.input(|i| i.pointer.primary_down()) {
                if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                    self.autoscroll(p);
                    let (sx, sy) = (self.state.rt.scroll_x, self.state.rt.scroll_y);
                    if let (Some(row), Some(pos)) = (self.geo.row_at(p.y, sy, true), self.geo.col_at(p.x, sx, true)) {
                        let cell = CellPos::new(row, pos);
                        if cell != self.state.rt.selection.active() {
                            self.state.rt.selection.extend_to(cell);
                        }
                    }
                }
            } else {
                self.state.rt.drag = Drag::None;
            }
        }
        if body.double_clicked()
            && !mods.command
            && let Some((row, col)) = self.active()
        {
            self.start_edit(row, col, None);
        }
        // FK link cursor.
        if let Some(c) = self.hover_cell
            && mods.command
            && self.geo.cols.get(c.col).and_then(|&col| self.model.foreign_key_for_column(col)).is_some()
        {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }

    /// Emit a NavigateForeignKey action for the FK of `col` in `row`.
    pub fn navigate_fk(&mut self, row: usize, col: usize) {
        let Some(i) = self.model.foreign_key_for_column(col) else { return };
        let link = self.model.foreign_keys()[i].clone();
        let values: Vec<Value> = link.columns.iter().map(|&c| self.model.cell(row, c).unwrap_or_default()).collect();
        if values.iter().all(Value::is_null) {
            self.notify(tr("The foreign key value is NULL"), false);
            return;
        }
        self.actions.push(GridAction::NavigateForeignKey { link, values });
    }

    fn keyboard(&mut self, ui: &mut Ui) {
        let events = ui.input(|i| i.events.clone());
        let shift_now = ui.input(|i| i.modifiers.shift);
        let nrows = self.geo.nrows;
        let ncols = self.geo.cols.len();
        let page = self.geo.page_rows() as isize;
        let mut handled_space = false;
        for ev in events {
            match ev {
                Event::Key { key, pressed: true, modifiers: m, .. } => {
                    if m.command && m.alt {
                        match key {
                            Key::ArrowUp => self.copy_from_adjacent(true),
                            Key::ArrowDown => self.copy_from_adjacent(false),
                            Key::ArrowLeft => self.move_active_column(-1),
                            Key::ArrowRight => self.move_active_column(1),
                            _ => {}
                        }
                        if matches!(key, Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight) {
                            continue;
                        }
                    }
                    let sel = &mut self.state.rt.selection;
                    let mut moved = true;
                    match key {
                        Key::ArrowUp if m.command => sel.move_by(-(nrows as isize), 0, nrows, ncols, m.shift),
                        Key::ArrowDown if m.command => sel.move_by(nrows as isize, 0, nrows, ncols, m.shift),
                        Key::ArrowLeft if m.command => sel.move_by(0, -(ncols as isize), nrows, ncols, m.shift),
                        Key::ArrowRight if m.command => sel.move_by(0, ncols as isize, nrows, ncols, m.shift),
                        Key::ArrowUp => sel.move_by(-1, 0, nrows, ncols, m.shift),
                        Key::ArrowDown => sel.move_by(1, 0, nrows, ncols, m.shift),
                        Key::ArrowLeft => sel.move_by(0, -1, nrows, ncols, m.shift),
                        Key::ArrowRight => sel.move_by(0, 1, nrows, ncols, m.shift),
                        Key::PageUp => {
                            sel.move_by(-page, 0, nrows, ncols, m.shift);
                            self.state.rt.scroll_y -= self.geo.body.height() as f64;
                        }
                        Key::PageDown => {
                            sel.move_by(page, 0, nrows, ncols, m.shift);
                            self.state.rt.scroll_y += self.geo.body.height() as f64;
                        }
                        Key::Home if m.command => sel.move_by(-(nrows as isize), -(ncols as isize), nrows, ncols, m.shift),
                        Key::End if m.command => sel.move_by(nrows as isize, ncols as isize, nrows, ncols, m.shift),
                        Key::Home => sel.move_by(0, -(ncols as isize), nrows, ncols, m.shift),
                        Key::End => sel.move_by(0, ncols as isize, nrows, ncols, m.shift),
                        // Tab shows the record view (DBeaver); Shift+Tab moves left.
                        Key::Tab if m.shift => sel.tab(true, nrows, ncols),
                        Key::Tab => {
                            self.actions.push(GridAction::SwitchView(super::ResultViewKind::Record));
                            moved = false;
                        }
                        _ => moved = false,
                    }
                    if moved {
                        self.state.rt.scroll_to_active = true;
                        continue;
                    }
                    match key {
                        Key::A if m.command => self.state.rt.selection.select_all(),
                        Key::Z if m.command && !m.shift => {
                            if !self.model.undo() {
                                self.notify(tr("Nothing to undo"), false);
                            }
                        }
                        Key::S if m.command => self.actions.push(GridAction::SaveChanges),
                        Key::R if m.command => self.actions.push(GridAction::Refresh),
                        Key::F5 => self.actions.push(GridAction::Refresh),
                        Key::D if m.command => self.duplicate_selected(),
                        Key::Insert if m.shift => self.add_row_before_active(),
                        Key::Insert => self.add_row_after_active(),
                        Key::Enter | Key::F2 if m.shift => {
                            if let Some((row, col)) = self.active() {
                                self.actions.push(GridAction::OpenValuePanel { row, col });
                            }
                        }
                        Key::Enter | Key::F2 => {
                            if let Some((row, col)) = self.active() {
                                self.start_edit(row, col, None);
                            }
                        }
                        Key::Delete if m.shift => self.set_selected_null(),
                        Key::Delete => self.delete_selected(),
                        Key::Backspace => {
                            if let Some((row, col)) = self.active() {
                                self.start_edit(row, col, Some(String::new()));
                            }
                        }
                        Key::Space if !m.command => {
                            if let Some((row, col)) = self.active()
                                && self.model.column_kind(col) == DataKind::Bool
                            {
                                self.toggle_bool(row, col);
                                handled_space = true;
                            }
                        }
                        Key::Escape => {
                            self.state.rt.message = None;
                        }
                        _ => {}
                    }
                }
                Event::Text(t) => {
                    if handled_space && t == " " {
                        continue;
                    }
                    let cmd = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                    if !cmd && !t.chars().all(char::is_control) {
                        if let Some((row, col)) = self.active() {
                            self.start_edit(row, col, Some(t));
                        }
                        break;
                    }
                }
                Event::Copy | Event::Cut => {
                    if shift_now {
                        self.state.rt.copy_dialog = true;
                    } else {
                        self.copy_selection(&CopyOptions::plain());
                    }
                }
                Event::Paste(text) => self.paste(&text),
                _ => {}
            }
        }
    }

    /// Copy the selection to the clipboard.
    pub fn copy_selection(&mut self, opts: &CopyOptions) {
        match super::DataGrid::copy_text(self.model, self.state, self.options, opts) {
            Ok((text, cells)) => {
                self.ctx.copy_text(text);
                self.actions.push(GridAction::Copied { cells, format: opts.format });
                let msg = if opts.format == CopyFormat::Tsv && !opts.include_header {
                    trf("Copied {} cell(s)", &[&cells])
                } else {
                    trf("Copied {} cell(s) as {}", &[&cells, &opts.format.label()])
                };
                self.notify(msg, false);
            }
            Err(e) => self.notify(e, true),
        }
    }

    /// Paste text at the active cell.
    pub fn paste(&mut self, text: &str) {
        if !self.options.allow_editing {
            self.notify(tr("Editing is disabled"), true);
            return;
        }
        match super::paste_into(self.model, self.state, text) {
            Ok(n) => self.notify(trf("Pasted {} cell(s)", &[&n]), false),
            Err(e) => self.notify(e, true),
        }
    }

    /// Start editing a cell (or toggle / open a dropdown / the value panel).
    pub fn start_edit(&mut self, row: usize, col: usize, typed: Option<String>) {
        if !self.options.allow_editing {
            self.notify(tr("Editing is disabled"), true);
            return;
        }
        if let Err(e) = self.model.can_edit_cell(row, col) {
            self.notify(e, true);
            return;
        }
        let kind = self.model.column_kind(col);
        let value = self.model.cell(row, col).unwrap_or_default();
        if kind == DataKind::Bool && typed.is_none() {
            self.toggle_bool(row, col);
            return;
        }
        if typed.is_none() && (self.model.dictionary(col).is_some() || self.model.enum_values(col).is_some()) {
            self.state.rt.dropdown = Some(super::state::Dropdown { row, col, search: String::new(), just_opened: true });
            return;
        }
        let text = value.to_text().into_owned();
        if typed.is_none() && (text.contains('\n') || text.chars().count() > 2000 || matches!(value, Value::Bytes(_))) {
            self.actions.push(GridAction::OpenValuePanel { row, col });
            return;
        }
        let is_typed = typed.is_some();
        self.state.rt.editing = Some(EditSession { row, col, text: typed.unwrap_or_else(|| text.clone()), original: text, error: None, just_started: true, typed: is_typed });
        self.state.rt.scroll_to_active = true;
    }

    /// Commit the open editor. `move_rows`/`move_cols` move the selection afterwards.
    pub fn commit_edit(&mut self, keep_on_error: bool, move_rows: isize, move_cols: isize) -> bool {
        let Some(mut session) = self.state.rt.editing.take() else { return true };
        if session.text != session.original
            && let Err(e) = self.model.set_cell(session.row, session.col, &session.text)
        {
            if keep_on_error {
                session.error = Some(e);
                self.state.rt.editing = Some(session);
                return false;
            }
            self.notify(e, true);
            return false;
        }
        if move_rows != 0 || move_cols != 0 {
            let (n, m) = (self.geo.nrows, self.geo.cols.len());
            if move_cols != 0 && move_rows == 0 {
                self.state.rt.selection.tab(move_cols < 0, n, m);
            } else {
                self.state.rt.selection.move_by(move_rows, move_cols, n, m, false);
            }
            self.state.rt.scroll_to_active = true;
        }
        self.ctx.memory_mut(|mem| mem.request_focus(self.id));
        true
    }

    /// Toggle a boolean cell (NULL → true → false → true ...).
    pub fn toggle_bool(&mut self, row: usize, col: usize) {
        if let Err(e) = self.model.can_edit_cell(row, col) {
            self.notify(e, true);
            return;
        }
        let v = self.model.cell(row, col).unwrap_or_default();
        let next = !v.as_bool().unwrap_or(false);
        if let Err(e) = self.model.set_value(row, col, Value::Bool(next)) {
            self.notify(e, true);
        }
    }

    /// Set selected cells to NULL.
    pub fn set_selected_null(&mut self) {
        let cells = self.selected_cells(100_000);
        let edits = cells.into_iter().map(|(r, c)| (r, c, crate::model::CellValue::Value(Value::Null))).collect();
        if let Err(e) = self.model.set_cells(edits) {
            self.notify(e, true);
        }
    }

    /// Set selected cells to DEFAULT.
    pub fn set_selected_default(&mut self) {
        let cells = self.selected_cells(100_000);
        let edits = cells.into_iter().map(|(r, c)| (r, c, crate::model::CellValue::Default)).collect();
        if let Err(e) = self.model.set_cells(edits) {
            self.notify(e, true);
        }
    }

    /// Selected (row, model column) pairs, excluding virtual columns.
    pub fn selected_cells(&self, limit: usize) -> Vec<(usize, usize)> {
        let sel = &self.state.rt.selection;
        let ncols = self.geo.cols.len();
        let mut out = Vec::new();
        for r in sel.rows(self.geo.nrows, limit) {
            for p in sel.columns(ncols) {
                if sel.contains(r, p) && !self.model.is_virtual_column(self.geo.cols[p]) {
                    out.push((r, self.geo.cols[p]));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Mark selected rows deleted.
    pub fn delete_selected(&mut self) {
        let rows = self.state.rt.selection.rows(self.geo.nrows, 1_000_000);
        if rows.is_empty() {
            return;
        }
        match self.model.delete_rows(&rows) {
            Ok(n) => {
                let msg = trf("{} row(s) marked for deletion", &[&n]);
                self.notify(msg, false);
            }
            Err(e) => self.notify(e, true),
        }
    }

    /// Duplicate selected rows.
    pub fn duplicate_selected(&mut self) {
        let rows = self.state.rt.selection.rows(self.geo.nrows, 10_000);
        match self.model.duplicate_rows(&rows) {
            Ok(new) => {
                if let (Some(&first), Some(&last)) = (new.first(), new.last()) {
                    let col = self.state.rt.selection.active().col;
                    self.state.rt.selection.select_rows(first, last, false);
                    self.state.rt.selection.move_to(CellPos::new(first, col), true);
                    self.state.rt.selection.select_cell(CellPos::new(first, col));
                    self.state.rt.scroll_to_active = true;
                }
            }
            Err(e) => self.notify(e, true),
        }
    }

    /// Run a [`super::GridCommand`].
    pub fn run_command(&mut self, ui: &Ui, cmd: super::GridCommand) {
        use super::GridCommand as C;
        match cmd {
            C::AddRowBefore => self.add_row_before_active(),
            C::CopyFromAbove => self.copy_from_adjacent(true),
            C::CopyFromBelow => self.copy_from_adjacent(false),
            C::GenerateUuid => self.generate_uuid(),
            C::MoveColumnLeft => self.move_active_column(-1),
            C::MoveColumnRight => self.move_active_column(1),
            C::FitColumnsToValues => {
                let painter = ui.painter().clone();
                self.fit_columns_to_values(&painter);
            }
            C::FitColumnsToScreen => self.fit_columns_to_screen(),
            C::HideEmptyColumns => self.hide_empty_columns(),
        }
    }

    /// Add a new row before the active row.
    pub fn add_row_before_active(&mut self) {
        let before = self.active().map(|(r, _)| r).unwrap_or(0);
        match self.model.add_row_before(before) {
            Ok(row) => {
                let col = self.state.rt.selection.active().col;
                self.state.rt.selection.select_cell(CellPos::new(row, col));
                self.state.rt.scroll_to_active = true;
            }
            Err(e) => self.notify(e, true),
        }
    }

    /// Copy into the selected cells the value of the same column in the row
    /// above (`above`) or below.
    pub fn copy_from_adjacent(&mut self, above: bool) {
        let cells = self.selected_cells(100_000);
        let mut edits = Vec::with_capacity(cells.len());
        for (r, c) in cells {
            let src = if above { r.checked_sub(1) } else { Some(r + 1).filter(|&x| x < self.geo.nrows) };
            let Some(src) = src else { continue };
            let v = self.model.cell(src, c).unwrap_or(Value::Null);
            edits.push((r, c, crate::model::CellValue::Value(v)));
        }
        if edits.is_empty() {
            return;
        }
        if let Err(e) = self.model.set_cells(edits) {
            self.notify(e, true);
        }
    }

    /// Put a new random UUID into each selected cell.
    pub fn generate_uuid(&mut self) {
        let cells = self.selected_cells(100_000);
        let edits = cells.into_iter().map(|(r, c)| (r, c, crate::model::CellValue::Value(Value::text(uuid::Uuid::new_v4().to_string())))).collect();
        if let Err(e) = self.model.set_cells(edits) {
            self.notify(e, true);
        }
    }

    /// Move the active column one position to the left (`delta` -1) or right (+1).
    pub fn move_active_column(&mut self, delta: isize) {
        let Some((_, col)) = self.active() else { return };
        let Some(pos) = self.geo.cols.iter().position(|&c| c == col) else { return };
        let to = pos as isize + delta;
        if to < 0 || to as usize >= self.geo.cols.len() {
            return;
        }
        self.state.move_visible_column(pos, to as usize);
        let row = self.state.rt.selection.active().row;
        self.state.rt.selection.select_cell(CellPos::new(row, to as usize));
        self.state.rt.scroll_to_active = true;
    }

    /// Widths that fit the visible values of every column.
    pub fn fit_columns_to_values(&mut self, painter: &egui::Painter) {
        for col in self.geo.cols.clone() {
            self.autosize_with(painter, col);
        }
    }

    /// Widths that fill the visible width, in proportion to the current ones.
    pub fn fit_columns_to_screen(&mut self) {
        let cols = self.geo.cols.clone();
        let total: f32 = cols.iter().map(|&c| self.state.column_width(c)).sum();
        let avail = self.geo.body.width() - 2.0;
        if total <= 0.0 || avail <= 0.0 {
            return;
        }
        let k = avail / total;
        for c in cols {
            let w = (self.state.column_width(c) * k).max(super::state::MIN_COLUMN_WIDTH);
            self.state.set_column_width(c, w);
        }
    }

    /// Hide the columns whose fetched values are all NULL or empty.
    pub fn hide_empty_columns(&mut self) {
        let cols = self.geo.cols.clone();
        let mut has_data = vec![false; self.model.column_count()];
        self.model.visit_rows(0..self.geo.nrows, |_, cells| {
            for &c in &cols {
                if !has_data[c] && cells.value_of(c).is_some_and(|v| !v.is_null() && !v.to_text().is_empty()) {
                    has_data[c] = true;
                }
            }
        });
        let mut hidden = 0;
        for c in cols {
            if !has_data[c] {
                self.state.set_column_visible(c, false);
                hidden += 1;
            }
        }
        self.notify(trf("{} column(s) without data hidden", &[&hidden]), false);
    }

    /// Add a new row after the active row and start editing it.
    pub fn add_row_after_active(&mut self) {
        let after = self.active().map(|(r, _)| r);
        match self.model.add_row(after.or(self.geo.nrows.checked_sub(1))) {
            Ok(row) => {
                let col = self
                    .geo
                    .cols
                    .iter()
                    .position(|&c| self.model.can_edit_cell(row, c).is_ok() && self.model.column_meta(c).is_some_and(|m| m.auto_increment != Some(true)))
                    .unwrap_or(0);
                self.state.rt.selection.select_cell(CellPos::new(row, col));
                self.state.rt.scroll_to_active = true;
            }
            Err(e) => self.notify(e, true),
        }
    }

    /// Auto-size a column from its header and the visible cells.
    pub fn autosize_column(&mut self, ui: &Ui, col: usize) {
        let painter = ui.painter().clone();
        self.autosize_with(&painter, col);
    }

    /// Text width with the grid font.
    pub fn measure(&mut self, painter: &egui::Painter, text: &str) -> f32 {
        match self.state.rt.glyphs.measure(painter, text) {
            Some(w) => w,
            None => super::text::measure_slow(painter, &self.font, text),
        }
    }
}

/// Next sort keys after clicking the sort indicator of `col`:
/// ascending → descending → none. `add` keeps other keys (multi-column sort).
pub(crate) fn next_sort(current: &[SortKey], col: usize, add: bool) -> Vec<SortKey> {
    let existing = current.iter().find(|k| k.column == col).copied();
    let next = match existing {
        None => Some(SortKey::asc(col)),
        Some(k) if !k.descending => Some(SortKey::desc(col)),
        Some(_) => None,
    };
    if add {
        let mut keys: Vec<SortKey> = current.to_vec();
        match (keys.iter().position(|k| k.column == col), next) {
            (Some(i), Some(k)) => keys[i] = k,
            (Some(i), None) => {
                keys.remove(i);
            }
            (None, Some(k)) => keys.push(k),
            (None, None) => {}
        }
        keys
    } else {
        next.into_iter().collect()
    }
}
