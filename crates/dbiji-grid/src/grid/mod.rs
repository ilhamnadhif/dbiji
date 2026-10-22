//! The virtual data grid widget (FR-D1 .. FR-D6).
//!
//! Custom painted and virtualised in both axes: per frame only the visible
//! rows and columns are formatted and painted, the row store is locked once,
//! and cell text is composed from a glyph cache (see `text.rs`), so the cost
//! of a frame does not depend on the number of rows.
//!
//! ```no_run
//! # use dbiji_grid::{DataGrid, DataGridState, GridOptions, GridAction, ResultModel};
//! # fn ui(ui: &mut egui::Ui, model: &mut ResultModel, state: &mut DataGridState, options: &GridOptions) {
//! let response = DataGrid::new(model, state, options).show(ui);
//! for action in response.actions {
//!     match action {
//!         GridAction::FetchMore => { /* fetch the next page into model.store() */ }
//!         _ => {}
//!     }
//! }
//! # }
//! ```

mod dialogs;
mod editor;
mod frame;
mod input;
mod menu;
mod paint;
pub mod selection;
pub mod state;
pub(crate) mod text;

pub(crate) use paint::{CANCEL_AFTER, kind_icon};
pub use selection::{CellPos, CellRange, Selection};
pub use state::{ColumnState, DataGridState, DataMode};

use crate::copy::{CopyColumn, CopyData, CopyFormat, CopyOptions, CopyRow, parse_clipboard_table};
use crate::model::{ForeignKeyLink, ResultModel};
use crate::view::{ClientFilter, FilterOp, SortKey, ViewSpec, condition_text};
use dbiji_core::value::FormatOptions;
use dbiji_core::{Dialect, ObjectRef, Value, tr, trf};
use std::sync::Arc;
use std::time::Duration;

/// Editing and layout commands that act on the active cell or the
/// selection, run by the grid on its next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridCommand {
    /// Insert a new row before the active one.
    AddRowBefore,
    /// Copy the values of the row above into the selected cells.
    CopyFromAbove,
    /// Copy the values of the row below into the selected cells.
    CopyFromBelow,
    /// A new UUID in each selected cell.
    GenerateUuid,
    /// Move the active column left.
    MoveColumnLeft,
    /// Move the active column right.
    MoveColumnRight,
    /// Column widths that fit the visible values.
    FitColumnsToValues,
    /// Column widths that fill the visible width.
    FitColumnsToScreen,
    /// Hide the columns without data.
    HideEmptyColumns,
}

/// Display options of a grid, owned by the application.
#[derive(Clone)]
pub struct GridOptions {
    /// Value formatting ("format data").
    pub format: FormatOptions,
    /// Show the row number column.
    pub show_row_numbers: bool,
    /// Alternate row background.
    pub striped: bool,
    /// Row height; `None` derives it from the font.
    pub row_height: Option<f32>,
    /// Derived row heights leave little space around the text (more rows on screen).
    pub compact_rows: bool,
    /// Cell font; `None` uses the body text style.
    pub font: Option<egui::FontId>,
    /// Emit [`GridAction::FetchMore`] when the last visible row is this close to the end.
    pub fetch_more_threshold: usize,
    /// Dialect for SQL generation / SQL INSERT copies (ANSI when `None`).
    pub dialect: Option<Arc<dyn Dialect>>,
    /// Table name for SQL INSERT copies when the result has no edit target.
    pub sql_table_name: Option<String>,
    /// Master switch for editing (in addition to the model's editability).
    pub allow_editing: bool,
    /// Show dictionary descriptions next to values.
    pub show_descriptions: bool,
    /// Refuse to copy more cells than this.
    pub max_copy_cells: usize,
    /// Maximum characters shown in a cell tooltip.
    pub max_tooltip_chars: usize,
    /// Record view: hide the columns that are NULL or empty.
    pub record_hide_empty: bool,
}

impl std::fmt::Debug for GridOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GridOptions")
            .field("show_row_numbers", &self.show_row_numbers)
            .field("striped", &self.striped)
            .field("row_height", &self.row_height)
            .field("allow_editing", &self.allow_editing)
            .finish_non_exhaustive()
    }
}

impl GridOptions {
    /// Height of a row whose text is `text_h` high (grid and record view).
    pub fn row_height_for(&self, text_h: f32) -> f32 {
        let pad = if self.compact_rows { 4.0 } else { 8.0 };
        self.row_height.unwrap_or((text_h + pad).round()).max(text_h + 2.0)
    }
}

