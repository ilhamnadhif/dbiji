//! Record view: one row transposed (column | value), editable (FR-D5).

use crate::format::PreparedFormat;
use crate::grid::{DataGridState, GridAction, GridOptions, ResultViewKind};
use crate::model::{CellRef, ResultModel, RowState};
use crate::theme::GridTheme;
use dbiji_core::{Value, tr, trf};
use egui::{Button, Key, RichText, ScrollArea, TextEdit, Ui};
use egui_phosphor::regular as icon;

/// Long values are cut (the value panel shows them in full).
fn clip(s: &str) -> String {
    if s.chars().count() > 2000 { s.chars().take(2000).collect::<String>() + "…" } else { s.to_string() }
}

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
        focus.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, trf("Record view, row {} of {}", &[&(row + 1), &nrows])));
        let total_columns = grid.visible_columns().len();
        egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui.add_enabled(row > 0, Button::new(icon::CARET_DOUBLE_LEFT)).on_hover_text(tr("First row")).clicked() {
                    go_to = Some(0);
                }
                if ui.add_enabled(row > 0, Button::new(icon::CARET_LEFT)).on_hover_text(tr("Previous row (←)")).clicked() {
                    go_to = Some(row - 1);
                }
                ui.label(tr("Row"));
                let mut n = row + 1;
                if ui.add(egui::DragValue::new(&mut n).range(1..=nrows).speed(0.25)).on_hover_text(tr("Type or drag to go to a row")).changed() {
                    go_to = Some(n.clamp(1, nrows) - 1);
                }
                ui.label(RichText::new(trf("of {}", &[&crate::format::group_thousands(&nrows.to_string())])).weak());
                if ui.add_enabled(row + 1 < nrows, Button::new(icon::CARET_RIGHT)).on_hover_text(tr("Next row (→)")).clicked() {
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
                    let warn = ui.visuals().warn_fg_color;
                    egui::Frame::new().fill(warn.gamma_multiply(0.15)).corner_radius(8).inner_margin(egui::Margin::symmetric(8, 1)).show(ui, |ui| {
                        ui.label(RichText::new(s).small().color(warn));
                    });
                }
                ui.add_space(12.0);
                ui.add(TextEdit::singleline(&mut state.column_filter).hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr("Filter columns"))).desired_width(220.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::KEYBOARD, tr("Tab: grid · ← →: rows"))).small().weak())
                        .on_hover_text(tr("Tab: back to the grid · ← →: previous / next row"));
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
        // One line per column: key / link icon, name (secondary), type
        // (small), the value wrapped over the rest of the width, copy.
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::TextStyle::Small.resolve(ui.style());
        let measure = |ui: &Ui, text: String, font: egui::FontId| ui.fonts_mut(|f| f.layout_no_wrap(text, font, egui::Color32::WHITE).rect.width());
        let name_w = columns.iter().map(|&c| measure(ui, model.column_name(c).to_string(), body.clone())).fold(0.0_f32, f32::max).clamp(100.0, 300.0) + 8.0;
        let type_of = |c: usize| model.column_meta(c).map_or_else(|| tr("virtual").to_string(), |m| m.type_name.to_lowercase());
        let mono_small = egui::FontId::monospace(small.size);
        let type_w = columns.iter().map(|&c| measure(ui, type_of(c), mono_small.clone())).fold(0.0_f32, f32::max).clamp(40.0, 160.0) + 24.0;
        let key_cols: Vec<usize> = model.editability().as_ref().map(|t| t.key_columns.clone()).unwrap_or_default();
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 2.0;
        let mut copied: Option<String> = None;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, &c) in columns.iter().enumerate() {
                let Some((v, is_default, modified)) = values.get(i) else { continue };
                let active = active_col == c;
                let bg = ui.painter().add(egui::Shape::Noop);
                let frame = egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 4, top: 5, bottom: 5 }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        // Key / foreign key marker.
                        let marker = if key_cols.contains(&c) {
                            Some((icon::KEY, theme.warn, tr("Key column")))
                        } else if model.foreign_key_for_column(c).is_some() {
                            Some((icon::LINK_SIMPLE, theme.link, tr("References another table")))
                        } else {
                            None
                        };
                        ui.allocate_ui_with_layout(egui::vec2(16.0, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(16.0);
                            if let Some((ic, color, tip)) = marker {
                                ui.label(RichText::new(ic).color(color)).on_hover_text(tip);
                            }
                        });
                        let mut name = RichText::new(model.column_name(c)).color(if active { theme.text } else { theme.weak });
                        if model.is_virtual_column(c) {
                            name = name.italics();
                        }
                        ui.allocate_ui_with_layout(egui::vec2(name_w, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(name_w);
                            if ui.add(egui::Label::new(name).truncate().sense(egui::Sense::click())).clicked() {
                                grid.select_cell(row, c);
                            }
                        });
                        ui.allocate_ui_with_layout(egui::vec2(type_w, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(type_w);
                            ui.add(egui::Label::new(RichText::new(type_of(c)).small().monospace().color(theme.weak.gamma_multiply(0.8))).truncate());
                        });
                        let value_w = (ui.available_width() - 30.0).max(80.0);
                        ui.allocate_ui_with_layout(egui::vec2(value_w, row_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(value_w);
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
                            let mut rt = if *is_default {
                                RichText::new(tr("[DEFAULT]")).italics().color(theme.weak)
                            } else if v.is_null() {
                                RichText::new(options.format.null_text.clone()).italics().color(theme.weak)
                            } else {
                                match v {
                                    Value::Bool(true) => RichText::new(format!("{}  true", icon::CHECK_SQUARE)).color(theme.bool_true),
                                    Value::Bool(false) => RichText::new(format!("{}  false", icon::SQUARE)).color(theme.text),
                                    Value::Json(s) => RichText::new(s.to_string()).monospace().color(theme.text),
                                    Value::Text(s) => RichText::new(clip(s)).color(theme.text),
                                    other => RichText::new(clip(&fmt.format(other))).color(theme.text),
                                }
                            };
                            if *modified {
                                rt = rt.background_color(theme.modified);
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
                        let copy = ui.add(Button::new(RichText::new(icon::COPY).color(theme.weak)).frame(false)).on_hover_text(tr("Copy the value"));
                        if copy.clicked() {
                            copied = Some(if v.is_null() { String::new() } else { v.to_text().into_owned() });
                        }
                    });
                });
                let rect = frame.response.rect;
                let hovered = ui.rect_contains_pointer(rect);
                let fill = if active {
                    theme.selection
                } else if hovered {
                    crate::theme::mix(theme.bg, theme.text, 0.06)
                } else if i % 2 == 1 {
                    theme.stripe
                } else {
                    egui::Color32::TRANSPARENT
                };
                ui.painter().set(bg, egui::Shape::rect_filled(rect, 4.0, fill));
            }
        });
        if let Some(text) = copied {
            ui.ctx().copy_text(text);
            actions.push(GridAction::Notify { message: tr("Value copied").to_string(), error: false });
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
        if let Some(c) = open_value {
            actions.push(GridAction::OpenValuePanel { row, col: c });
        }
        actions
    }
}
