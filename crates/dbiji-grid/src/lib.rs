//! # dbiji-grid — virtual data grid and result views
//!
//! The data editor of DBiji (PRD §6.4, FR-D1 … FR-D8): a custom painted,
//! virtualised egui grid that stays responsive with millions of rows, plus
//! the toolbar, filter bar, alternative views (record, text, JSON, XML) and
//! side panels (value, metadata, grouping, calc, references).
//!
//! The crate never talks to a database. The application owns the
//! connection, executes queries, appends fetched rows to the
//! [`SharedRowStore`](dbiji_core::SharedRowStore) and reacts to
//! [`GridAction`]s.
//!
//! ## Building blocks
//!
//! | Layer | Types |
//! | --- | --- |
//! | Data | [`ResultModel`] (rows, client view, pending edits, virtual columns, dictionaries, fetch state) |
//! | Editability | [`determine_editability`], [`EditTarget`], [`ReadOnlyReason`] |
//! | Client sort / filter | [`view`] (`compute_sort_index`, `compute_filter_index`, background [`view::ViewJob`]), [`expr`] |
//! | Widgets | [`DataGrid`], [`toolbar::FilterBar`], [`toolbar::ResultToolbar`], [`views`], [`panels`] |
//! | All in one | [`ResultViewer`] + [`ResultViewState`] |
//! | Clipboard | [`copy`] (TSV, CSV, Markdown, JSON, SQL INSERT, HTML, XML; TSV paste) |
//!
//! ## Integration guide
//!
//! 1. **Run the query** on the async runtime. On `ResultHeader::Rows(columns)`
//!    create a store and a model:
//!    ```no_run
//!    # use dbiji_core::{RowStore, ColumnMeta};
//!    # use dbiji_grid::{ResultModel, ResultViewState};
//!    # let columns: Vec<ColumnMeta> = vec![];
//!    let store = RowStore::shared(dbiji_core::rowstore::DEFAULT_STORE_BUDGET);
//!    let mut model = ResultModel::new(columns, store.clone());
//!    let mut view = ResultViewState::new(&model); // or DataGridState::for_model
//!    model.fetch.fetching = true;
//!    ```
//!    The fetch task pushes rows with `store.lock().extend(batch)` (lock per
//!    batch of a few hundred to ~2,000 rows, never per row; build the rows
//!    before taking the lock) and calls `ctx.request_repaint()`. When a page
//!    is complete set `model.fetch.fetching = false`, `model.fetch.has_more`,
//!    `model.fetch.elapsed` (from the UI thread, e.g. via a channel). The grid
//!    never blocks on that lock for more than a few milliseconds: when the
//!    fetch task holds it (the store may spill pages to disk under the lock)
//!    the previous frame is repeated and the row count is picked up later.
//! 2. **Editability** (FR-D3): when the driver resolved column sources
//!    (`ExecOptions::resolve_sources`), call
//!    `model.set_editability(determine_editability(model.columns(), |table| key_columns(table), read_only))`
//!    where `key_columns` returns the primary key, a unique key or a user
//!    defined virtual key. Otherwise the grid shows *why* it is read-only.
//!    Optional metadata: `set_foreign_keys` (link cells, cmd+click
//!    navigation), `set_dictionary` (value descriptions, dropdown editor),
//!    `set_enum_values` (dropdown editor).
//! 3. **Show it** every frame:
//!    ```no_run
//!    # use dbiji_grid::*;
//!    # fn f(ui: &mut egui::Ui, model: &mut ResultModel, view: &mut ResultViewState, options: &GridOptions) {
//!    let response = ResultViewer::new(model, view, options).show(ui);
//!    for action in response.actions { /* see below */ }
//!    # }
//!    ```
//!    or compose [`DataGrid`] with the toolbar / panels yourself.
//!    [`GridOptions`] carries [`FormatOptions`](dbiji_core::value::FormatOptions),
//!    the dialect (SQL generation, SQL INSERT copies) and display switches.
//! 4. **Handle actions** ([`GridAction`]):
//!    * `FetchMore` / `FetchAll` — fetch the next page / everything into the store.
//!    * `CountTotal` — run `dialect.count_query(sql)` and set `model.fetch.total_count`.
//!    * `Refresh` (manual or auto refresh) — re-run the query; reuse the model with
//!      [`ResultModel::reset`] to keep virtual columns and layout.
//!    * `SaveChanges` — show `model.change_preview_sql(dialect)` (PRD §10),
//!      execute `model.change_statements(dialect)` (parameterised with
//!      `Dialect::placeholder`) in one transaction, then call
//!      `model.apply_saved()`; on error keep the changes and show the error.
//!    * `ServerQueryChanged { where_clause, order_by }` — server side mode:
//!      re-run the query with the new `WHERE` / `ORDER BY` (e.g. via
//!      `Dialect::select_table` or by wrapping the query). Client side mode
//!      is handled entirely inside the grid on a background thread.
//!    * `NavigateForeignKey { link, values }` — open `link.ref_table` filtered by
//!      `link.ref_columns = values` (keep a back/forward history, FR-D6).
//!    * `RequestReferences { relation, values }` — load related rows and call
//!      `view.references.set_related_rows(...)`.
//!    * `PasteRequested` — read the clipboard and call [`DataGrid::paste_text`]
//!      (Cmd+V is handled by the grid itself).
//!    * `Export { rows, columns }`, `GeneratedSql`, `ShowChart`, `ShowSpatial`,
//!      `SaveValueToFile`, `DefineVirtualKey` — features owned by the application.
//!    * `Notify` — status bar messages; `SelectionChanged`, `Copied`,
//!      `SwitchView`, `TogglePanel`, `AutoRefreshChanged`, `ChangesDiscarded`
//!      are informational.
//! 5. **Persist layouts** by serialising [`DataGridState`] / [`ResultViewState`]
//!    (column widths, order, visibility, formats, sort, toolbar settings).
//!
//! ## Performance
//!
//! Per frame the grid formats and paints only visible cells, locks the row
//! store once for the visible slice, and composes cell text from a glyph
//! cache instead of shaping every new string (`grid/text.rs`). Sorting,
//! filtering and grouping of fetched rows run on worker threads that lock the
//! store in short chunks. Nothing requests a repaint while idle (except the
//! auto refresh timer and transient status messages).
//!
//! All user visible text goes through [`dbiji_core::tr`] / [`dbiji_core::trf`];
//! colours derive from `ui.visuals()`; icons come from `egui-phosphor`
//! (the application installs the font).
#![warn(missing_docs)]

pub mod calc;
pub mod copy;
pub mod expr;
pub mod format;
pub mod grid;
pub mod grouping;
pub mod model;
pub mod panels;
pub mod result_view;
pub mod task;
pub mod theme;
pub mod toolbar;
pub mod valueview;
pub mod view;
pub mod views;

#[cfg(test)]
mod i18n_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod ui_tests;

pub use grid::{
    DataGrid, DataGridState, DataMode, GridAction, GridOptions, GridResponse, PanelKind, ResultViewKind, SqlKind,
};
pub use model::{EditTarget, ReadOnlyReason, ResultModel, determine_editability};
pub use result_view::{ResultViewResponse, ResultViewState, ResultViewer};
