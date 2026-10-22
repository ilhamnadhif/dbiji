//! Per-frame state of the grid: geometry and orchestration.

use super::selection::{CellPos, Selection};
use super::state::DataGridState;
use super::{GridAction, GridOptions, GridResponse};
use crate::model::ResultModel;
use crate::theme::GridTheme;
use dbiji_core::{tr, trf};
use egui::{FontId, Id, Pos2, Rect, Sense, TextStyle, Ui, Vec2, pos2, vec2};
use std::ops::Range;

/// Thickness of the scroll bars.
pub(super) const BAR: f32 = 12.0;
/// Horizontal cell padding.
pub(super) const PAD: f32 = 5.0;

/// Geometry of one frame.
#[derive(Debug, Clone)]
pub(super) struct Geo {
    pub rect: Rect,
    pub header: Rect,
    pub rownums: Rect,
    pub body: Rect,
    pub corner: Rect,
    pub vbar: Option<Rect>,
    pub hbar: Option<Rect>,
    pub row_h: f32,
    /// Visible model columns in display order.
    pub cols: Vec<usize>,
    /// Column start offsets in content coordinates (`cols.len() + 1` entries).
    pub xs: Vec<f32>,
    pub nrows: usize,
}

impl Geo {
    pub fn content_w(&self) -> f32 {
        self.xs.last().copied().unwrap_or(0.0)
    }

    pub fn content_h(&self) -> f64 {
        self.nrows as f64 * self.row_h as f64
    }

    pub fn max_scroll_y(&self) -> f64 {
        (self.content_h() - self.body.height() as f64).max(0.0)
    }

    pub fn max_scroll_x(&self) -> f32 {
        (self.content_w() - self.body.width()).max(0.0)
    }

    /// Screen y of the top of `row`.
    pub fn row_top(&self, row: usize, scroll_y: f64) -> f32 {
        self.body.top() + (row as f64 * self.row_h as f64 - scroll_y) as f32
    }

    /// Screen x of the left edge of visible column position `pos`.
    pub fn col_left(&self, pos: usize, scroll_x: f32) -> f32 {
        self.body.left() + self.xs.get(pos).copied().unwrap_or_else(|| self.content_w()) - scroll_x
    }

    /// Rows intersecting the body.
    pub fn visible_rows(&self, scroll_y: f64) -> Range<usize> {
        if self.nrows == 0 || self.row_h <= 0.0 {
            return 0..0;
        }
        let first = (scroll_y / self.row_h as f64).floor().max(0.0) as usize;
        let count = (self.body.height() / self.row_h).ceil() as usize + 1;
        first.min(self.nrows)..(first + count).min(self.nrows)
    }

    /// Column positions intersecting the body.
    pub fn visible_cols(&self, scroll_x: f32) -> Range<usize> {
        let n = self.cols.len();
        if n == 0 {
            return 0..0;
        }
        let left = scroll_x;
        let right = scroll_x + self.body.width();
        let first = self.xs.partition_point(|&x| x <= left).saturating_sub(1).min(n);
        let mut last = first;
        while last < n && self.xs[last] < right {
            last += 1;
        }
        first..last
    }

    /// Row at screen y (clamped into the data when `clamp`).
    pub fn row_at(&self, y: f32, scroll_y: f64, clamp: bool) -> Option<usize> {
        if self.nrows == 0 {
            return None;
        }
        let content = (y - self.body.top()) as f64 + scroll_y;
        let r = (content / self.row_h as f64).floor();
        if clamp {
            Some(r.clamp(0.0, (self.nrows - 1) as f64) as usize)
        } else if r < 0.0 || r >= self.nrows as f64 {
            None
        } else {
            Some(r as usize)
        }
    }

    /// Column position at screen x (clamped when `clamp`).
    pub fn col_at(&self, x: f32, scroll_x: f32, clamp: bool) -> Option<usize> {
        let n = self.cols.len();
        if n == 0 {
            return None;
        }
        let content = x - self.body.left() + scroll_x;
        if content < 0.0 {
            return clamp.then_some(0);
        }
        let p = self.xs.partition_point(|&s| s <= content);
        if p == 0 {
            return clamp.then_some(0);
        }
        if p > n {
            return clamp.then_some(n - 1);
        }
        Some(p - 1)
    }