impl Default for GridOptions {
    fn default() -> Self {
        GridOptions {
            format: FormatOptions::default(),
            show_row_numbers: true,
            striped: true,
            row_height: None,
            compact_rows: true,
            font: None,
            fetch_more_threshold: 200,
            dialect: None,
            sql_table_name: None,
            allow_editing: true,
            show_descriptions: true,
            max_copy_cells: 1_000_000,
            max_tooltip_chars: 2_000,
            record_hide_empty: false,
        }
    }
}

pub use crate::model::SqlKind;

/// Which alternative result view to show (toolbar view switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum ResultViewKind {
    /// The data grid.
    #[default]
    Grid,
    /// Fixed-width text table.
    Text,
    /// Single record, transposed.
    Record,
    /// JSON document.
    Json,
    /// XML document.
    Xml,
}

/// Side panels (FR-D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PanelKind {
    /// Value viewer / editor of the active cell.
    Value,
    /// Column metadata.
    Metadata,
    /// Grouping.
    Grouping,
    /// Statistics of the selection.
    Calc,
    /// Foreign key references.
    References,
}

/// Something the application has to handle (database access, navigation,
/// dialogs it owns). Everything else is handled inside the grid.
#[derive(Debug, Clone, PartialEq)]
pub enum GridAction {
    /// The record view's "Hide empty" was changed (for the application to
    /// remember it).
    RecordPrefs {
        /// Hide the columns that are NULL or empty.
        hide_empty: bool,
    },
    /// Put this text at the end of the filter box and focus it (the user
    /// types the value); empty: just focus the filter box.
    EditFilter(String),
    /// Save the filter box's WHERE / ORDER BY as the table's default
    /// (reported as [`GridAction::DefaultFilter`] by the result view).
    SaveDefaultFilter,
    /// Empty the filter and order boxes (filters and orderings were removed).
    ClearFilterText,
    /// Save the current WHERE / ORDER BY as the table's default (both
    /// `None`: forget the default).
    DefaultFilter {
        /// WHERE condition.
        where_clause: Option<String>,
        /// ORDER BY list.
        order_by: Option<String>,
    },
    /// The user scrolled near the end and `model.fetch.has_more` is set:
    /// fetch the next page and append it to `model.store()` (FR-D1).
    FetchMore,
    /// Fetch all remaining rows.
    FetchAll,
    /// Stop the running fetch (the query on the server too, where it can be cancelled).
    CancelFetch,
    /// Run a `COUNT(*)` and set `model.fetch.total_count`.
    CountTotal,
    /// Re-run the query (manual or auto refresh).
    Refresh,
    /// Save pending changes: show `model.change_preview_sql`, execute
    /// `model.change_statements` in one transaction, then call `model.apply_saved()`.
    SaveChanges,
    /// Pending changes were discarded (informational).
    ChangesDiscarded,
    /// Server side mode: re-run the query with this `WHERE` / `ORDER BY`
    /// (`None` = unchanged, empty string = remove).
    ServerQueryChanged {
        /// New WHERE condition (without `WHERE`).
        where_clause: Option<String>,
        /// New ORDER BY list (without `ORDER BY`).
        order_by: Option<String>,
    },
    /// Navigate through a foreign key (cmd+click on a link cell, FR-D6).
    NavigateForeignKey {
        /// The foreign key.
        link: ForeignKeyLink,
        /// Values of the key columns, in `link.columns` order.
        values: Vec<Value>,
    },
    /// Show the value panel for this cell (multi-line values, Shift+Enter).
    OpenValuePanel {
        /// Display row.
        row: usize,
        /// Model column.
        col: usize,
    },
    /// "Paste" chosen from the menu: read the clipboard and call [`DataGrid::paste_text`].
    PasteRequested,
    /// Export the selection (`rows: None` = all rows).
    Export {
        /// Display rows, `None` for all.
        rows: Option<Vec<usize>>,
        /// Model columns in display order.
        columns: Vec<usize>,
    },
    /// SQL generated for selected rows (also shown in a window).
    GeneratedSql {
        /// Statement kind.
        kind: SqlKind,
        /// The SQL text.
        sql: String,
    },
    /// Text was copied to the clipboard.
    Copied {
        /// Number of cells.
        cells: usize,
        /// Format used.
        format: CopyFormat,
    },
    /// The user wants to define a virtual key for the table (result is read-only: no key).
    DefineVirtualKey {
        /// The table, when known.
        table: Option<ObjectRef>,
    },
    /// Switch the result view.
    SwitchView(ResultViewKind),
    /// Show the result as a chart (FR-D8, provided by the application).
    ShowChart,
    /// Show the spatial (GIS) view (provided by the application).
    ShowSpatial,
    /// Toggle a side panel.
    TogglePanel(PanelKind),
    /// Auto refresh interval changed (`None` = off).
    AutoRefreshChanged(Option<Duration>),
    /// Information or error for the status bar.
    Notify {
        /// Message text.
        message: String,
        /// True for errors.
        error: bool,
    },
    /// The selection or active cell changed.
    SelectionChanged,
    /// Value panel "save to file" for a binary value.
    SaveValueToFile {
        /// Display row.
        row: usize,
        /// Model column.
        col: usize,
        /// Suggested file extension.
        extension: String,
    },
    /// The references panel needs related rows.
    RequestReferences {
        /// Index into the relations given to the panel.
        relation: usize,
        /// Key values of the current row.
        values: Vec<Value>,
    },
}

