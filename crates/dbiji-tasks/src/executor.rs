//! The interface the application implements to execute tasks.

use crate::model::{MessageLevel, RunMessage, RunOutcome, TaskDef, Trigger};
use async_trait::async_trait;
use chrono::Utc;
use std::fmt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Executes one (non composite) task. Implemented by the application, which
/// knows connections, transfer jobs, backup tools and shells. Composite tasks
/// are run by the scheduler itself.
///
/// Implementations should check [`RunContext::cancel`] regularly; a cancelled
/// or timed out run is also aborted at its next `.await`.
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Run `task`; `Err` carries the message shown to the user.
    async fn execute(&self, task: &TaskDef, ctx: RunContext) -> Result<RunOutcome, String>;
}

/// Progress of a running task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// Units done (rows, bytes, statements ...).
    pub done: u64,
    /// Total units, when known.
    pub total: Option<u64>,
    /// What is being worked on (table name, file ...).
    pub label: Option<String>,
}

/// Something a running task reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSignal {
    /// A log message.
    Message(RunMessage),
    /// A progress update.
    Progress(Progress),
}

/// Receiver of [`RunSignal`]s.
pub type RunSink = Arc<dyn Fn(RunSignal) + Send + Sync>;

/// Context handed to [`TaskExecutor::execute`].
#[derive(Clone)]
pub struct RunContext {
    /// Id of this run.
    pub run_id: String,
    /// Task being run.
    pub task_id: String,
    /// What started the run.
    pub trigger: Trigger,
    /// Cancelled when the user cancels the run, the run times out or the
    /// scheduler shuts down.
    pub cancel: CancellationToken,
    sink: RunSink,
}

impl fmt::Debug for RunContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("task_id", &self.task_id)
            .field("trigger", &self.trigger)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl RunContext {
    /// Context whose messages and progress go to `sink`.
    pub fn new(run_id: impl Into<String>, task_id: impl Into<String>, trigger: Trigger, cancel: CancellationToken, sink: RunSink) -> Self {
        RunContext { run_id: run_id.into(), task_id: task_id.into(), trigger, cancel, sink }
    }

    /// Context that discards messages (for executor unit tests).
    pub fn detached(task: &TaskDef) -> Self {
        Self::new(uuid::Uuid::new_v4().to_string(), task.id.clone(), Trigger::Manual, CancellationToken::new(), Arc::new(|_| {}))
    }

    /// Log a message to the run.
    pub fn log(&self, level: MessageLevel, text: impl Into<String>) {
        (self.sink)(RunSignal::Message(RunMessage { time: Utc::now(), level, text: text.into() }));
    }

    /// Log an informational message.
    pub fn info(&self, text: impl Into<String>) {
        self.log(MessageLevel::Info, text);
    }

    /// Log a warning.
    pub fn warn(&self, text: impl Into<String>) {
        self.log(MessageLevel::Warning, text);
    }

    /// Log an error (the run continues; return `Err` to fail it).
    pub fn error(&self, text: impl Into<String>) {
        self.log(MessageLevel::Error, text);
    }

    /// Report progress.
    pub fn progress(&self, done: u64, total: Option<u64>, label: Option<&str>) {
        (self.sink)(RunSignal::Progress(Progress { done, total, label: label.map(str::to_string) }));
    }

    /// The run should stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}
