//! Tasks for DBiji (PRD FR-A3): save, run and schedule operations (SQL
//! scripts, export, import, backup, restore, migration, schema compare,
//! shell commands), composite tasks and an execution log.
//!
//! * [`TaskDef`] / [`TaskKind`] / [`Schedule`]: the saved definitions,
//! * [`TaskStore`]: `tasks.json` with folders, import / export,
//! * [`RunLog`]: run history as JSON lines with rotation,
//! * [`TaskExecutor`]: implemented by the application to execute a task,
//! * [`Scheduler`]: background loop, manual runs, cancellation, events,
//! * [`run_task_once`]: headless runs for `dbiji --run-task <id|name>`,
//! * [`os`]: launchd / crontab / schtasks integration so tasks also run
//!   while DBiji is closed (opt-in per task).

// `DbError` (dbiji-core) is 144 bytes; every `DbResult` triggers this lint.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

mod cli;
mod engine;
mod executor;
mod model;
pub mod os;
mod runlog;
mod schedule;
mod scheduler;
mod store;

#[cfg(test)]
mod tests;

pub use cli::{EventCallback, run_task_once};
pub use engine::{Clock, RunningInfo, SystemClock, TaskEvent, TokioClock};
pub use executor::{Progress, RunContext, RunSignal, RunSink, TaskExecutor};
pub use model::{KEEP_FIRST_MESSAGES, MAX_MESSAGE_CHARS, MAX_RUN_MESSAGES, MessageLevel, NotifyOptions, RunMessage, RunOutcome, RunStatus, TaskDef, TaskKind, TaskRun, Trigger};
pub use runlog::{RunLog, RunLogOptions};
pub use schedule::{Schedule, ScheduleError};
pub use scheduler::{Scheduler, SchedulerOptions};
pub use store::TaskStore;
