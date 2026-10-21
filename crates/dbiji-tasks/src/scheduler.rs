//! Background scheduler running tasks at their scheduled times.

use crate::engine::{Clock, Engine, RunningInfo, SystemClock, TaskEvent};
use crate::executor::TaskExecutor;
use crate::model::{TaskDef, TaskRun, Trigger};
use crate::runlog::RunLog;
use crate::schedule::Schedule;
use crate::store::TaskStore;
use chrono::{DateTime, Local};
use dbiji_core::{DbError, DbResult, tr, trf};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Scheduler behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerOptions {
    /// At start-up, run once every task whose scheduled time passed while
    /// DBiji was not running (otherwise such runs are skipped).
    pub run_missed_on_startup: bool,
    /// Record a skipped run when a task is due while its previous run is
    /// still active.
    pub record_skipped: bool,
    /// Longest sleep between checks (bounds the reaction to wall clock
    /// changes such as a laptop waking up).
    pub max_sleep: Duration,
    /// At start-up, mark runs left `running` by an exited process as failed.
    pub mark_interrupted_on_start: bool,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        SchedulerOptions { run_missed_on_startup: false, record_skipped: true, max_sleep: Duration::from_secs(60), mark_interrupted_on_start: true }
    }
}

#[derive(Default)]
struct State {
    /// task id -> next due time
    next_due: HashMap<String, DateTime<Local>>,
    /// task id -> schedule the due time was computed from
    known: HashMap<String, Schedule>,
}

struct Inner {
    engine: Arc<Engine>,
    options: SchedulerOptions,
    state: Mutex<State>,
    wake: Notify,
    shutdown: CancellationToken,
    loop_handle: Mutex<Option<JoinHandle<()>>>,
    handle: Handle,
}

/// Runs scheduled tasks in the background and manual runs on request.
///
/// ```ignore
/// let scheduler = Scheduler::new(store, run_log, executor, SchedulerOptions::default(), runtime.handle().clone());
/// let mut events = scheduler.subscribe();
/// scheduler.start()?;
/// let run_id = scheduler.run_now(&task_id, Trigger::Manual)?;
/// scheduler.cancel(&run_id);
/// scheduler.shutdown(Duration::from_secs(5)).await;
/// ```
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler").field("options", &self.inner.options).field("running", &self.inner.engine.running().len()).finish()
    }
}

fn scheduled(t: &TaskDef) -> Option<&Schedule> {
    if t.enabled { t.schedule.as_ref() } else { None }
}

impl Scheduler {
    /// Scheduler using the system clock. Runs are spawned on `handle`.
    pub fn new(store: Arc<TaskStore>, log: Arc<RunLog>, executor: Arc<dyn TaskExecutor>, options: SchedulerOptions, handle: Handle) -> Scheduler {
        Self::with_clock(store, log, executor, options, handle, Arc::new(SystemClock))
    }

    /// Scheduler with a custom clock (see [`crate::TokioClock`] for tests).
    pub fn with_clock(store: Arc<TaskStore>, log: Arc<RunLog>, executor: Arc<dyn TaskExecutor>, options: SchedulerOptions, handle: Handle, clock: Arc<dyn Clock>) -> Scheduler {
        let engine = Engine::new(store, log, executor, clock, handle.clone());
        Scheduler {
            inner: Arc::new(Inner {
                engine,
                options,
                state: Mutex::new(State::default()),
                wake: Notify::new(),
                shutdown: CancellationToken::new(),
                loop_handle: Mutex::new(None),
                handle,
            }),
        }
    }

    /// Start the background loop.
    pub fn start(&self) -> DbResult<()> {
        let mut h = self.inner.loop_handle.lock();
        if h.is_some() {
            return Err(DbError::config(tr("The scheduler is already running")));
        }
        if self.inner.shutdown.is_cancelled() {
            return Err(DbError::config(tr("The scheduler has been shut down")));
        }
        let inner = Arc::clone(&self.inner);
        *h = Some(self.inner.handle.spawn(async move { run_loop(inner).await }));
        Ok(())
    }

    /// The background loop runs (tasks start on their schedule).
    pub fn is_started(&self) -> bool {
        self.inner.loop_handle.lock().as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stop starting tasks on their schedule (runs in progress go on; `start` resumes).
    pub fn stop_schedule(&self) {
        if let Some(h) = self.inner.loop_handle.lock().take() {
            h.abort();
        }
    }

    /// Events of all runs (started, messages, progress, finished).
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.inner.engine.subscribe()
    }

    /// Start a run now. Returns the run id; fails when the task does not
    /// exist or is already running.
    pub fn run_now(&self, task_id: &str, trigger: Trigger) -> DbResult<String> {
        if self.inner.shutdown.is_cancelled() {
            return Err(DbError::config(tr("The scheduler has been shut down")));
        }
        let task = self.inner.engine.store.get(task_id).ok_or_else(|| DbError::config(trf("No task with id {}", &[&task_id])))?;
        task.validate()?;
        let (run_id, _) = self.inner.engine.start(task, trigger, None)?;
        Ok(run_id)
    }