/// Result of [`DataGrid::show`].
#[derive(Debug)]
pub struct GridResponse {
    /// Response of the grid body (focus, hover, context menu).
    pub response: egui::Response,
    /// Actions for the application.
    pub actions: Vec<GridAction>,
    /// Active cell (display row, model column) after this frame.
    pub active_cell: Option<(usize, usize)>,
    /// The selection changed this frame.
    pub selection_changed: bool,
}

/// The data grid widget. Build it every frame.
pub struct DataGrid<'a> {
    model: &'a mut ResultModel,
    state: &'a mut DataGridState,
    options: &'a GridOptions,
    id_salt: egui::Id,
    notices_elsewhere: bool,
}

impl<'a> DataGrid<'a> {
    /// Grid over `model` with layout `state`.
    pub fn new(model: &'a mut ResultModel, state: &'a mut DataGridState, options: &'a GridOptions) -> Self {
        DataGrid { model, state, options, id_salt: egui::Id::new("dbiji_data_grid"), notices_elsewhere: false }
    }

    /// Distinguish several grids in the same `Ui`.
    pub fn id_salt(mut self, salt: impl egui::AsId) -> Self {
        self.id_salt = egui::Id::new(salt);
        self
    }

    /// The host shows [`GridAction::Notify`] messages (in a status bar): the
    /// grid does not repeat them over the data.
    pub fn notices_elsewhere(mut self) -> Self {
        self.notices_elsewhere = true;
        self
    }

    /// Show the grid in all available space.
    pub fn show(self, ui: &mut egui::Ui) -> GridResponse {
        self.state.rt.notices_elsewhere = self.notices_elsewhere;
        frame::show(self.model, self.state, self.options, self.id_salt, ui)
    }

    /// Paste TSV text at the active cell (adding rows when needed). Used for
    /// [`GridAction::PasteRequested`]; Cmd+V is handled by the grid itself.
    /// Returns the number of cells set.
    pub fn paste_text(model: &mut ResultModel, state: &mut DataGridState, text: &str) -> Result<usize, String> {
        paste_into(model, state, text)
    }

    /// Text for the current selection in `opts` format (what Cmd+C copies with
    /// [`CopyOptions::plain`]).
    pub fn copy_text(model: &ResultModel, state: &DataGridState, options: &GridOptions, opts: &CopyOptions) -> Result<(String, usize), String> {
        copy_text_and_nulls(model, state, options, opts).map(|(text, cells, ..)| (text, cells))
    }
}

/// [`DataGrid::copy_text`] and which copied cells are NULL (row by row).
/// Copied text, its cell count, which cells are NULL, and whether cells
/// between the copied ones were left out (a ⌘-click selection).
type CopiedText = (String, usize, Vec<Vec<bool>>, bool);

/// Copy the selection (see [`CopiedText`]).
pub(crate) fn copy_text_and_nulls(model: &ResultModel, state: &DataGridState, options: &GridOptions, opts: &CopyOptions) -> Result<CopiedText, String> {
    let data = selection_copy_data(model, state, options)?;
    let mut opts = opts.clone();
    if opts.sql_table.trim().is_empty() {
        opts.sql_table = sql_table_name(model, options);
    }
    let cells = data.cell_count();
    let dialect = options.dialect.as_deref();
    let nulls = data.rows.iter().map(|r| r.cells.iter().map(|c| c.as_ref().is_some_and(Value::is_null)).collect()).collect();
    let gaps = data.rows.iter().any(|r| r.cells.iter().any(Option::is_none));
    Ok((crate::copy::render(&data, &opts, &options.format, dialect), cells, nulls, gaps))
}