    /// Screen rectangle of a cell.
    pub fn cell_rect(&self, row: usize, pos: usize, scroll_x: f32, scroll_y: f64) -> Rect {
        let x0 = self.col_left(pos, scroll_x);
        let x1 = self.col_left(pos + 1, scroll_x);
        let y0 = self.row_top(row, scroll_y);
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y0 + self.row_h))
    }

    /// Number of fully visible rows (page size).
    pub fn page_rows(&self) -> usize {
        ((self.body.height() / self.row_h).floor() as usize).max(1)
    }
}

/// Everything a frame needs.
pub(super) struct GridFrame<'a> {
    pub model: &'a mut ResultModel,
    pub state: &'a mut DataGridState,
    pub options: &'a GridOptions,
    pub id: Id,
    pub ctx: egui::Context,
    pub actions: Vec<GridAction>,
    pub geo: Geo,
    pub font: FontId,
    pub theme: GridTheme,
    pub selection_before: Selection,
    /// Full text of the hovered cell when it was elided (tooltip).
    pub hover_tooltip: Option<(Rect, String)>,
    /// Cell under the pointer.
    pub hover_cell: Option<CellPos>,
    pub focused: bool,
    /// The menu is shown by another view of the rows (the record view).
    pub detached: bool,
}

/// The body's context menu of `resp` and the dialogs its items open, shown
/// by another view of the grid's rows (the record view): the items, and the
/// editing keys when `keys`, act on `cells` (row, model column); the view's
/// own items (`own`) come first.
/// Afterwards the grid's selection is what it was, unless an item changed it
/// (a new row is selected). Returns the actions, and the cell "Edit cell"
/// opened (row, column, text), for the view's own editor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn detached_menu(
    model: &mut ResultModel,
    state: &mut DataGridState,
    options: &GridOptions,
    id: Id,
    ui: &mut Ui,
    resp: Option<&egui::Response>,
    keys: bool,
    cells: &[(usize, usize)],
    own: impl FnOnce(&mut Ui),
) -> (Vec<GridAction>, Option<(usize, usize, String)>) {
    let font = options.font.clone().unwrap_or_else(|| TextStyle::Body.resolve(ui.style()));
    let geo = compute_geometry(model, state, options, ui.max_rect(), font.size * 1.3, font.size * 0.6);
    let before = state.rt.selection.clone();
    let mut picked = Selection::default();
    for (i, p) in cells.iter().filter_map(|&(r, c)| geo.cols.iter().position(|&x| x == c).map(|pos| CellPos::new(r, pos))).enumerate() {
        if i == 0 {
            picked.select_cell(p);
        } else {
            picked.toggle_cell(p);
        }
    }
    let acting = !picked.is_empty();
    if acting {
        state.rt.selection = picked.clone();
    }
    let mut f = GridFrame {
        model,
        state,
        options,
        id,
        ctx: ui.ctx().clone(),
        actions: Vec::new(),
        geo,
        font,
        theme: GridTheme::from_visuals(ui.visuals()),
        selection_before: before.clone(),
        hover_tooltip: None,
        hover_cell: None,
        focused: false,
        detached: true,
    };
    if acting {
        if let Some(resp) = resp {
            egui::Popup::context_menu(resp).show(|ui| {
                ui.set_min_width(220.0);
                own(ui);
                f.menu_items(ui);
            });
        }
        if keys {
            f.detached_keys(ui);
        }
    }
    f.dialogs(ui);
    let actions = f.actions;
    // "Advanced copy…" copies the picked cells when its dialog closes.
    if acting && state.rt.selection == picked && !state.rt.copy_dialog {
        state.rt.selection = before;
    }
    // The grid's editor and list of values are not shown here: the view edits.
    let edit = state.rt.editing.take().map(|e| (e.row, e.col, e.text)).or_else(|| state.rt.dropdown.take().map(|d| (d.row, d.col, model.cell(d.row, d.col).filter(|v| !v.is_null()).map(|v| v.to_text().into_owned()).unwrap_or_default())));
    (actions, edit)
}

