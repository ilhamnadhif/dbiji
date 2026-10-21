//! Record view: one row transposed (column | value), editable (FR-D5).

use crate::format::PreparedFormat;
use crate::grid::{DataGridState, GridAction, GridOptions, ResultViewKind};
use crate::model::{CellRef, ResultModel, RowAnchor, RowState};
use crate::theme::GridTheme;
use dbiji_core::{Value, tr, trf};
use egui::{Button, Key, RichText, ScrollArea, TextEdit, Ui};
use egui_phosphor::regular as icon;

/// A value styled by kind: NULL / DEFAULT in italics, booleans with a
/// check box, JSON in monospace, modified values highlighted.
fn value_text(v: &Value, is_default: bool, modified: bool, fmt: &PreparedFormat, options: &GridOptions, theme: &GridTheme) -> RichText {
    let rt = if is_default {
        RichText::new(tr("[DEFAULT]")).italics().color(theme.weak)
    } else if v.is_null() {
        RichText::new(options.format.null_text.clone()).italics().color(theme.weak)
    } else {
        match v {
            // A check box only (its state is the value).
            Value::Bool(true) => RichText::new(icon::CHECK_SQUARE).size(17.0).color(theme.bool_true),
            Value::Bool(false) => RichText::new(icon::SQUARE).size(17.0).color(theme.weak),
            Value::Json(s) => RichText::new(s.to_string()).monospace().color(theme.text),
            Value::Text(s) => RichText::new(clip(s)).color(theme.text),
            other => RichText::new(clip(&fmt.format(other))).color(theme.text),
        }
    };
    if modified { rt.background_color(theme.modified) } else { rt }
}

/// The draggable right border of a header cell: drag to resize, double-click
/// to fit (`fit`). Returns true when `width` changed.
/// The keyboard focus goes back to `focus` (the view) afterwards.
fn resize_handle(ui: &mut Ui, cell: egui::Rect, id: egui::Id, label: &str, focus: egui::Id, width: &mut f32, fit: impl FnOnce(&Ui) -> f32) -> bool {
    let handle = egui::Rect::from_x_y_ranges(cell.right() - 4.0..=cell.right() + 4.0, cell.y_range());
    let resp = ui.interact(handle, id, egui::Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, trf("Resize {}", &[&label])));
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let before = *width;
    if resp.double_clicked() {
        *width = fit(ui);
    } else if resp.dragged() {
        *width += resp.drag_delta().x;
    }
    *width = width.clamp(60.0, 1600.0);
    if resp.clicked() || resp.double_clicked() || resp.drag_stopped() {
        ui.memory_mut(|m| m.request_focus(focus));
    }
    (*width - before).abs() > f32::EPSILON
}

/// A boolean column (or value): edited with a check box, never typed.
fn is_bool_column(model: &ResultModel, c: usize, v: &Value) -> bool {
    matches!(v, Value::Bool(_)) || model.column_kind(c) == dbiji_core::DataKind::Bool
}

/// One line of text in `rect` (left, vertically centred, cut with "…").
fn paint_text(ui: &Ui, rect: egui::Rect, text: RichText, fallback: egui::Color32) {
    let galley = egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Truncate), rect.width(), egui::TextStyle::Body);
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    ui.painter_at(rect).galley(pos, galley, fallback);
}

/// Most rows shown side by side.
const MAX_ROWS: usize = 50;

/// Width key of the value column when one row is shown.
const SINGLE: usize = usize::MAX;

/// Long values are cut (the value panel shows them in full).
fn clip(s: &str) -> String {
    if s.chars().count() > 2000 { s.chars().take(2000).collect::<String>() + "…" } else { s.to_string() }
}

/// State of the record view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordViewState {
    /// Filter on column names.
    pub column_filter: String,
    /// Row, column, text being typed, the text it started with.
    editing: Option<(usize, usize, String, String)>,
    /// The record being edited: the editor closes when the row shows another one.
    editing_anchor: Option<RowAnchor>,
    error: Option<String>,
    /// Last shown values: ((generation, row, columns), values) for frames where the store is busy.
    cache: Option<RecordCache>,
    /// Take the keyboard focus on the next frame (opened with Tab).
    focus_request: bool,
    /// Cell picked in the table: (row, column).
    picked: Option<(usize, usize)>,
    /// Widths set by the user: per row (side by side), of the single row's
    /// value column ([`SINGLE`]), and of the column-name column.
    widths: std::collections::BTreeMap<usize, f32>,
    lead_width: Option<f32>,
    /// "Hide empty" set here (else the options').
    hide_empty: Option<bool>,
    /// Columns hidden as empty at the last frame.
    hidden_empty: usize,
}