/// Insert a new row after the active row (or at the end) and select it.
/// Returns the display row.
pub fn add_row(model: &mut ResultModel, state: &mut DataGridState) -> Result<usize, String> {
    let after = state.active_cell().map(|(r, _)| r).or(model.row_count().checked_sub(1));
    let row = model.add_row(after)?;
    let col = state.active_cell().map_or(0, |(_, c)| c);
    state.select_cell(row, col);
    Ok(row)
}

/// Duplicate the selected rows; selects the first copy.
pub fn duplicate_selected(model: &mut ResultModel, state: &mut DataGridState) -> Result<usize, String> {
    let rows = state.selected_rows(model.row_count(), 10_000);
    let new = model.duplicate_rows(&rows)?;
    if let Some(&first) = new.first() {
        let col = state.active_cell().map_or(0, |(_, c)| c);
        state.select_cell(first, col);
    }
    Ok(new.len())
}

/// Mark the selected rows for deletion. Returns the number of rows.
pub fn delete_selected(model: &mut ResultModel, state: &mut DataGridState) -> Result<usize, String> {
    let rows = state.selected_rows(model.row_count(), 1_000_000);
    model.delete_rows(&rows)
}

/// Table name used for generated SQL.
pub(crate) fn sql_table_name(model: &ResultModel, options: &GridOptions) -> String {
    let generic = dbiji_core::dialect::GenericDialect::new();
    let dialect: &dyn Dialect = options.dialect.as_deref().unwrap_or(&generic);
    if let Ok(t) = model.editability() {
        return dialect.qualified_ref(&t.table);
    }
    if let Some(t) = &options.sql_table_name {
        return t.clone();
    }
    match crate::model::source_table(model.columns()) {
        Ok(t) => dialect.qualified_ref(&t),
        Err(_) => "table_name".to_string(),
    }
}

/// Copy data for the selection (rows × selected columns).
pub(crate) fn selection_copy_data(model: &ResultModel, state: &DataGridState, options: &GridOptions) -> Result<CopyData, String> {
    selection_copy_data_limited(model, state, options, usize::MAX)
}

/// Copy data for at most `row_limit` selected rows (previews).
pub(crate) fn selection_copy_data_limited(model: &ResultModel, state: &DataGridState, options: &GridOptions, row_limit: usize) -> Result<CopyData, String> {
    let visible = state.visible_columns();
    let nrows = model.row_count();
    let sel = state.selection();
    if sel.is_empty() || nrows == 0 || visible.is_empty() {
        return Err(tr("Nothing is selected").to_string());
    }
    let cells = sel.cell_count(nrows, visible.len());
    if cells > options.max_copy_cells && row_limit == usize::MAX {
        return Err(trf(
            "The selection has {} cells; at most {} can be copied at once. Use Export instead.",
            &[&crate::format::group_thousands(&cells.to_string()), &crate::format::group_thousands(&options.max_copy_cells.to_string())],
        ));
    }
    let col_pos = sel.columns(visible.len());
    let rows = sel.rows(nrows, row_limit);
    let model_cols: Vec<usize> = col_pos.iter().map(|&p| visible[p]).collect();
    let columns: Vec<CopyColumn> = model_cols
        .iter()
        .map(|&c| CopyColumn {
            name: model.column_name(c).to_string(),
            kind: model.column_kind(c),
            type_name: model.column_meta(c).map(|m| m.type_name.clone()).unwrap_or_default(),
            format: state.column_format(c).cloned(),
        })
        .collect();
    let values = model.values(&rows, &model_cols);
    let all = sel.is_all();
    let out_rows = rows
        .iter()
        .zip(values)
        .map(|(&r, vals)| CopyRow { number: r + 1, cells: vals.into_iter().zip(&col_pos).map(|(v, &p)| if all || sel.contains(r, p) { Some(v) } else { None }).collect() })
        .collect();
    Ok(CopyData { columns, rows: out_rows })
}

