//! Persistent layout state of a grid plus transient interaction state.

use super::GridCommand;
use super::selection::{CellPos, Selection};
use super::text::GlyphCache;
use crate::copy::CopyOptions;
use crate::format::ColumnFormat;
use crate::model::ResultModel;
use crate::view::SortKey;
use dbiji_core::{ColumnMeta, DataKind};
use serde::{Deserialize, Serialize};

/// Where sorting and filtering are executed (FR-D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum DataMode {
    /// On the fetched rows, in DBiji (background thread).
    #[default]
    Client,
    /// By re-running the query with `WHERE` / `ORDER BY` (the application does it).
    Server,
}

/// Layout of one column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnState {
    /// Column name (used to match state with a new result).
    pub name: String,
    /// Width in points.
    pub width: f32,
    /// Shown or hidden.
    pub visible: bool,
    /// Display format override.
    pub format: Option<ColumnFormat>,
}

impl Default for ColumnState {
    fn default() -> Self {
        ColumnState { name: String::new(), width: 120.0, visible: true, format: None }
    }
}

/// Minimum column width.
pub const MIN_COLUMN_WIDTH: f32 = 24.0;
/// Maximum column width set by auto-sizing.
pub const MAX_AUTO_WIDTH: f32 = 480.0;

/// Default width for a column from its name and kind.
pub fn default_width(name: &str, kind: DataKind) -> f32 {
    let name_w = name.chars().count() as f32 * 7.5 + 34.0;
    let kind_w = match kind {
        DataKind::Bool => 60.0,
        DataKind::Int => 80.0,
        DataKind::Float | DataKind::Decimal => 100.0,
        DataKind::Date | DataKind::Time | DataKind::TimeTz => 96.0,
        DataKind::DateTime | DataKind::DateTimeTz => 160.0,
        DataKind::Uuid => 270.0,
        DataKind::Json | DataKind::Xml | DataKind::Geometry => 220.0,
        DataKind::Text | DataKind::Enum => 150.0,
        _ => 130.0,
    };
    name_w.max(kind_w).clamp(48.0, 320.0)
}

/// Highlight of the rows whose `column` has `value` (`None` = NULL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowHighlight {
    /// Column name.
    pub column: String,
    /// The value as text; `None` matches NULL.
    pub value: Option<String>,
    /// RGB colour.
    pub color: [u8; 3],
}

/// Colours offered for row highlighting: (label, RGB).
pub const HIGHLIGHT_COLORS: [(&str, [u8; 3]); 6] =
    [("Red", [230, 70, 70]), ("Orange", [240, 150, 40]), ("Yellow", [230, 200, 40]), ("Green", [60, 180, 90]), ("Blue", [60, 130, 230]), ("Purple", [160, 90, 220])];

/// Grid state the application keeps per result tab. The serialisable part
/// (column widths, order, visibility, formats, sort, mode) can be stored to
/// persist layouts; selection, scroll position and editing state are transient.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DataGridState {
    /// One entry per model column (result columns, then virtual columns).
    pub columns: Vec<ColumnState>,
    /// Display order of model columns (hidden ones included).
    pub order: Vec<usize>,
    /// Sort shown in the header (and applied in client mode).
    pub sort: Vec<SortKey>,
    /// Client or server side sorting / filtering.
    pub mode: DataMode,
    /// Last options used in the advanced copy dialog.
    pub copy_options: CopyOptions,
    /// Text size of the grid (`None` = 100 %).
    pub zoom: Option<f32>,
    /// Rows highlighted by the value of a column.
    pub highlights: Vec<RowHighlight>,
    #[serde(skip)]
    pub(crate) rt: Runtime,
}