pub(super) fn show(model: &mut ResultModel, state: &mut DataGridState, options: &GridOptions, id_salt: Id, ui: &mut Ui) -> GridResponse {
    let id = ui.make_persistent_id(id_salt);
    model.try_sync();
    if let Some(Err(e)) = model.poll_view_job() {
        state.show_message(e, true);
    }
    // The view may have changed under an open cell editor (a sort or filter, a
    // refresh): it edits the same record, at its new display row, or closes.
    if let Some(e) = &mut state.rt.editing {
        match e.anchor.and_then(|a| model.anchored_row(a, e.row)) {
            Some(row) => e.row = row,
            None => state.rt.editing = None,
        }
    }
    if let Some(d) = &mut state.rt.dropdown {
        match d.anchor.and_then(|a| model.anchored_row(a, d.row)) {
            Some(row) => d.row = row,
            None => state.rt.dropdown = None,
        }
    }
    state.sync_columns(model);
    let store_id = std::sync::Arc::as_ptr(model.store()) as *const () as usize;
    if state.rt.view_store != store_id {
        state.rt.view_store = store_id;
        state.rt.auto_view_len = usize::MAX;
    }
    // Rows fetched after a client sort/filter: recompute once the fetch settles.
    if model.is_view_stale() && !model.fetch.fetching && !model.is_view_job_running() && state.rt.auto_view_len != model.stored_row_count() {
        let spec = model.client_view_spec();
        if !spec.is_identity() {
            state.rt.auto_view_len = model.stored_row_count();
            let ctx = ui.ctx().clone();
            model.start_client_view(spec, Box::new(move || ctx.request_repaint()));
        }
    }

    let mut font = options.font.clone().unwrap_or_else(|| TextStyle::Body.resolve(ui.style()));
    if let Some(z) = state.zoom {
        font.size = (font.size * z.clamp(0.5, 3.0)).round();
    }
    let theme = GridTheme::from_visuals(ui.visuals());
    let avail = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(avail.min, avail.size().max(vec2(160.0, 80.0)));
    ui.allocate_rect(rect, Sense::hover());

    state.rt.glyphs.begin_frame(ui.painter(), &font);
    let mut text_h = state.rt.glyphs.row_height();
    if text_h <= 0.0 {
        text_h = font.size * 1.3;
    }
    let digit_w = state.rt.glyphs.measure(ui.painter(), "0").unwrap_or(font.size * 0.6);
    let geo = compute_geometry(model, state, options, rect, text_h, digit_w);
    // Keep scroll offsets in range.
    state.rt.scroll_y = state.rt.scroll_y.clamp(0.0, geo.max_scroll_y());
    state.rt.scroll_x = state.rt.scroll_x.clamp(0.0, geo.max_scroll_x());
    state.rt.selection.clamp(geo.nrows, geo.cols.len());
    state.rt.last_geometry = Some((geo.body, geo.header, geo.rownums, geo.row_h));

    let mut f = GridFrame {
        model,
        state,
        options,
        id,
        ctx: ui.ctx().clone(),
        actions: Vec::new(),
        geo,
        font,
        theme,
        selection_before: Selection::default(),
        hover_tooltip: None,
        hover_cell: None,
        focused: false,
        detached: false,
    };
    f.selection_before = f.state.rt.selection.clone();
    f.fit_columns(&ui.painter().clone());

    let (body, header, rownums) = f.interact(ui);
    for cmd in std::mem::take(&mut f.state.rt.pending) {
        f.run_command(ui, cmd);
    }
    f.ensure_active_visible();
    f.check_fetch_more();
    f.paint(ui);
    f.show_editor(ui);
    f.show_dropdown(ui);
    f.cell_tooltip(ui);
    f.fetching_overlay(ui);
    f.context_menu(&body);
    f.context_menu(&rownums);
    f.header_menu(&header);
    f.header_popup(ui);
    f.dialogs(ui);

    let selection_changed = f.state.rt.selection != f.selection_before;
    if selection_changed {
        f.actions.push(GridAction::SelectionChanged);
    }
    let mut body = body;
    f.accessibility(&mut body, selection_changed);
    let active_cell = f.state.active_cell();
    GridResponse { response: body, actions: f.actions, active_cell, selection_changed }
}

