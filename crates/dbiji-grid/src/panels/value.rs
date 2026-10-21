//! Value panel for the active cell (FR-D5, FR-D7 special value viewers).

use crate::grid::{DataGridState, GridAction, GridOptions};
use crate::model::{ResultModel, RowKey};
use crate::valueview::{array_lines, byte_size_text, decode_image, detect_image, hex_dump, pretty_json, pretty_xml, split_ewkt};
use dbiji_core::{DataKind, Value, tr, trf};
use egui::{CollapsingHeader, RichText, ScrollArea, TextEdit, TextureHandle, TextureOptions, Ui};

/// How a value is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ValueMode {
    /// Pick from the column kind / value.
    #[default]
    Auto,
    /// Plain text editor.
    Text,
    /// JSON pretty printed.
    Json,
    /// JSON tree.
    JsonTree,
    /// XML pretty printed.
    Xml,
    /// Hex dump.
    Hex,
    /// Image preview.
    Image,
}

/// Identity of a cell value: row, model column, model generation.
type CellKey = (RowKey, usize, u64);

/// A decoded image preview: texture, original size and format label.
type Preview = (TextureHandle, [u32; 2], &'static str);

/// State of the value panel.
#[derive(Default)]
pub struct ValuePanelState {
    /// Presentation mode.
    pub mode: ValueMode,
    /// Wrap long lines.
    pub wrap: bool,
    edit_key: Option<CellKey>,
    /// The row store the text was read from: after a refresh a row key (a
    /// store index) names another record, so typed text is not kept.
    edit_store: usize,
    edit_text: String,
    edit_dirty: bool,
    error: Option<String>,
    image_key: Option<CellKey>,
    image: Option<Result<Preview, String>>,
    last_value: Option<(Option<CellKey>, Value)>,
}

impl std::fmt::Debug for ValuePanelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValuePanelState").field("mode", &self.mode).field("wrap", &self.wrap).finish_non_exhaustive()
    }
}

/// Value viewer / editor of the grid's active cell.
pub struct ValuePanel<'a> {
    model: &'a mut ResultModel,
    grid: &'a DataGridState,
    options: &'a GridOptions,
    state: &'a mut ValuePanelState,
}

fn json_tree(ui: &mut Ui, key: &str, v: &serde_json::Value, budget: &mut usize, depth: usize) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    match v {
        serde_json::Value::Object(map) => {
            CollapsingHeader::new(format!("{key} {{{}}}", map.len())).id_salt((key, depth, *budget)).default_open(depth < 2).show(ui, |ui| {
                for (k, child) in map {
                    json_tree(ui, k, child, budget, depth + 1);
                }
            });
        }
        serde_json::Value::Array(items) => {
            CollapsingHeader::new(format!("{key} [{}]", items.len())).id_salt((key, depth, *budget)).default_open(depth < 2).show(ui, |ui| {
                for (i, child) in items.iter().enumerate().take(5000) {
                    json_tree(ui, &format!("[{i}]"), child, budget, depth + 1);
                }
            });
        }
        other => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{key}:")).strong());
                let text = match other {
                    serde_json::Value::String(s) => format!("\"{s}\""),
                    o => o.to_string(),
                };
                ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
            });
        }
    }
}

impl<'a> ValuePanel<'a> {
    /// New value panel.
    pub fn new(model: &'a mut ResultModel, grid: &'a DataGridState, options: &'a GridOptions, state: &'a mut ValuePanelState) -> Self {
        ValuePanel { model, grid, options, state }
    }