/// Transient interaction state.
#[derive(Debug, Clone, Default)]
pub(crate) struct Runtime {
    /// Commands to run on the next frame (see [`DataGridState::request`]).
    pub pending: Vec<GridCommand>,
    pub selection: Selection,
    pub scroll_x: f32,
    pub scroll_y: f64,
    pub editing: Option<EditSession>,
    pub drag: Drag,
    pub glyphs: GlyphCache,
    /// Row count at which FetchMore was last emitted.
    pub fetch_requested_at: Option<usize>,
    /// Scroll the active cell into view on the next frame.
    pub scroll_to_active: bool,
    pub copy_dialog: bool,
    pub sql_window: Option<(String, String)>,
    pub virtual_dialog: Option<VirtualColumnDialog>,
    pub message: Option<(String, bool, f64)>,
    pub dropdown: Option<Dropdown>,
    /// Store length when a stale client view was last refreshed automatically.
    pub auto_view_len: usize,
    pub last_selection_label: String,
    /// Geometry of the last frame: body, header, row numbers, row height.
    pub last_geometry: Option<(egui::Rect, egui::Rect, egui::Rect, f32)>,
    /// Body shapes of the last frame, repeated when the store is busy.
    pub body_cache: Option<BodyCache>,
}

/// The painted body of the last frame. When the fetch task holds the row
/// store lock (it may spill pages to disk under it), the grid repeats these
/// shapes shifted by the scroll change instead of blocking the UI thread.
#[derive(Debug, Clone)]
pub(crate) struct BodyCache {
    pub shapes: Vec<egui::Shape>,
    pub scroll: (f32, f64),
    pub body: egui::Rect,
}

/// An inline edit in progress.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EditSession {
    pub row: usize,
    /// Model column.
    pub col: usize,
    pub text: String,
    pub original: String,
    pub error: Option<String>,
    /// First frame: focus the editor and place the cursor.
    pub just_started: bool,
    /// Started by typing (cursor at the end) instead of Enter/F2 (select all).
    pub typed: bool,
}

/// State of the dictionary / enum dropdown editor.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Dropdown {
    pub row: usize,
    pub col: usize,
    pub search: String,
    pub just_opened: bool,
}

/// State of the "add virtual column" dialog.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct VirtualColumnDialog {
    pub name: String,
    pub expression: String,
    pub error: Option<String>,
    /// Editing an existing virtual column (index among virtual columns).
    pub editing: Option<usize>,
}

/// Mouse drag in progress.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum Drag {
    #[default]
    None,
    /// Selecting cells by dragging.
    Cells,
    /// Selecting rows on the row header.
    Rows,
    /// Resizing a column (model index).
    Resize { col: usize, start_width: f32, start_x: f32 },
    /// Moving a column (from display position); `active` once the pointer moved enough.
    Move { from: usize, start_x: f32, active: bool },
    /// Dragging the vertical scrollbar thumb.
    VThumb { grab: f32 },
    /// Dragging the horizontal scrollbar thumb.
    HThumb { grab: f32 },
}

impl DataGridState {
    /// Default layout for `columns`.
    pub fn new(columns: &[ColumnMeta]) -> Self {
        let cols: Vec<ColumnState> = columns.iter().map(|c| ColumnState { name: c.name.clone(), width: default_width(&c.name, c.kind), ..Default::default() }).collect();
        let order = (0..cols.len()).collect();
        DataGridState { columns: cols, order, ..Default::default() }
    }

    /// Default layout for a model (including virtual columns).
    pub fn for_model(model: &ResultModel) -> Self {
        let mut s = DataGridState::default();
        s.sync_columns(model);
        s
    }

    /// Adapt to the model's columns: keeps widths, visibility and order of
    /// columns with the same names, adds new columns at the end.
    pub fn sync_columns(&mut self, model: &ResultModel) {
        let n = model.column_count();
        let same = self.columns.len() == n && (0..n).all(|c| self.columns[c].name == model.column_name(c));
        if same {
            if self.order.len() != n || !is_permutation(&self.order, n) {
                self.order = (0..n).collect();
            }
            return;
        }
        let old = std::mem::take(&mut self.columns);
        let old_order = std::mem::take(&mut self.order);
        let mut used = vec![false; old.len()];
        let mut mapping: Vec<Option<usize>> = Vec::with_capacity(n); // new -> old
        for c in 0..n {
            let name = model.column_name(c);
            let by_index = old.get(c).filter(|o| o.name == name && !used[c]).map(|_| c);
            let found = by_index.or_else(|| (0..old.len()).find(|&i| !used[i] && old[i].name == name));
            if let Some(i) = found {
                used[i] = true;
            }
            mapping.push(found);
            let kind = model.column_kind(c);
            self.columns.push(match found {
                Some(i) => ColumnState { name: name.to_string(), ..old[i].clone() },
                None => ColumnState { name: name.to_string(), width: default_width(name, kind), ..Default::default() },
            });
        }
        // Order: old relative order for kept columns, then new columns.
        let mut order: Vec<usize> = Vec::with_capacity(n);
        for &o in &old_order {
            if let Some(newc) = mapping.iter().position(|m| *m == Some(o)) {
                order.push(newc);
            }
        }
        for c in 0..n {
            if !order.contains(&c) {
                order.push(c);
            }
        }
        self.order = order;
        self.sort.retain(|k| k.column < n);
        self.rt.editing = None;
        self.rt.dropdown = None;
    }

