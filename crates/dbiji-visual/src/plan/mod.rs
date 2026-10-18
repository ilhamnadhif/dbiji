//! Execution plans (FR-S9): diagram, explain tree, text view and summary.
//!
//! All views take the [`QueryPlan`](dbiji_core::plan::QueryPlan) produced by
//! `Session::explain` and share one [`PlanViewState`], so selecting a node
//! in the tree selects it in the diagram and vice versa.

mod analysis;
mod diagram;
mod tree;

pub use analysis::{
    FlatPlanNode, LARGE_TABLE_ROWS, PlanHotspot, PlanMetric, PlanNodePath, PlanSummary, PlanWarning, PlanWarningKind,
    flatten_plan, is_full_scan, node_at, plan_summary, summary_line,
};
pub use diagram::{PlanOrientation, PlanResponse, PlanViewState, show_plan_diagram};
pub use tree::{show_plan_text, show_plan_tree};