fn compute_geometry(model: &ResultModel, state: &DataGridState, options: &GridOptions, rect: Rect, text_h: f32, digit_w: f32) -> Geo {
    let nrows = model.row_count();
    let cols = state.visible_columns();
    let mut xs = Vec::with_capacity(cols.len() + 1);
    let mut x = 0.0f32;
    xs.push(x);
    for &c in &cols {
        x += state.column_width(c);
        xs.push(x);
    }
    let row_h = options.row_height_for(text_h);
    let header_h = (text_h + if options.compact_rows { 10.0 } else { 12.0 }).round();
    let rownum_w = if options.show_row_numbers {
        let digits = (nrows.max(1) as f64).log10().floor() as usize + 1;
        (digits.max(2) as f32 * digit_w + 2.0 * PAD + 10.0).round()
    } else {
        0.0
    };
    let content_w = x;
    let content_h = nrows as f64 * row_h as f64;
    let avail_w = rect.width() - rownum_w;
    let avail_h = rect.height() - header_h;
    let mut need_v = content_h > avail_h as f64;
    let need_h = content_w > avail_w - if need_v { BAR } else { 0.0 };
    if !need_v && need_h {
        need_v = content_h > (avail_h - BAR) as f64;
    }
    let body = Rect::from_min_max(rect.min + vec2(rownum_w, header_h), rect.max - vec2(if need_v { BAR } else { 0.0 }, if need_h { BAR } else { 0.0 }));
    Geo {
        rect,
        header: Rect::from_min_max(pos2(body.left(), rect.top()), pos2(body.right(), body.top())),
        rownums: Rect::from_min_max(pos2(rect.left(), body.top()), pos2(body.left(), body.bottom())),
        body,
        corner: Rect::from_min_max(rect.min, pos2(body.left(), body.top())),
        vbar: need_v.then(|| Rect::from_min_max(pos2(body.right(), body.top()), pos2(rect.right(), body.bottom()))),
        hbar: need_h.then(|| Rect::from_min_max(pos2(body.left(), body.bottom()), pos2(body.right(), rect.bottom()))),
        row_h,
        cols,
        xs,
        nrows,
    }
}