    /// Model columns in display order, visible ones only.
    pub fn visible_columns(&self) -> Vec<usize> {
        self.order.iter().copied().filter(|&c| self.columns.get(c).is_some_and(|s| s.visible)).collect()
    }

    /// Show or hide a column.
    pub fn set_column_visible(&mut self, col: usize, visible: bool) {
        if let Some(c) = self.columns.get_mut(col) {
            c.visible = visible;
            self.rt.selection.clamp(usize::MAX, self.order.len());
        }
    }

    /// Show all columns.
    pub fn show_all_columns(&mut self) {
        for c in &mut self.columns {
            c.visible = true;
        }
    }

    /// Width of a model column.
    pub fn column_width(&self, col: usize) -> f32 {
        self.columns.get(col).map_or(120.0, |c| c.width)
    }

    /// Set the width of a model column.
    pub fn set_column_width(&mut self, col: usize, width: f32) {
        if let Some(c) = self.columns.get_mut(col) {
            c.width = width.max(MIN_COLUMN_WIDTH);
        }
    }

    /// Per-column display format.
    pub fn column_format(&self, col: usize) -> Option<&ColumnFormat> {
        self.columns.get(col).and_then(|c| c.format.as_ref())
    }

    /// Set (or clear) a per-column display format.
    pub fn set_column_format(&mut self, col: usize, format: Option<ColumnFormat>) {
        if let Some(c) = self.columns.get_mut(col) {
            c.format = format.filter(|f| !f.is_empty());
        }
    }

    /// Run a grid command on the next frame (menus, shortcuts, the command palette).
    pub fn request(&mut self, cmd: GridCommand) {
        self.rt.pending.push(cmd);
    }

    /// Change the grid text size by `step` (e.g. +0.1); `None` resets it.
    pub fn zoom_by(&mut self, step: Option<f32>) {
        self.zoom = step.map(|d| (self.zoom.unwrap_or(1.0) + d).clamp(0.5, 3.0)).filter(|z| (z - 1.0).abs() > 0.01);
    }

    /// Highlight (or stop highlighting, `color: None`) the rows where `column` is `value`.
    pub fn set_highlight(&mut self, column: &str, value: Option<String>, color: Option<[u8; 3]>) {
        self.highlights.retain(|h| !(h.column == column && h.value == value));
        if let Some(color) = color {
            self.highlights.push(RowHighlight { column: column.to_string(), value, color });
        }
    }

    /// Move the visible column at display position `from` to position `to`.
    pub fn move_visible_column(&mut self, from: usize, to: usize) {
        let visible = self.visible_columns();
        let (Some(&moving), Some(&target)) = (visible.get(from), visible.get(to.min(visible.len().saturating_sub(1)))) else {
            return;
        };
        if moving == target {
            return;
        }
        let Some(src) = self.order.iter().position(|&c| c == moving) else { return };
        self.order.remove(src);
        let dst = self.order.iter().position(|&c| c == target).unwrap_or(self.order.len());
        let dst = if to > from { dst + 1 } else { dst };
        self.order.insert(dst.min(self.order.len()), moving);
    }

    /// The selection (display coordinates: row, visible column position).
    pub fn selection(&self) -> &Selection {
        &self.rt.selection
    }