/// Paste clipboard text at the active cell.
pub(crate) fn paste_into(model: &mut ResultModel, state: &mut DataGridState, text: &str) -> Result<usize, String> {
    // Text DBiji copied itself: its NULLs and its single cell are known.
    let own = state.rt.last_copy.as_ref().filter(|(t, ..)| !text.is_empty() && t == text);
    if own.is_some_and(|(.., gaps)| *gaps) {
        return Err(tr("Cells copied with gaps between them cannot be pasted: copy a block").to_string());
    }
    let remembered = own.map(|(_, n, _)| n.clone());
    let block = match &remembered {
        Some(n) if n.len() == 1 && n[0].len() == 1 => vec![vec![text.to_string()]],
        Some(n) => Some(crate::copy::parse_own_copy(text)).filter(|b| b.len() == n.len()).unwrap_or_else(|| parse_clipboard_table(text)),
        None => parse_clipboard_table(text),
    };
    if block.is_empty() {
        return Ok(0);
    }
    let nulls = remembered.filter(|n| n.len() == block.len() && n.iter().zip(&block).all(|(a, b)| a.len() == b.len()));
    let visible = state.visible_columns();
    let Some((active_row, col)) = state.active_cell() else { return Err(tr("Select a cell to paste into").to_string()) };
    let mut row = active_row;
    let mut start = visible.iter().position(|&c| c == col).unwrap_or(0);
    // A block goes to the top left of a selected range, wherever the cursor is in it.
    if state.selection().cell_count(model.row_count(), visible.len()) > 1
        && let (Some(&r0), Some(&c0)) = (state.selection().rows(model.row_count(), 1).first(), state.selection().columns(visible.len()).first())
    {
        (row, start) = (r0, c0);
    }
    let width = block.iter().map(Vec::len).max().unwrap_or(0);
    // Paste a single value into every selected cell of a multi-cell selection.
    if block.len() == 1 && width == 1 {
        let nrows = model.row_count();
        let sel = state.selection().clone();
        let cells = sel.cell_count(nrows, visible.len());
        if cells > 1 && cells <= 100_000 {
            let mut edits = Vec::new();
            for r in sel.rows(nrows, usize::MAX) {
                for p in sel.columns(visible.len()) {
                    if sel.contains(r, p) && !model.is_virtual_column(visible[p]) {
                        let v = if nulls.as_ref().is_some_and(|n| n[0][0]) { Value::Null } else { model.parse_for_column(visible[p], &block[0][0])? };
                        edits.push((r, visible[p], crate::model::CellValue::Value(v)));
                    }
                }
            }
            let n = edits.len();
            model.set_cells(edits)?;
            return Ok(n);
        }
    }
    let columns: Vec<usize> = visible[start..].iter().copied().take(width).collect();
    let n = model.paste_block(row, &block, &columns, nulls.as_deref())?;
    // Select the pasted block.
    let last_row = row + block.len() - 1;
    let last_col = start + columns.len().saturating_sub(1);
    let sel = state.selection_mut();
    sel.select_cell(CellPos::new(row, start));
    sel.extend_to(CellPos::new(last_row, last_col));
    Ok(n)
}

/// Quoted name of `col` in a server side `WHERE` condition.
pub(crate) fn server_column(model: &ResultModel, options: &GridOptions, col: usize) -> String {
    let generic = dbiji_core::dialect::GenericDialect::new();
    let d: &dyn Dialect = options.dialect.as_deref().unwrap_or(&generic);
    // Query results are filtered as `SELECT * FROM (<query>) q WHERE ...`, so
    // they use the result column names; table data uses the table's names.
    let name = if options.sql_table_name.is_none() {
        model.column_name(col).to_string()
    } else {
        model
            .editability()
            .as_ref()
            .ok()
            .and_then(|t| t.column_names.get(col).cloned().flatten())
            .or_else(|| model.column_meta(col).and_then(|m| m.source.as_ref().map(|s| s.column.clone())))
            .unwrap_or_else(|| model.column_name(col).to_string())
    };
    d.quote_ident(&name)
}

