//! Record view: one row transposed (column | value), editable (FR-D5).

use crate::format::PreparedFormat;
use crate::grid::{DataGridState, GridAction, GridOptions, ResultViewKind};
use crate::model::{CellRef, ResultModel, RowState};
use crate::theme::GridTheme;
use dbiji_core::{Value, tr, trf};
use egui::{Button, Key, RichText, ScrollArea, TextEdit, Ui};
use egui_phosphor::regular as icon;

/// State of the record view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordViewState {
    /// Filter on column names.
    pub column_filter: String,
    editing: Option<(usize, usize, String)>,
    error: Option<String>,
    /// Last shown values: ((generation, row, columns), values) for frames where the store is busy.
    cache: Option<RecordCache>,
    /// Take the keyboard focus on the next frame (opened with Tab).
    focus_request: bool,
}

impl RecordViewState {
    /// Take the keyboard focus on the next frame: Tab goes back to the
    /// grid, ← / → show the previous / next row.
    pub fn request_focus(&mut self) {
        self.focus_request = true;
    }
}

type RecordCache = ((u64, usize, Vec<usize>), Vec<(Value, bool, bool)>);

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
        let nrows = model.row_count();
        if nrows == 0 {
            ui.weak(tr("No rows"));
            return actions;
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
        ui.horizontal(|ui| {
            if ui.add_enabled(row > 0, Button::new(icon::CARET_DOUBLE_LEFT)).on_hover_text(tr("First row")).clicked() {
                go_to = Some(0);
            }
            if ui.add_enabled(row > 0, Button::new(icon::CARET_LEFT)).on_hover_text(tr("Previous row")).clicked() {
                go_to = Some(row - 1);
            }
            ui.label(trf("Row {} of {}", &[&(row + 1), &crate::format::group_thousands(&nrows.to_string())]));
            if ui.add_enabled(row + 1 < nrows, Button::new(icon::CARET_RIGHT)).on_hover_text(tr("Next row")).clicked() {
                go_to = Some(row + 1);
            }
            if ui.add_enabled(row + 1 < nrows, Button::new(icon::CARET_DOUBLE_RIGHT)).on_hover_text(tr("Last row")).clicked() {
                go_to = Some(nrows - 1);
            }
            let state_label = match model.row_state(row) {
                RowState::Inserted => Some(tr("new row")),
                RowState::Deleted => Some(tr("marked for deletion")),
                RowState::Modified => Some(tr("modified")),
                RowState::Unchanged => None,
            };
            if let Some(s) = state_label {
                ui.colored_label(ui.visuals().warn_fg_color, s);
            }
            ui.add(TextEdit::singleline(&mut state.column_filter).hint_text(tr("Filter columns")).desired_width(160.0));
            ui.weak(tr("Tab: back to the grid · ← →: previous / next row"));
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
        ui.separator();
        let theme = GridTheme::from_visuals(ui.visuals());
        let needle = state.column_filter.to_lowercase();
        let columns: Vec<usize> = grid.visible_columns().into_iter().filter(|&c| needle.is_empty() || model.column_name(c).to_lowercase().contains(&needle)).collect();
        let mut values: Vec<(Value, bool, bool)> = Vec::with_capacity(columns.len());
        let key = (model.generation(), row, columns.clone());
        let fresh = model.visit_rows_for(row..row + 1, std::time::Duration::from_millis(4), |_, cells| {
            for &c in &columns {
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
        if fresh {
            state.cache = Some((key, values.clone()));
        } else {
            // The fetch task holds the store: show the last values and retry soon.
            if let Some((_, cached)) = state.cache.as_ref().filter(|(k, _)| k.1 == row && k.2 == columns) {
                values = cached.clone();
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(10));
        }
        let editable = model.is_editable() && options.allow_editing;
        let mut commit: Option<(usize, usize, String)> = None;
        let mut open_value: Option<usize> = None;
        // Column names in a fixed column, values wrapped over the rest of the width.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let name_w = columns
            .iter()
            .map(|&c| ui.fonts_mut(|f| f.layout_no_wrap(model.column_name(c).to_string(), font.clone(), egui::Color32::WHITE).rect.width()))
            .fold(0.0_f32, f32::max)
            .clamp(100.0, 280.0)
            + 12.0;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, &c) in columns.iter().enumerate() {
                let Some((v, is_default, modified)) = values.get(i) else { continue };
                let fill = if i % 2 == 1 { theme.stripe } else { egui::Color32::TRANSPARENT };
                egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(6, 3)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        let meta = model.column_meta(c);
                        let name = model.column_name(c);
                        let mut label = RichText::new(name).strong();
                        if model.is_virtual_column(c) {
                            label = label.italics();
                        }
                        let hint = meta.map_or_else(|| tr("Virtual column").to_string(), |m| m.type_name.clone());
                        ui.allocate_ui_with_layout(egui::vec2(name_w, row_h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                            ui.set_min_width(name_w);
                            ui.add(egui::Label::new(label).truncate()).on_hover_text(hint);
                        });
                        let editing_this = state.editing.as_ref().is_some_and(|(r, col, _)| *r == row && *col == c);
                        if editing_this {
                            if let Some((_, _, text)) = state.editing.as_mut() {
                                let resp = ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY));
                                if resp.lost_focus() {
                                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                                        state.editing = None;
                                    } else {
                                        commit = Some((row, c, text.clone()));
                                    }
                                } else if !resp.has_focus() {
                                    resp.request_focus();
                                }
                            }
                            return;
                        }
                        let fmt = PreparedFormat::new(&options.format, grid.column_format(c));
                        let (text, color) = if *is_default {
                            (tr("[DEFAULT]").to_string(), theme.weak)
                        } else if v.is_null() {
                            (options.format.null_text.clone(), theme.weak)
                        } else {
                            let t = match v {
                                Value::Text(s) | Value::Json(s) => s.to_string(),
                                other => fmt.format(other).into_owned(),
                            };
                            let t = if t.chars().count() > 2000 { t.chars().take(2000).collect::<String>() + "…" } else { t };
                            (t, theme.text)
                        };
                        let mut rt = RichText::new(text).color(color);
                        if *modified {
                            rt = rt.background_color(theme.modified);
                        }
                        if grid.active_cell().is_some_and(|(_, ac)| ac == c) {
                            rt = rt.underline();
                        }
                        let can_edit = editable && model.can_edit_cell(row, c).is_ok();
                        let resp = ui.add(egui::Label::new(rt).wrap().sense(egui::Sense::click()));
                        let resp = if can_edit { resp.on_hover_text(tr("Double-click to edit")) } else { resp };
                        if resp.double_clicked() {
                            let multi = matches!(v, Value::Text(s) if s.contains('\n')) || matches!(v, Value::Bytes(_));
                            if can_edit && !multi {
                                state.editing = Some((row, c, if v.is_null() { String::new() } else { v.to_text().into_owned() }));
                                state.error = None;
                            } else {
                                open_value = Some(c);
                            }
                        }
                        if resp.clicked() {
                            grid.select_cell(row, c);
                        }
                    });
                });
            }
        });
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
        if let Some(c) = open_value {
            actions.push(GridAction::OpenValuePanel { row, col: c });
        }
        actions
    }
}