    /// Show the panel. Returns actions (e.g. save to file).
    pub fn show(self, ui: &mut Ui) -> Vec<GridAction> {
        let ValuePanel { model, grid, options, state } = self;
        let mut actions = Vec::new();
        let Some((row, col)) = grid.active_cell().filter(|(r, _)| *r < model.row_count()) else {
            ui.weak(tr("Select a cell to see its value"));
            return actions;
        };
        let kind = model.column_kind(col);
        let key = model.row_key(row).map(|k| (k, col, model.generation()));
        // Never wait for the fetch task: reuse the last value of this cell when the store is busy.
        let value = match model.try_cell(row, col, std::time::Duration::from_millis(4)) {
            Some(v) => {
                let v = v.unwrap_or_default();
                state.last_value = Some((key, v.clone()));
                v
            }
            None => {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(10));
                match &state.last_value {
                    Some((k, v)) if *k == key => v.clone(),
                    _ => Value::Null,
                }
            }
        };
        let cell_editable = options.allow_editing && model.can_edit_cell(row, col).is_ok();
        ui.horizontal(|ui| {
            ui.strong(model.column_name(col));
            if let Some(m) = model.column_meta(col) {
                ui.weak(&m.type_name);
            }
            ui.weak(trf("row {}", &[&(row + 1)]));
        });
        // Mode selector with the modes that make sense for the value.
        let mut modes = vec![(ValueMode::Auto, tr("Auto")), (ValueMode::Text, tr("Text"))];
        let is_json = matches!(value, Value::Json(_)) || kind == DataKind::Json;
        if is_json || value.as_str().is_some_and(|s| s.trim_start().starts_with(['{', '['])) {
            modes.push((ValueMode::Json, "JSON"));
            modes.push((ValueMode::JsonTree, tr("JSON tree")));
        }
        if kind == DataKind::Xml || value.as_str().is_some_and(|s| s.trim_start().starts_with('<')) {
            modes.push((ValueMode::Xml, "XML"));
        }
        if let Value::Bytes(b) = &value {
            modes.push((ValueMode::Hex, tr("Hex")));
            if detect_image(b).is_some() {
                modes.push((ValueMode::Image, tr("Image")));
            }
        }
        if !modes.iter().any(|(m, _)| *m == state.mode) {
            state.mode = ValueMode::Auto;
        }
        ui.horizontal(|ui| {
            for (m, label) in &modes {
                ui.selectable_value(&mut state.mode, *m, *label);
            }
            ui.checkbox(&mut state.wrap, tr("Wrap"));
        });
        ui.separator();
        let mode = match state.mode {
            // NULL has nothing to parse or decode: show the (empty) text editor.
            _ if value.is_null() => ValueMode::Text,
            ValueMode::Auto => match &value {
                Value::Bytes(b) if detect_image(b).is_some() => ValueMode::Image,
                Value::Bytes(_) => ValueMode::Hex,
                Value::Json(_) => ValueMode::Json,
                _ if kind == DataKind::Json => ValueMode::Json,
                _ if kind == DataKind::Xml => ValueMode::Xml,
                _ => ValueMode::Text,
            },
            m => m,
        };
        if value.is_null() {
            ui.weak(&options.format.null_text);
        }
        match mode {
            ValueMode::Text | ValueMode::Auto => {
                // New rows or a dictionary change the generation: typed text of the same cell stays.
                let store = std::sync::Arc::as_ptr(model.store()) as usize;
                let same_cell = state.edit_store == store && state.edit_key.as_ref().map(|k| (&k.0, k.1)) == key.as_ref().map(|k| (&k.0, k.1));
                if state.edit_key != key && !(same_cell && state.edit_dirty) || state.edit_store != store {
                    state.edit_key = key;
                    state.edit_store = store;
                    state.edit_text = text_for(&value, kind);
                    state.edit_dirty = false;
                    state.error = None;
                }
                ScrollArea::both().auto_shrink([false, false]).max_height(ui.available_height() - 36.0).show(ui, |ui| {
                    let te = TextEdit::multiline(&mut state.edit_text)
                        .desired_width(if state.wrap { ui.available_width() } else { f32::INFINITY })
                        .desired_rows(8)
                        .font(egui::TextStyle::Monospace);
                    let resp = if cell_editable {
                        ui.add(te)
                    } else {
                        let mut ro = state.edit_text.as_str();
                        ui.add(TextEdit::multiline(&mut ro).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace))
                    };
                    if resp.changed() {
                        state.edit_dirty = true;
                        state.error = None;
                    }
                });
                ui.horizontal(|ui| {
                    if cell_editable {
                        if ui.add_enabled(state.edit_dirty, egui::Button::new(tr("Apply"))).clicked() {
                            match model.set_cell(row, col, &state.edit_text) {
                                Ok(()) => {
                                    state.edit_dirty = false;
                                    state.edit_key = model.row_key(row).map(|k| (k, col, model.generation()));
                                }
                                Err(e) => state.error = Some(e),
                            }
                        }
                        if ui.add_enabled(state.edit_dirty, egui::Button::new(tr("Revert"))).clicked() {
                            state.edit_text = text_for(&value, kind);
                            state.edit_dirty = false;
                        }
                        if ui.button(tr("Set NULL")).clicked() {
                            match model.set_null(row, col) {
                                // The text shows the cell again (typed text is gone).
                                Ok(()) => {
                                    state.edit_dirty = false;
                                    state.edit_key = None;
                                }
                                Err(e) => state.error = Some(e),
                            }
                        }
                    } else if let Some(r) = model.read_only_reason() {
                        ui.weak(r.short_label());
                    }
                    ui.weak(trf("{} chars", &[&state.edit_text.chars().count()]));
                });
                if let Some(e) = &state.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
            }
            ValueMode::Json => {
                let text = value.to_text();
                match pretty_json(&text) {
                    Ok(p) => readonly_text(ui, &p, state.wrap),
                    Err(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                        readonly_text(ui, &text, state.wrap);
                    }
                }
            }
            ValueMode::JsonTree => match serde_json::from_str::<serde_json::Value>(&value.to_text()) {
                Ok(j) => {
                    ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        let mut budget = 20_000usize;
                        json_tree(ui, "$", &j, &mut budget, 0);
                        if budget == 0 {
                            ui.weak(tr("(tree truncated)"));
                        }
                    });
                }
                Err(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, trf("Invalid JSON: {}", &[&e]));
                }
            },
            ValueMode::Xml => {
                let text = value.to_text();
                match pretty_xml(&text) {
                    Ok(p) => readonly_text(ui, &p, state.wrap),
                    Err(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                        readonly_text(ui, &text, state.wrap);
                    }
                }
            }
            ValueMode::Hex => {
                if let Value::Bytes(b) = &value {
                    ui.horizontal(|ui| {
                        ui.weak(byte_size_text(b.len()));
                        save_button(ui, &mut actions, row, col, detect_image(b).map_or("bin", |k| k.extension()));
                    });
                    readonly_text(ui, &hex_dump(b, 64 * 1024), false);
                }
            }
            ValueMode::Image => {
                if let Value::Bytes(b) = &value {
                    if state.image_key != key {
                        state.image_key = key;
                        state.image = Some(decode_image(b, 2048).map(|d| {
                            let tex = ui.ctx().load_texture(format!("dbiji-value-{row}-{col}"), d.image, TextureOptions::LINEAR);
                            (tex, d.original_size, d.kind.label())
                        }));
                    }
                    ui.horizontal(|ui| {
                        ui.weak(byte_size_text(b.len()));
                        save_button(ui, &mut actions, row, col, detect_image(b).map_or("bin", |k| k.extension()));
                    });
                    match &state.image {
                        Some(Ok((tex, size, label))) => {
                            ui.weak(format!("{label} {} × {}", size[0], size[1]));
                            ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                                let avail = ui.available_size();
                                let s = tex.size_vec2();
                                let scale = (avail.x / s.x).min(avail.y / s.y).clamp(0.05, 1.0);
                                ui.image((tex.id(), s * scale));
                            });
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        None => {}
                    }
                }
            }
        }
        // Extra details for special kinds.
        match (&value, kind) {
            (Value::Uuid(u), _) => {
                ui.separator();
                ui.monospace(u.hyphenated().to_string().to_uppercase());
                ui.monospace(u.simple().to_string());
            }
            (Value::Array(items), _) => {
                ui.separator();
                ui.label(trf("{} elements", &[&items.len()]));
                readonly_text(ui, &array_lines(items), false);
            }
            (Value::Text(s), DataKind::Geometry) => {
                let (srid, wkt) = split_ewkt(s);
                ui.separator();
                if let Some(srid) = srid {
                    ui.label(trf("SRID {}", &[&srid]));
                }
                let geom = wkt.split(['(', ' ']).next().unwrap_or("");
                ui.weak(trf("Geometry type: {}", &[&geom]));
            }
            (Value::Text(_), DataKind::Interval) => {
                ui.separator();
                ui.weak(tr("Interval"));
            }
            _ => {}
        }
        actions
    }
}

fn save_button(ui: &mut Ui, actions: &mut Vec<GridAction>, row: usize, col: usize, ext: &str) {
    if ui.button(format!("{}  {}", egui_phosphor::regular::DOWNLOAD_SIMPLE, tr("Save to file…"))).clicked() {
        actions.push(GridAction::SaveValueToFile { row, col, extension: ext.to_string() });
    }
}

fn text_for(v: &Value, kind: DataKind) -> String {
    match v {
        Value::Null => String::new(),
        Value::Json(s) if kind == DataKind::Json => s.to_string(),
        other => other.to_text().into_owned(),
    }
}

fn readonly_text(ui: &mut Ui, text: &str, wrap: bool) {
    ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let mut s = text;
        let mut te = TextEdit::multiline(&mut s).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY);
        if !wrap {
            te = te.clip_text(false);
        }
        ui.add(te);
    });
}