impl GridFrame<'_> {
    /// Visible column position of a model column.
    pub fn pos_of(&self, model_col: usize) -> Option<usize> {
        self.geo.cols.iter().position(|&c| c == model_col)
    }

    /// Active cell as (row, model column).
    pub fn active(&self) -> Option<(usize, usize)> {
        if self.state.rt.selection.is_empty() || self.geo.nrows == 0 {
            return None;
        }
        let p = self.state.rt.selection.active();
        self.geo.cols.get(p.col).map(|&c| (p.row, c))
    }

    /// Scroll so that the active cell is visible.
    pub fn ensure_active_visible(&mut self) {
        if !std::mem::take(&mut self.state.rt.scroll_to_active) {
            return;
        }
        let Some((row, col)) = self.active() else { return };
        let Some(pos) = self.pos_of(col) else { return };
        let geo = &self.geo;
        let top = row as f64 * geo.row_h as f64;
        let bottom = top + geo.row_h as f64;
        let view_h = geo.body.height() as f64;
        let sy = &mut self.state.rt.scroll_y;
        if top < *sy {
            *sy = top;
        } else if bottom > *sy + view_h {
            *sy = bottom - view_h;
        }
        *sy = sy.clamp(0.0, geo.max_scroll_y());
        let left = geo.xs[pos];
        let right = geo.xs[pos + 1];
        let view_w = geo.body.width();
        let sx = &mut self.state.rt.scroll_x;
        if left < *sx {
            *sx = left;
        } else if right > *sx + view_w {
            *sx = (right - view_w).min(left);
        }
        *sx = sx.clamp(0.0, geo.max_scroll_x());
    }

    /// Emit FetchMore when the end of the fetched rows is close.
    pub fn check_fetch_more(&mut self) {
        let f = &self.model.fetch;
        if !f.has_more || f.fetching {
            return;
        }
        let rows = self.geo.visible_rows(self.state.rt.scroll_y);
        let near_end = rows.end + self.options.fetch_more_threshold >= self.geo.nrows;
        let stored = self.model.stored_row_count();
        let store_id = std::sync::Arc::as_ptr(self.model.store()) as *const () as usize;
        if self.state.rt.fetch_store != store_id {
            self.state.rt.fetch_store = store_id;
            self.state.rt.fetch_requested_at = None;
        }
        if near_end && self.state.rt.fetch_requested_at != Some(stored) {
            self.state.rt.fetch_requested_at = Some(stored);
            self.actions.push(GridAction::FetchMore);
        }
    }

    /// Notify the user: an action for the host, and a status overlay unless
    /// the host shows the message itself.
    pub fn notify(&mut self, message: impl Into<String>, error: bool) {
        let m = message.into();
        if !self.state.rt.notices_elsewhere {
            let now = self.ctx.input(|i| i.time);
            self.state.rt.message = Some((m.clone(), error, now));
        }
        self.actions.push(super::GridAction::Notify { message: m, error });
    }

    /// Accessibility label and change events.
    fn accessibility(&mut self, body: &mut egui::Response, changed: bool) {
        let rows = crate::format::group_thousands(&self.geo.nrows.to_string());
        let mut label = trf("Data grid, {} rows, {} columns", &[&rows, &self.geo.cols.len()]);
        if let Some((row, col)) = self.active() {
            // Never wait for the fetch task: keep the previous label when the store is busy.
            let Some(value) = self.model.try_cell(row, col, std::time::Duration::from_millis(2)) else {
                let last = self.state.rt.last_selection_label.clone();
                body.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &last));
                return;
            };
            let value = value.unwrap_or_default();
            let text = if value.is_null() {
                tr("NULL").to_string()
            } else {
                let t = value.to_text();
                if t.chars().count() > 200 { t.chars().take(200).collect::<String>() + "…" } else { t.into_owned() }
            };
            label.push_str(&trf(", selected cell: row {}, {} = {}", &[&(row + 1), &self.model.column_name(col), &text]));
        }
        if !self.model.is_editable()
            && let Some(r) = self.model.read_only_reason()
        {
            label.push_str(", ");
            label.push_str(r.short_label());
        }
        if changed && label != self.state.rt.last_selection_label {
            body.mark_changed();
        }
        self.state.rt.last_selection_label = label.clone();
        body.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &label));
    }

    /// Pointer position, if any.
    pub fn pointer(&self) -> Option<Pos2> {
        self.ctx.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()))
    }

    /// Scroll by a delta (content moves by `delta`).
    pub fn scroll_by(&mut self, delta: Vec2) {
        let rt = &mut self.state.rt;
        rt.scroll_y = (rt.scroll_y - delta.y as f64).clamp(0.0, self.geo.max_scroll_y());
        rt.scroll_x = (rt.scroll_x - delta.x).clamp(0.0, self.geo.max_scroll_x());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(nrows: usize, widths: &[f32]) -> Geo {
        let mut xs = vec![0.0];
        for w in widths {
            xs.push(xs.last().unwrap() + w);
        }
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0));
        Geo {
            rect,
            header: rect,
            rownums: rect,
            body: Rect::from_min_max(pos2(50.0, 20.0), pos2(390.0, 290.0)),
            corner: rect,
            vbar: None,
            hbar: None,
            row_h: 20.0,
            cols: (0..widths.len()).collect(),
            xs,
            nrows,
        }
    }

    #[test]
    fn visible_ranges_and_hit_testing() {
        let g = geo(1_000_000, &[100.0, 100.0, 100.0, 100.0, 100.0]);
        assert_eq!(g.visible_rows(0.0), 0..15);
        assert_eq!(g.visible_rows(10.0), 0..15);
        assert_eq!(g.visible_rows(19_999_990.0).start, 999_999);
        assert_eq!(g.visible_cols(0.0), 0..4);
        assert_eq!(g.visible_cols(150.0), 1..5);
        assert_eq!(g.row_at(25.0, 0.0, false), Some(0));
        assert_eq!(g.row_at(45.0, 0.0, false), Some(1));
        assert_eq!(g.row_at(10.0, 0.0, false), None);
        assert_eq!(g.row_at(10.0, 0.0, true), Some(0));
        assert_eq!(g.col_at(60.0, 0.0, false), Some(0));
        assert_eq!(g.col_at(160.0, 0.0, false), Some(1));
        assert_eq!(g.col_at(160.0, 100.0, false), Some(2));
        assert_eq!(g.col_at(1000.0, 0.0, false), None);
        assert_eq!(g.col_at(1000.0, 0.0, true), Some(4));
        let r = g.cell_rect(1, 1, 0.0, 0.0);
        assert_eq!(r, Rect::from_min_max(pos2(150.0, 40.0), pos2(250.0, 60.0)));
        assert_eq!(g.page_rows(), 13);
        assert!(g.max_scroll_y() > 19_000_000.0);
        assert_eq!(g.max_scroll_x(), 160.0);
        let empty = geo(0, &[]);
        assert_eq!(empty.visible_rows(0.0), 0..0);
        assert_eq!(empty.visible_cols(0.0), 0..0);
        assert_eq!(empty.col_at(10.0, 0.0, true), None);
    }
}