/// NULL or empty text (hidden by "Hide empty").
fn is_empty_value(v: &Value) -> bool {
    v.is_null() || matches!(v, Value::Text(s) if s.is_empty())
}

/// The "Hide empty" toggle; true when it changed.
fn hide_empty_toggle(ui: &mut Ui, state: &mut RecordViewState, options: &GridOptions, tip: &str) -> bool {
    let mut hide = state.hide_empty.unwrap_or(options.record_hide_empty);
    let changed = ui.checkbox(&mut hide, tr("Hide empty")).on_hover_text(tip).changed();
    if changed {
        state.hide_empty = Some(hide);
    }
    if hide && state.hidden_empty > 0 {
        ui.label(RichText::new(trf("{} hidden", &[&state.hidden_empty])).small().weak());
    }
    changed
}

impl RecordViewState {
    fn prefs(&self, options: &GridOptions) -> GridAction {
        GridAction::RecordPrefs { hide_empty: self.hide_empty.unwrap_or(options.record_hide_empty) }
    }
}

impl RecordViewState {
    /// Take the keyboard focus on the next frame: Tab goes back to the
    /// grid, ← / → show the previous / next row.
    pub fn request_focus(&mut self) {
        self.focus_request = true;
    }
}

/// A shown value: (value, is default, modified).
type Shown = (Value, bool, bool);

type RecordCache = ((u64, usize, Vec<usize>), Vec<Shown>);

/// Single record view. The shown row is the grid's active row.
pub struct RecordView<'a> {
    model: &'a mut ResultModel,
    grid: &'a mut DataGridState,
    options: &'a GridOptions,
    state: &'a mut RecordViewState,
}

impl<'a> RecordView<'a> {
    /// New record view.
    pub fn new(model: &'a mut ResultModel, grid: &'a mut DataGridState, options: &'a GridOptions, state: &'a mut RecordViewState) -> Self {
        RecordView { model, grid, options, state }
    }

