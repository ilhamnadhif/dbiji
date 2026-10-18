//! Schema compare, data compare, reports and Liquibase changelogs for DBiji
//! (PRD FR-M3).
//!
//! * [`schema_diff`] compares two [`SchemaSnapshot`]s,
//! * [`migration_script`] / [`migration_plan`] produce the DDL that makes the
//!   target identical to the source, in dependency-safe order, through the
//!   target dialect's [`DdlGenerator`](dbiji_core::ddl::DdlGenerator),
//! * [`apply_plan`] simulates a plan on a schema model (used to verify plans),
//! * [`html_report`], [`liquibase_changelog`] and [`diagram_status`] render a diff,
//! * [`data`] compares the rows of two tables and generates sync statements.
//!
//! # Gate test recipe
//!
//! The PRD gate ("schema compare produces DDL that, when executed, makes the
//! two schemas identical") is checked with real drivers like this:
//!
//! ```ignore
//! let a = session_a.schema_snapshot(&schema_a, &SnapshotOptions::full()).await?;
//! let b = session_b.schema_snapshot(&schema_b, &SnapshotOptions::full()).await?;
//! let diff = schema_diff(&a, &b, &CompareOptions::default());
//! for sql in migration_script(&diff, &a, &b, &*session_b.dialect(), &ScriptOptions::full_sync()) {
//!     execute_update(&mut *session_b, &sql, &[]).await?;
//! }
//! let b2 = session_b.schema_snapshot(&schema_b, &SnapshotOptions::full()).await?;
//! assert!(!schema_diff(&a, &b2, &CompareOptions::default()).has_differences());
//! ```

// `DbError` (dbiji-core) is 144 bytes; every `DbResult` triggers this lint.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

mod analysis;
pub mod data;
mod diff;
mod liquibase;
mod model;
mod normalize;
mod options;
mod plan;
mod report;
mod simulate;
mod util;

#[cfg(test)]
mod tests;

pub use data::{
    CompareStrategy, DataCompareJob, DataCompareOptions, DataCompareProgress, DataDiff, RowDiff, RowDiffKind, RowProvider,
    TableSpec, run_data_compare,
};
pub use diff::schema_diff;
pub use liquibase::{LiquibaseOptions, liquibase_changelog};
pub use model::{DiffItem, DiffStatus, DiffSummary, PropertyChange, SchemaDiff, StatusCounts};
pub use options::{ColumnRename, CompareOptions, TypeNormalizer};
pub use plan::{AlterMode, MigrationOp, MigrationPlan, MigrationStep, Phase, ScriptOptions, migration_plan};
pub use report::{diagram_status, html_report};
pub use simulate::apply_plan;

use dbiji_core::Dialect;
use dbiji_core::ddl::SchemaSnapshot;

/// Statements that make `target` identical to `source` for the objects listed
/// in `diff` (remove items from the diff to exclude them), in dependency-safe
/// order, qualified with the target schema. Comments and transaction wrapping
/// follow `opts`. See [`migration_plan`] for the structured form.
pub fn migration_script(
    diff: &SchemaDiff,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    target_dialect: &dyn Dialect,
    opts: &ScriptOptions,
) -> Vec<String> {
    migration_plan(diff, source, target, target_dialect, opts).script(opts.include_comments)
}
