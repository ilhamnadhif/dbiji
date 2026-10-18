//! Visual query builder (FR-S10).
//!
//! [`QueryModel`] holds tables, joins, select list, conditions, grouping and
//! ordering; [`QueryModel::to_sql`] renders it for any dialect;
//! [`show_query_builder`] is the editor. The application answers
//! [`QbEvent::AddTableRequested`] with a table picker and calls
//! [`QueryModel::add_table`] with the chosen table definition (joins are
//! created automatically from foreign keys).

mod model;
mod sql;
mod view;

pub use model::{
    Aggregate, Connector, JoinKind, OPERATORS, QbColumn, QbCondition, QbJoin, QbSelectItem, QbTable, QueryModel,
};
pub use view::{QbEvent, QbResponse, QbTab, QbViewState, show_query_builder};