/// Build a `WHERE` condition for "filter by value" in server mode.
pub(crate) fn server_condition(model: &ResultModel, options: &GridOptions, col: usize, op: FilterOp, value: &Value) -> String {
    let generic = dbiji_core::dialect::GenericDialect::new();
    let d: &dyn Dialect = options.dialect.as_deref().unwrap_or(&generic);
    let c = server_column(model, options, col);
    let type_name = model.column_meta(col).map(|m| m.type_name.as_str()).filter(|t| !t.is_empty());
    match op {
        FilterOp::IsNull => format!("{c} IS NULL"),
        FilterOp::IsNotNull => format!("{c} IS NOT NULL"),
        _ if value.is_null() => {
            if op == FilterOp::Eq {
                format!("{c} IS NULL")
            } else {
                format!("{c} IS NOT NULL")
            }
        }
        FilterOp::Contains => {
            // `\` escapes the pattern's wildcards; most databases need ESCAPE to know it.
            let kind = d.kind();
            let mut p = String::from("%");
            for ch in value.to_text().chars() {
                if matches!(ch, '%' | '_' | '\\') || (ch == '[' && kind == dbiji_core::DbKind::SqlServer) {
                    p.push('\\');
                }
                p.push(ch);
            }
            p.push('%');
            let escape = if kind == dbiji_core::DbKind::ClickHouse { String::new() } else { format!(" ESCAPE {}", d.string_literal("\\")) };
            format!("{c} LIKE {}{escape}", d.string_literal(&p))
        }
        FilterOp::Like | FilterOp::NotLike => format!("{c} {} {}", op.sql(), d.string_literal(&value.to_text())),
        _ => format!("{c} {} {}", op.sql(), d.literal(value, type_name)),
    }
}

/// `ORDER BY` text for sort keys in server mode.
pub(crate) fn server_order_by(model: &ResultModel, options: &GridOptions, keys: &[SortKey]) -> String {
    let generic = dbiji_core::dialect::GenericDialect::new();
    let d: &dyn Dialect = options.dialect.as_deref().unwrap_or(&generic);
    keys.iter()
        .filter(|k| !model.is_virtual_column(k.column))
        .map(|k| {
            let name = if options.sql_table_name.is_none() {
                model.column_name(k.column).to_string()
            } else {
                model.column_meta(k.column).and_then(|m| m.source.as_ref().map(|s| s.column.clone())).unwrap_or_else(|| model.column_name(k.column).to_string())
            };
            format!("{}{}", d.quote_ident(&name), if k.descending { " DESC" } else { "" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Apply a sort in the current mode.
pub(crate) fn apply_sort(model: &mut ResultModel, state: &mut DataGridState, options: &GridOptions, keys: Vec<SortKey>, ctx: &egui::Context, actions: &mut Vec<GridAction>) {
    state.sort = keys.clone();
    match state.mode {
        DataMode::Server => actions.push(GridAction::ServerQueryChanged { where_clause: None, order_by: Some(server_order_by(model, options, &keys)) }),
        DataMode::Client => {
            let mut spec = model.requested_view_spec();
            spec.sort = keys;
            let ctx = ctx.clone();
            model.start_client_view(spec, Box::new(move || ctx.request_repaint()));
        }
    }
}

/// Apply "filter by value" (`col op value`) in `mode`.
pub(crate) fn apply_value_filter(
    model: &mut ResultModel,
    mode: DataMode,
    options: &GridOptions,
    (col, op, value): (usize, FilterOp, &Value),
    ctx: &egui::Context,
    actions: &mut Vec<GridAction>,
) {
    match mode {
        DataMode::Server => {
            let cond = server_condition(model, options, col, op, value);
            actions.push(GridAction::ServerQueryChanged { where_clause: Some(cond), order_by: None });
        }
        DataMode::Client => {
            let cond = ClientFilter::condition(col, model.column_name(col), op, value);
            let mut spec = model.requested_view_spec();
            spec.filter = Some(match spec.filter.take() {
                Some(f) => f.and(cond),
                None => cond,
            });
            let ctx = ctx.clone();
            model.start_client_view(spec, Box::new(move || ctx.request_repaint()));
        }
    }
}

/// Apply a client filter typed in the filter bar (or clear it with `None`).
pub fn apply_client_filter(model: &mut ResultModel, filter: Option<ClientFilter>, ctx: &egui::Context) {
    let spec = ViewSpec { sort: model.requested_view_spec().sort, filter };
    let ctx = ctx.clone();
    model.start_client_view(spec, Box::new(move || ctx.request_repaint()));
}

/// Text of a client condition (used to show generated filters in the filter bar).
pub fn client_condition_text(model: &ResultModel, col: usize, op: FilterOp, value: &Value) -> String {
    condition_text(model.column_name(col), op, value)
}

#[cfg(test)]
mod tests;
