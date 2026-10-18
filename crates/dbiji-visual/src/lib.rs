//! Visual tools of DBiji, all rendered with egui:
//!
//! * [`er`] – entity relationship diagrams: model built from schema
//!   snapshots, Sugiyama style automatic layout, an interactive (and editable)
//!   canvas, saved custom diagrams, diagram diff colouring and SVG/PNG export.
//! * [`plan`] – execution plan diagram and explain tree with cost/time heat
//!   colouring and a plan summary with warnings.
//! * [`builder`] – the visual query builder and its SQL generator.
//! * [`chart`] – charts over result sets (line, bar, scatter, pie, area).
//! * [`dashboard`] – server health dashboards and custom SQL dashboards.
//! * [`spatial`] – WKT/WKB parsing and a pan/zoom viewer for geometries.
//!
//! The crate contains UI and pure logic only: the application supplies data
//! (schema snapshots, plans, rows, metrics) and turns the returned events
//! into database work.

#![warn(missing_docs)]
// `DbError` (the shared error type of dbiji-core) is large by design.
#![allow(clippy::result_large_err)]

pub mod builder;
pub mod chart;
mod common;
pub mod dashboard;
pub mod er;
pub mod plan;
pub mod spatial;
#[cfg(test)]
mod test_harness;

pub use common::view::ViewTransform;
