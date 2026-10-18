//! Infrastructure shared by the visual tools: text metrics, SVG writing,
//! colors, number formatting, the pan/zoom transform and serde adapters.

pub(crate) mod colors;
pub(crate) mod format;
pub(crate) mod paint;
pub(crate) mod serde_helpers;
pub(crate) mod svg;
pub(crate) mod text;
pub(crate) mod ticks;
pub mod view;
