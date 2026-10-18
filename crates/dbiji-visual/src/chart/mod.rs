//! Charts from result sets (FR-D8): line, bar, area, scatter and pie.
//!
//! [`ChartData::build`] turns columns and rows into series according to a
//! [`ChartConfig`]; [`show_chart`] draws them, [`chart_config_ui`] edits the
//! configuration and [`export_chart_svg`] writes an image.

mod config;
mod data;
mod svg;
mod view;

pub use config::{ChartAggregate, ChartConfig, ChartKind};
pub use data::{ChartData, ChartSeries, PieSlice, XKind};
pub use svg::{export_chart_png, export_chart_svg};
pub use view::{ChartState, chart_config_ui, show_chart};