    /// Show the view.
    pub fn show(self, ui: &mut Ui) -> Vec<GridAction> {
        let RecordView { model, grid, options, state } = self;
        let mut actions = Vec::new();
        model.try_sync();
        grid.sync_columns(model);
        if let Some((r, ..)) = &state.editing
            && state.editing_anchor.and_then(|a| model.anchored_row(a, *r)) != Some(*r)
        {
            state.editing = None;
        }
        let nrows = model.row_count();
        if nrows == 0 {
            ui.weak(tr("No rows"));
            return actions;
        }
        // Several selected rows: side by side, one column per row (DBeaver).
        let selected = grid.selection().rows(nrows, MAX_ROWS + 1);
        if selected.len() > 1 {
            return show_rows(model, grid, options, state, ui, selected);
        }
        let (row, active_col) = grid.active_cell().unwrap_or((0, grid.visible_columns().first().copied().unwrap_or(0)));
        let row = row.min(nrows - 1);
        let mut go_to: Option<usize> = None;
        // Keyboard focus of the view (fields being edited have their own).
        let focus = ui.interact(ui.max_rect(), ui.id().with("record_focus"), egui::Sense::click());
        if std::mem::take(&mut state.focus_request) || focus.clicked() {
            focus.request_focus();
        }
        if focus.has_focus() && state.editing.is_none() {
            ui.memory_mut(|m| m.set_focus_lock_filter(focus.id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false }));
            ui.input(|i| {
                if i.key_pressed(Key::Tab) {
                    actions.push(GridAction::SwitchView(ResultViewKind::Grid));
                } else if i.key_pressed(Key::ArrowRight) && row + 1 < nrows {
                    go_to = Some(row + 1);
                } else if i.key_pressed(Key::ArrowLeft) && row > 0 {
                    go_to = Some(row - 1);
                }
            });
        }
        focus.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, trf("Record view, row {} of {}", &[&(row + 1), &nrows])));
        let total_columns = grid.visible_columns().len();
        egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui.add_enabled(row > 0, Button::new(dbiji_icons::ic(icon::CARET_DOUBLE_LEFT))).on_hover_text(tr("First row")).clicked() {
                    go_to = Some(0);
                }
                if ui.add_enabled(row > 0, Button::new(dbiji_icons::ic(icon::CARET_LEFT))).on_hover_text(tr("Previous row (←)")).clicked() {
                    go_to = Some(row - 1);
                }
                ui.label(tr("Row"));
                let mut n = row + 1;
                if ui.add(egui::DragValue::new(&mut n).range(1..=nrows).speed(0.25)).on_hover_text(tr("Type or drag to go to a row")).changed() {
                    go_to = Some(n.clamp(1, nrows) - 1);
                }
                ui.label(RichText::new(trf("of {}", &[&crate::format::group_thousands(&nrows.to_string())])).weak());
                if ui.add_enabled(row + 1 < nrows, Button::new(dbiji_icons::ic(icon::CARET_RIGHT))).on_hover_text(tr("Next row (→)")).clicked() {
                    go_to = Some(row + 1);
                }
                if ui.add_enabled(row + 1 < nrows, Button::new(dbiji_icons::ic(icon::CARET_DOUBLE_RIGHT))).on_hover_text(tr("Last row")).clicked() {
                    go_to = Some(nrows - 1);
                }
                let state_label = match model.row_state(row) {
                    RowState::Inserted => Some(tr("new row")),
                    RowState::Deleted => Some(tr("marked for deletion")),
                    RowState::Modified => Some(tr("modified")),
                    RowState::Unchanged => None,
                };
                if let Some(s) = state_label {
                    let warn = ui.visuals().warn_fg_color;
                    egui::Frame::new().fill(warn.gamma_multiply(0.15)).corner_radius(8).inner_margin(egui::Margin::symmetric(8, 1)).show(ui, |ui| {
                        ui.label(RichText::new(s).small().color(warn));
                    });
                }
                ui.add_space(12.0);
                ui.add(TextEdit::singleline(&mut state.column_filter).hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr("Filter columns"))).desired_width(220.0));
                ui.add_space(8.0);
                if hide_empty_toggle(ui, state, options, tr("Hide the columns that are NULL or empty in this row")) {
                    actions.push(state.prefs(options));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::KEYBOARD, tr("Tab: grid · ← →: rows · ⌘C: copy"))).small().weak())
                        .on_hover_text(tr("Tab: back to the grid · ← →: previous / next row · ⌘C: copy the picked value"));
                    if !state.column_filter.trim().is_empty() {
                        let needle = state.column_filter.to_lowercase();
                        let shown = grid.visible_columns().into_iter().filter(|&c| model.column_name(c).to_lowercase().contains(&needle)).count();
                        ui.label(RichText::new(trf("{} of {} columns", &[&shown, &total_columns])).small().weak());
                    }
                });
            });
        });
        // Keyboard: Page Up / Page Down move between rows when no field is edited.
        if state.editing.is_none() && go_to.is_none() && ui.ui_contains_pointer() {
            ui.input(|i| {
                if i.key_pressed(Key::PageDown) && row + 1 < nrows {
                    go_to = Some(row + 1);
                } else if i.key_pressed(Key::PageUp) && row > 0 {
                    go_to = Some(row - 1);
                }
            });
        }
        if let Some(r) = go_to {
            grid.select_cell(r, active_col);
            state.editing = None;
            state.error = None;
        }
        let row = go_to.unwrap_or(row);
        rows_table(model, grid, options, state, ui, &[row], &focus, &mut actions);
        actions
    }
}

