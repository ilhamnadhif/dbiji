//! Entity relationship diagrams (FR-M2, FR-M3 diagram diff).
//!
//! * [`ErModel`] – tables, columns and relations, built from a
//!   [`SchemaSnapshot`](dbiji_core::ddl::SchemaSnapshot) or table definitions.
//! * [`ErDiagram`] – a saved diagram: model + positions + notes + settings.
//! * [`auto_layout`] / [`layout_incremental`] – automatic layered layout.
//! * [`show_er`] – the interactive canvas returning [`ErEvent`]s.
//! * [`export_svg`] / [`export_png`] – image export.

mod diagram;
mod export;
mod geometry;
mod layout;
mod model;
pub(crate) mod scene;
mod view;

pub use diagram::{ColumnDisplay, ErDiagram, ErNote, ErSettings, Notation};
pub use export::{export_png, export_svg};
pub use view::{ErDrag, ErEvent, ErHover, ErResponse, ErViewState, ZOOM_RANGE, show_er};
pub use geometry::{TableGeometry, table_geometry};
pub use layout::{LayoutDirection, LayoutOptions, auto_layout, layout_incremental};
pub use model::{DiffStatus, ErColumn, ErModel, ErRelation, ErTable, ErTableKind, table_key};