    /// Mutable selection.
    pub fn selection_mut(&mut self) -> &mut Selection {
        &mut self.rt.selection
    }

    /// Active cell as (display row, model column).
    pub fn active_cell(&self) -> Option<(usize, usize)> {
        if self.rt.selection.is_empty() {
            return None;
        }
        let p = self.rt.selection.active();
        let col = *self.visible_columns().get(p.col)?;
        Some((p.row, col))
    }

    /// Select a cell by display row and model column and scroll to it.
    pub fn select_cell(&mut self, row: usize, model_col: usize) {
        let pos = self.visible_columns().iter().position(|&c| c == model_col).unwrap_or(0);
        self.rt.selection.select_cell(CellPos::new(row, pos));
        self.rt.scroll_to_active = true;
    }

    /// Selected display rows (sorted), at most `limit`.
    pub fn selected_rows(&self, row_count: usize, limit: usize) -> Vec<usize> {
        self.rt.selection.rows(row_count, limit)
    }

    /// Selected model columns in display order.
    pub fn selected_columns(&self) -> Vec<usize> {
        let visible = self.visible_columns();
        self.rt.selection.columns(visible.len()).into_iter().filter_map(|p| visible.get(p).copied()).collect()
    }

    /// Request scrolling so the active cell is visible.
    pub fn scroll_to_active(&mut self) {
        self.rt.scroll_to_active = true;
    }

    /// Vertical scroll offset in points.
    pub fn scroll_offset(&self) -> (f32, f64) {
        (self.rt.scroll_x, self.rt.scroll_y)
    }

    /// Set the scroll offset (clamped on the next frame).
    pub fn set_scroll_offset(&mut self, x: f32, y: f64) {
        self.rt.scroll_x = x.max(0.0);
        self.rt.scroll_y = y.max(0.0);
    }

    /// True while a cell editor is open.
    pub fn is_editing(&self) -> bool {
        self.rt.editing.is_some()
    }

    /// Show a transient status message inside the grid.
    pub fn show_message(&mut self, text: impl Into<String>, error: bool) {
        self.rt.message = Some((text.into(), error, 0.0));
    }
}

fn is_permutation(order: &[usize], n: usize) -> bool {
    let mut seen = vec![false; n];
    for &o in order {
        if o >= n || seen[o] {
            return false;
        }
        seen[o] = true;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnMeta, Value};

    fn model(names: &[&str]) -> ResultModel {
        let cols = names.iter().map(|n| ColumnMeta::new(*n, "text", DataKind::Text)).collect();
        ResultModel::from_rows(cols, vec![names.iter().map(|_| Value::Null).collect()])
    }

    #[test]
    fn sync_keeps_layout_by_name() {
        let m = model(&["a", "b", "c"]);
        let mut s = DataGridState::for_model(&m);
        s.set_column_width(1, 300.0);
        s.set_column_visible(2, false);
        s.move_visible_column(1, 0);
        assert_eq!(s.visible_columns(), vec![1, 0]);
        let m2 = model(&["c", "b", "d"]);
        s.sync_columns(&m2);
        assert_eq!(s.columns.len(), 3);
        assert_eq!(s.column_width(1), 300.0);
        assert!(!s.columns[0].visible, "c stays hidden");
        assert!(s.columns[2].visible);
        assert_eq!(s.order, vec![1, 0, 2]);
        // Serialisation round trip of the persistent part.
        let json = serde_json::to_string(&s).unwrap();
        let back: DataGridState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.columns, s.columns);
        assert_eq!(back.order, s.order);
    }

    #[test]
    fn move_and_select() {
        let m = model(&["a", "b", "c", "d"]);
        let mut s = DataGridState::for_model(&m);
        s.move_visible_column(0, 3);
        assert_eq!(s.visible_columns(), vec![1, 2, 3, 0]);
        s.move_visible_column(3, 1);
        assert_eq!(s.visible_columns(), vec![1, 0, 2, 3]);
        s.select_cell(0, 2);
        assert_eq!(s.active_cell(), Some((0, 2)));
        assert_eq!(s.selected_columns(), vec![2]);
        assert!(default_width("id", DataKind::Int) >= 48.0);
    }
}