/// Values of `rows` for `columns`: (value, is default, modified); `false`
/// when the store was busy (shown values may be incomplete).
fn row_values(model: &ResultModel, rows: &[usize], columns: &[usize]) -> (Vec<Vec<Shown>>, bool) {
    let mut all = Vec::with_capacity(rows.len());
    let mut fresh = true;
    for &r in rows {
        let mut values = Vec::with_capacity(columns.len());
        fresh &= model.visit_rows_for(r..r + 1, std::time::Duration::from_millis(2), |_, cells| {
            for &c in columns {
                let (v, is_default) = if model.is_virtual_column(c) {
                    (cells.value_of(c).unwrap_or_default(), false)
                } else {
                    match cells.get(c) {
                        CellRef::Value(v) => (v.clone(), false),
                        CellRef::Default => (Value::Null, true),
                        CellRef::Missing => (Value::Null, false),
                    }
                };
                values.push((v, is_default, cells.is_modified(c)));
            }
        });
        values.resize(columns.len(), (Value::Null, false, false));
        all.push(values);
    }
    (all, fresh)
}

/// Selected rows side by side: the header ("3 selected rows", the column
/// filter, Hide empty) above the table of [`rows_table`].
fn show_rows(model: &mut ResultModel, grid: &mut DataGridState, options: &GridOptions, state: &mut RecordViewState, ui: &mut Ui, mut rows: Vec<usize>) -> Vec<GridAction> {
    let mut actions = Vec::new();
    let more = rows.len() > MAX_ROWS;
    rows.truncate(MAX_ROWS);
    // Keyboard: Tab back to the grid (⌘C: see the table).
    let focus = ui.interact(ui.max_rect(), ui.id().with("record_focus"), egui::Sense::click());
    if std::mem::take(&mut state.focus_request) || focus.clicked() {
        focus.request_focus();
    }
    focus.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, trf("Record view, {} rows", &[&rows.len()])));
    if focus.has_focus() && state.editing.is_none() {
        ui.memory_mut(|m| m.set_focus_lock_filter(focus.id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false }));
        if ui.input(|i| i.key_pressed(Key::Tab)) {
            actions.push(GridAction::SwitchView(ResultViewKind::Grid));
        }
    }
    let total_columns = grid.visible_columns().len();
    egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 4)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  {}", icon::ROWS, trf("{} selected rows", &[&rows.len()]))).strong());
            if more {
                ui.label(RichText::new(trf("(the first {})", &[&MAX_ROWS])).weak());
            }
            ui.add_space(12.0);
            ui.add(TextEdit::singleline(&mut state.column_filter).hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr("Filter columns"))).desired_width(220.0));
            ui.add_space(8.0);
            if hide_empty_toggle(ui, state, options, tr("Hide the columns that are NULL or empty in every row")) {
                actions.push(state.prefs(options));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{}  {}", icon::KEYBOARD, tr("Tab: grid · ⌘C: copy"))).small().weak()).on_hover_text(tr("Select one row to see it as a record"));
                let needle = state.column_filter.trim().to_lowercase();
                if !needle.is_empty() {
                    let shown = grid.visible_columns().into_iter().filter(|&c| model.column_name(c).to_lowercase().contains(&needle)).count();
                    ui.label(RichText::new(trf("{} of {} columns", &[&shown, &total_columns])).small().weak());
                }
            });
        });
    });
    rows_table(model, grid, options, state, ui, &rows, &focus, &mut actions);
    actions
}

