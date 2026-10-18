//! Headless execution for `dbiji --run-task <id|name>` (also what the
//! operating system scheduler invokes).

use crate::engine::{Engine, SystemClock, TaskEvent};
use crate::executor::TaskExecutor;
use crate::model::{TaskRun, Trigger};
use crate::runlog::RunLog;
use crate::store::TaskStore;
use dbiji_core::{DbError, DbResult, tr};
use std::sync::Arc;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio_util::sync::CancellationToken;

/// Callback receiving the events of a CLI run (e.g. to print messages).
pub type EventCallback = Box<dyn Fn(&TaskEvent) + Send + Sync>;

/// Run one task (found by id, `folder/name` or unique name) to completion
/// and return its final record; use [`TaskRun::exit_code`] for the process
/// exit status. Composite tasks run their steps. `cancel` (e.g. wired to
/// Ctrl-C) cancels the run. The run is written to the run log.
pub async fn run_task_once(
    store: Arc<TaskStore>,
    log: Arc<RunLog>,
    executor: Arc<dyn TaskExecutor>,
    id_or_name: &str,
    trigger: Trigger,
    on_event: Option<EventCallback>,
    cancel: CancellationToken,
) -> DbResult<TaskRun> {
    let task = store.find(id_or_name)?;
    task.validate()?;
    let handle = tokio::runtime::Handle::try_current().map_err(|_| DbError::internal(tr("No async runtime is available")))?;
    let engine = Engine::new(store, log, executor, Arc::new(SystemClock), handle);
    let mut rx = engine.subscribe();
    let (run_id, join) = engine.start(task, trigger, None)?;
    let forward = |ev: &TaskEvent| {
        if let Some(f) = &on_event {
            f(ev);
        }
    };
    tokio::pin!(join);
    let mut cancel_sent = false;
    let run = loop {
        tokio::select! {
            r = &mut join => break r.map_err(|_| DbError::internal(tr("The task failed unexpectedly (internal error)")))?,
            _ = cancel.cancelled(), if !cancel_sent => {
                cancel_sent = true;
                engine.cancel(&run_id);
            }
            ev = rx.recv() => match ev {
                Ok(e) => forward(&e),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {}
            },
        }
    };
    loop {
        match rx.try_recv() {
            Ok(e) => forward(&e),
            Err(TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    Ok(run)
}