    /// Wait for a run to finish (returns its final record, `None` if unknown).
    pub async fn wait(&self, run_id: &str) -> Option<TaskRun> {
        let mut rx = self.subscribe();
        loop {
            if !self.inner.engine.running().iter().any(|r| r.run_id == run_id) {
                return self.inner.engine.log.get(run_id).ok().flatten();
            }
            match rx.recv().await {
                Ok(TaskEvent::Finished { run }) if run.run_id == run_id => return Some(run),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Cancel a run. Returns false when it is not running.
    pub fn cancel(&self, run_id: &str) -> bool {
        self.inner.engine.cancel(run_id)
    }

    /// Runs in progress.
    pub fn running(&self) -> Vec<RunningInfo> {
        self.inner.engine.running()
    }

    /// Next scheduled time of every scheduled task (task id -> time).
    pub fn next_runs(&self) -> BTreeMap<String, DateTime<Local>> {
        self.inner.state.lock().next_due.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Re-read schedules now (store changes are picked up automatically).
    pub fn reschedule(&self) {
        self.inner.wake.notify_one();
    }

    /// Stop the loop, cancel running tasks and wait up to `grace` for them to finish.
    pub async fn shutdown(&self, grace: Duration) {
        self.inner.shutdown.cancel();
        self.inner.engine.cancel_all();
        let deadline = tokio::time::Instant::now() + grace;
        while !self.inner.engine.running().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let h = self.inner.loop_handle.lock().take();
        if let Some(h) = h {
            let _ = tokio::time::timeout(grace.max(Duration::from_millis(100)), h).await;
        }
    }
}

/// Compute the due times of all scheduled tasks at start-up.
fn initial_schedule(inner: &Inner) {
    let engine = &inner.engine;
    let now = engine.clock.now();
    let last = engine.log.latest_by_task().unwrap_or_else(|e| {
        tracing::warn!("cannot read the task run log: {e}");
        HashMap::new()
    });
    let mut st = inner.state.lock();
    for t in engine.store.list() {
        let Some(s) = scheduled(&t) else { continue };
        if s.validate().is_err() {
            continue;
        }
        let anchor = last.get(&t.id).map(|r| r.started.with_timezone(&Local)).unwrap_or_else(|| t.updated.with_timezone(&Local));
        // an anchor in the future (clock skew) counts as now
        let anchor = if anchor > now { now } else { anchor };
        let due = match s.next_after(anchor) {
            Some(d) if d <= now => {
                if inner.options.run_missed_on_startup {
                    Some(now)
                } else {
                    s.next_after_skipping(&anchor, &now)
                }
            }
            other => other,
        };
        if let Some(d) = due {
            st.next_due.insert(t.id.clone(), d);
        }
        st.known.insert(t.id.clone(), s.clone());
    }
}

/// Pick up added, removed and changed tasks.
fn sync_tasks(inner: &Inner) {
    let now = inner.engine.clock.now();
    let tasks = inner.engine.store.list();
    let mut st = inner.state.lock();
    let mut seen = Vec::new();
    for t in &tasks {
        let Some(s) = scheduled(t).filter(|s| s.validate().is_ok()) else { continue };
        seen.push(t.id.clone());
        if st.known.get(&t.id) != Some(s) {
            st.known.insert(t.id.clone(), s.clone());
            match s.next_after(now) {
                Some(d) => {
                    st.next_due.insert(t.id.clone(), d);
                }
                None => {
                    st.next_due.remove(&t.id);
                }
            }
        }
    }
    st.known.retain(|id, _| seen.contains(id));
    st.next_due.retain(|id, _| seen.contains(id));
}

async fn run_loop(inner: Arc<Inner>) {
    let engine = Arc::clone(&inner.engine);
    if inner.options.mark_interrupted_on_start
        && let Err(e) = engine.log.mark_interrupted(std::process::id())
    {
        tracing::warn!("cannot update the task run log: {e}");
    }
    let mut changes = engine.store.subscribe();
    changes.borrow_and_update();
    initial_schedule(&inner);
    loop {
        if inner.shutdown.is_cancelled() {
            break;
        }
        let now = engine.clock.now();
        let due: Vec<(String, DateTime<Local>)> = {
            let st = inner.state.lock();
            let mut v: Vec<(String, DateTime<Local>)> = st.next_due.iter().filter(|(_, t)| **t <= now).map(|(k, v)| (k.clone(), *v)).collect();
            v.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            v
        };
        for (id, at) in due {
            let task = engine.store.get(&id);
            let schedule = task.as_ref().and_then(|t| scheduled(t).cloned());
            let (Some(task), Some(schedule)) = (task, schedule) else {
                inner.state.lock().next_due.remove(&id);
                continue;
            };
            if engine.is_task_running(&task.id) {
                if inner.options.record_skipped {
                    engine.record_skipped(&task, Trigger::Schedule, tr("Skipped: the previous run is still in progress"), None);
                }
            } else if let Err(e) = task.validate().and_then(|_| engine.start(task.clone(), Trigger::Schedule, None).map(|_| ())) {
                tracing::warn!("scheduled task {} could not start: {e}", task.name);
            }
            let mut st = inner.state.lock();
            match schedule.next_after_skipping(&at, &now) {
                Some(next) => {
                    st.next_due.insert(id.clone(), next);
                }
                None => {
                    st.next_due.remove(&id);
                }
            }
        }
        let sleep_for = {
            let st = inner.state.lock();
            st.next_due.values().min().map(|t| (*t - now).to_std().unwrap_or(Duration::ZERO)).unwrap_or(inner.options.max_sleep).min(inner.options.max_sleep)
        };
        tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            _ = tokio::time::sleep(sleep_for) => {}
            _ = inner.wake.notified() => sync_tasks(&inner),
            r = changes.changed() => {
                if r.is_ok() {
                    changes.borrow_and_update();
                    sync_tasks(&inner);
                }
            }
        }
    }
}