/// The columns of `rows` as a table: the column names down the left (the
/// type on hover, key columns pinned at the top), one value column per row
/// ("Row #6" …) whose borders are dragged to resize and double-clicked to
/// fit. A click picks a value (⌘C copies it), a double-click edits it,
/// booleans have a check box. One row (the record view) or several side by side.
#[allow(clippy::too_many_arguments)]
fn rows_table(
    model: &mut ResultModel,
    grid: &mut DataGridState,
    options: &GridOptions,
    state: &mut RecordViewState,
    ui: &mut Ui,
    rows: &[usize],
    focus: &egui::Response,
    actions: &mut Vec<GridAction>,
) {
    let theme = GridTheme::from_visuals(ui.visuals());
    let single = rows.len() == 1;
    let needle = state.column_filter.to_lowercase();
    let named: Vec<usize> = grid.visible_columns().into_iter().filter(|&c| needle.is_empty() || model.column_name(c).to_lowercase().contains(&needle)).collect();
    let (all_values, fresh) = row_values(model, rows, &named);
    if !fresh {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(10));
    }
    // "Hide empty": columns NULL or empty in every row go (key columns stay).
    let key_cols: Vec<usize> = model.editability().as_ref().map(|t| t.key_columns.clone()).unwrap_or_default();
    let hide_empty = state.hide_empty.unwrap_or(options.record_hide_empty);
    let all_empty = |i: usize| all_values.iter().all(|vs| vs.get(i).is_some_and(|(v, d, _)| !*d && is_empty_value(v)));
    let keep: Vec<usize> = (0..named.len()).filter(|&i| !hide_empty || key_cols.contains(&named[i]) || !all_empty(i)).collect();
    state.hidden_empty = named.len() - keep.len();
    let columns: Vec<usize> = keep.iter().map(|&i| named[i]).collect();
    let values: Vec<Vec<Shown>> = all_values.iter().map(|vs| keep.iter().filter_map(|&i| vs.get(i).cloned()).collect()).collect();
    let text_of = |r: usize, i: usize| values.get(r).and_then(|vs| vs.get(i)).map(|(v, ..)| if v.is_null() { String::new() } else { v.to_text().into_owned() });
    if focus.has_focus() && state.editing.is_none() {
        let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy
            && let Some((r, c)) = state.picked
            && let (Some(ri), Some(ci)) = (rows.iter().position(|x| *x == r), columns.iter().position(|x| *x == c))
            && let Some(t) = text_of(ri, ci)
        {
            ui.ctx().copy_text(t);
            actions.push(GridAction::Notify { message: tr("Value copied").to_string(), error: false });
        }
    }
    let body = egui::TextStyle::Body.resolve(ui.style());
    let measure = |ui: &Ui, text: String, font: egui::FontId| ui.fonts_mut(|f| f.layout_no_wrap(text, font, egui::Color32::WHITE).rect.width());
    let name_w = columns.iter().map(|&c| measure(ui, model.column_name(c).to_string(), body.clone())).fold(0.0_f32, f32::max).clamp(100.0, 260.0) + 8.0;
    let type_of = |c: usize| model.column_meta(c).map_or_else(|| tr("virtual").to_string(), |m| m.type_name.to_lowercase());
    let fit_lead = 24.0 + name_w;
    let lead = state.lead_width.unwrap_or(fit_lead);
    // One row: its value column takes the width; side by side: each a share.
    let width_key = |r: usize| if single { SINGLE } else { r };
    let default_w = if single { (ui.available_width() - lead - 16.0).max(200.0) } else { ((ui.available_width() - lead - 16.0) / rows.len() as f32).clamp(150.0, 360.0) };
    let mut widths: Vec<f32> = rows.iter().map(|r| state.widths.get(&width_key(*r)).copied().unwrap_or(default_w)).collect();
    let mut lead_w = lead;
    // Width that shows the longest value of row `j` (double-click on the border).
    let fit_row = |ui: &Ui, j: usize, r: usize| -> f32 {
        let mut w = measure(ui, trf("Row #{}", &[&(r + 1)]), body.clone());
        for (i, &c) in columns.iter().enumerate() {
            if let Some((v, is_default, modified)) = values.get(j).and_then(|vs| vs.get(i)) {
                let fmt = PreparedFormat::new(&options.format, grid.column_format(c));
                let rt = value_text(v, *is_default, *modified, &fmt, options, &theme);
                let g = egui::WidgetText::from(rt).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body);
                w = w.max(g.size().x);
            }
        }
        w + 20.0
    };
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 10.0;
    let active_row = grid.active_cell().map(|(r, _)| r);
    let editable = model.is_editable() && options.allow_editing;
    let mut commit: Option<(usize, usize, String)> = None;
    let mut open_value: Option<(usize, usize)> = None;
    let mut picked = state.picked;
    let mut toggled: Option<(usize, usize, bool)> = None;
    let mut select: Option<(usize, usize)> = None;
    ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            // Header: one column per row.
            // Borders are resized by dragging, fitted by a double-click.
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(lead_w, row_h), egui::Sense::hover());
                ui.painter().rect_filled(rect, 0.0, theme.header_bg);
                ui.painter().text(rect.left_center() + egui::vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, tr("Column"), body.clone(), theme.weak);
                if resize_handle(ui, rect, ui.id().with("record_lead"), tr("Column"), focus.id, &mut lead_w, |_| fit_lead) {
                    state.lead_width = Some(lead_w);
                }
                for (j, &r) in rows.iter().enumerate() {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(widths[j], row_h), egui::Sense::hover());
                    let fill = if active_row == Some(r) && !single { theme.header_selected_bg } else { theme.header_bg };
                    ui.painter().rect_filled(rect, 0.0, fill);
                    ui.painter().vline(rect.left(), rect.y_range(), egui::Stroke::new(1.0, theme.line));
                    ui.painter().text(rect.left_center() + egui::vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, trf("Row #{}", &[&(r + 1)]), body.clone(), theme.header_text);
                    if resize_handle(ui, rect, ui.id().with(("record_col", width_key(r))), &trf("Row #{}", &[&(r + 1)]), focus.id, &mut widths[j], |ui| fit_row(ui, j, r)) {
                        state.widths.insert(width_key(r), widths[j]);
                    }
                }
            });
            // Key columns stay at the top while the others scroll.
            let (pinned, rest): (Vec<usize>, Vec<usize>) = (0..columns.len()).partition(|&i| key_cols.contains(&columns[i]));
            let mut draw = |ui: &mut Ui, list: &[usize]| {
                for &i in list {
                    let c = columns[i];
                    ui.horizontal(|ui| {
                        let (lead_rect, lead_resp) = ui.allocate_exact_size(egui::vec2(lead_w, row_h), egui::Sense::hover());
                        let name = model.column_name(c).to_string();
                        lead_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name));
                        // The column's type, and why it has a marker, on hover.
                        let mut tip = type_of(c);
                        if key_cols.contains(&c) {
                            tip = format!("{tip}\n{}", tr("Key column"));
                        } else if model.foreign_key_for_column(c).is_some() {
                            tip = format!("{tip}\n{}", tr("References another table"));
                        }
                        lead_resp.on_hover_text(tip);
                        let stripe = if i % 2 == 1 { theme.stripe } else { theme.bg };
                        let hovered = ui.input(|inp| inp.pointer.hover_pos()).is_some_and(|p| p.y >= lead_rect.top() && p.y < lead_rect.bottom() && ui.clip_rect().contains(p));
                        let row_fill = if hovered { crate::theme::mix(stripe, theme.text, 0.06) } else { stripe };
                        ui.painter().rect_filled(lead_rect, 0.0, row_fill);
                        let p = ui.painter();
                        let mut x = lead_rect.left() + 6.0;
                        if key_cols.contains(&c) {
                            p.text(egui::pos2(x, lead_rect.center().y), egui::Align2::LEFT_CENTER, icon::KEY, body.clone(), theme.warn);
                        } else if model.foreign_key_for_column(c).is_some() {
                            p.text(egui::pos2(x, lead_rect.center().y), egui::Align2::LEFT_CENTER, icon::LINK_SIMPLE, body.clone(), theme.link);
                        }
                        x += 18.0;
                        let name_rect = egui::Rect::from_min_max(egui::pos2(x, lead_rect.top()), egui::pos2(lead_rect.right() - 6.0, lead_rect.bottom()));
                        let mut name_text = RichText::new(&name).color(theme.weak);
                        if model.is_virtual_column(c) {
                            name_text = name_text.italics();
                        }
                        paint_text(ui, name_rect, name_text, theme.text);
                        for (j, &r) in rows.iter().enumerate() {
                            if state.editing.as_ref().is_some_and(|(er, ec, _, _)| *er == r && *ec == c) {
                                if let Some((_, _, text, original)) = state.editing.as_mut() {
                                    let te = ui.add_sized([widths[j], row_h], TextEdit::singleline(text));
                                    if te.lost_focus() {
                                        // Unchanged text is no edit (a NULL would become '').
                                        if ui.input(|inp| inp.key_pressed(Key::Escape)) || text == original {
                                            state.editing = None;
                                        } else {
                                            commit = Some((r, c, text.clone()));
                                        }
                                    } else if !te.has_focus() {
                                        te.request_focus();
                                    }
                                }
                                continue;
                            }
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(widths[j], row_h), egui::Sense::click());
                            let is_picked = picked == Some((r, c));
                            let fill = if is_picked { theme.selection } else { row_fill };
                            ui.painter().rect_filled(rect, 0.0, fill);
                            ui.painter().vline(rect.left(), rect.y_range(), egui::Stroke::new(1.0, theme.line));
                            if is_picked {
                                ui.painter().rect_stroke(rect.shrink(1.0), 2.0, egui::Stroke::new(1.5, theme.active_border), egui::StrokeKind::Inside);
                            }
                            let inner = rect.shrink2(egui::vec2(8.0, 0.0));
                            let Some((v, is_default, modified)) = values.get(j).and_then(|vs| vs.get(i)) else { continue };
                            let fmt = PreparedFormat::new(&options.format, grid.column_format(c));
                            let rt = value_text(v, *is_default, *modified, &fmt, options, &theme);
                            paint_text(ui, inner, rt, theme.text);
                            let boolean = is_bool_column(model, c, v) && !*is_default;
                            let can_edit = editable && model.can_edit_cell(r, c).is_ok();
                            // Booleans: the box is a check box of its own (a click elsewhere picks the cell).
                            if boolean {
                                let on = v.as_bool().unwrap_or(false);
                                let label = if v.is_null() {
                                    options.format.null_text.clone()
                                } else if on {
                                    "true".into()
                                } else {
                                    "false".into()
                                };
                                let box_rect = egui::Rect::from_min_size(egui::pos2(inner.left() - 2.0, rect.top()), egui::vec2(24.0, rect.height()));
                                let box_resp = ui.interact(box_rect, ui.id().with(("record_bool", r, c)), egui::Sense::click());
                                box_resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, can_edit, on, &label));
                                let box_resp = if can_edit { box_resp.on_hover_text(tr("Click the box to change the value")) } else { box_resp };
                                if box_resp.clicked() {
                                    picked = Some((r, c));
                                    select = Some((r, c));
                                    focus.request_focus();
                                    if can_edit {
                                        toggled = Some((r, c, !on));
                                    }
                                }
                            }
                            let full = if v.is_null() { None } else { Some(v.to_text().into_owned()) };
                            let resp = match full.filter(|t| t.chars().count() > 40 || t.contains('\n')) {
                                Some(t) => resp.on_hover_text(clip(&t)),
                                None => resp,
                            };
                            let resp = if can_edit && !boolean { resp.on_hover_text(tr("Double-click to edit")) } else { resp };
                            if resp.clicked() {
                                picked = Some((r, c));
                                select = Some((r, c));
                                focus.request_focus();
                            }
                            if resp.double_clicked() && !boolean {
                                let multi = matches!(v, Value::Text(s) if s.contains('\n')) || matches!(v, Value::Bytes(_));
                                if can_edit && !multi {
                                    let text = if v.is_null() { String::new() } else { v.to_text().into_owned() };
                                    state.editing = Some((r, c, text.clone(), text));
                                    state.editing_anchor = model.row_anchor(r);
                                    state.error = None;
                                } else {
                                    open_value = Some((r, c));
                                }
                            }
                        }
                    });
                }
            };
            if !pinned.is_empty() && !rest.is_empty() {
                draw(ui, &pinned);
                // A line under the pinned key columns, as wide as the table.
                let (y, left) = (ui.cursor().top(), ui.max_rect().left());
                let right = left + lead_w + widths.iter().sum::<f32>();
                ui.painter().hline(left..=right, y, egui::Stroke::new(1.5, theme.weak.gamma_multiply(0.6)));
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| draw(ui, &rest));
            } else {
                let all: Vec<usize> = (0..columns.len()).collect();
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| draw(ui, &all));
            }
        });
    });
    state.picked = picked;
    // One row: the grid's active cell follows (side by side it would drop the other rows).
    if let (true, Some((r, c))) = (single, select) {
        grid.select_cell(r, c);
    }
    if let Some((r, c, on)) = toggled {
        match model.set_value(r, c, Value::Bool(on)) {
            Ok(()) => state.error = None,
            Err(e) => state.error = Some(e),
        }
    }
    if let Some(e) = &state.error {
        ui.colored_label(theme.error, e);
    }
    if let Some((r, c, text)) = commit {
        match model.set_cell(r, c, &text) {
            Ok(()) => {
                state.editing = None;
                state.error = None;
            }
            Err(e) => state.error = Some(e),
        }
    }
    if let Some((r, c)) = open_value {
        actions.push(GridAction::OpenValuePanel { row: r, col: c });
    }
}
