//! Side panels (FR-D5): Value, Metadata, Grouping, Calc and References.

mod calc;
mod grouping;
mod metadata;
mod references;
mod value;

pub use calc::{CalcPanel, CalcPanelState, MAX_CALC_CELLS, selection_stats};
pub use grouping::{GroupingAction, GroupingPanel, GroupingPanelState, MAX_GROUPS};
pub use metadata::{MetadataPanel, metadata_rows};
pub use references::{ReferencesPanel, ReferencesPanelState, Relation, RelationDirection};
pub use value::{ValueMode, ValuePanel, ValuePanelState};
