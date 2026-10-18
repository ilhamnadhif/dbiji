//! Run machinery shared by the [`crate::Scheduler`] and [`crate::run_task_once`]:
//! no overlapping runs of a task, time limits, cancellation, panic isolation,
//! composite tasks, run log records and events.

use crate::executor::{Progress, RunContext, RunSignal, TaskExecutor};
use crate::model::{MessageLevel, RunMessage, RunOutcome, RunStatus, TaskDef, TaskKind, TaskRun, Trigger};
use crate::runlog::RunLog;
use crate::store::TaskStore;
use chrono::{DateTime, Local, Utc};
use dbiji_core::{DbError, DbResult, tr, trf};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// Source of wall clock time (replaceable in tests).
pub trait Clock: Send + Sync {
    /// Current local time.
    fn now(&self) -> DateTime<Local>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// Wall clock that advances with tokio's clock (which tests can pause and
/// advance): `base` plus the tokio time elapsed since creation.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    base: DateTime<Local>,
    start: tokio::time::Instant,
}

impl TokioClock {
    /// Clock reading `base` now and advancing with tokio time.
    pub fn new(base: DateTime<Local>) -> Self {
        TokioClock { base, start: tokio::time::Instant::now() }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Local> {
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.start);
        let d = chrono::Duration::from_std(elapsed).unwrap_or(chrono::Duration::zero());
        self.base.checked_add_signed(d).unwrap_or(self.base)
    }
}

/// Notification for the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEvent {
    /// A run started.
    Started {
        /// The run record (in `running` state).
        run: TaskRun,
    },
    /// A running task logged a message.
    Message {
        /// Run that logged it.
        run_id: String,
        /// Task of the run.
        task_id: String,
        /// The message.
        message: RunMessage,
    },
    /// A running task reported progress.
    Progress {
        /// Run that reported it.
        run_id: String,
        /// Task of the run.
        task_id: String,
        /// The progress.
        progress: Progress,
    },
    /// A run finished (also emitted for skipped runs).
    Finished {
        /// The final run record.
        run: TaskRun,
    },
}

/// A run in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningInfo {
    /// Id of the run (for [`crate::Scheduler::cancel`]).
    pub run_id: String,
    /// Task being run.
    pub task_id: String,
    /// Name of the task.
    pub task_name: String,
    /// What started the run.
    pub trigger: Trigger,
    /// Start time.
    pub started: DateTime<Utc>,
    /// Composite run this run is a step of.
    pub parent_run_id: Option<String>,
}

struct Active {
    info: RunningInfo,
    cancel: CancellationToken,
}

/// Composite run starting a step.
#[derive(Clone)]
pub(crate) struct Parent {
    pub run_id: String,
    pub cancel: CancellationToken,
    /// Ids of composite tasks on the way to this step (cycle protection).
    pub ancestors: Vec<String>,
}

enum Failure {
    Cancelled(String),
    TimedOut(Duration),
    Error(String),
}

pub(crate) struct Engine {
    pub store: Arc<TaskStore>,
    pub log: Arc<RunLog>,
    executor: Arc<dyn TaskExecutor>,
    pub clock: Arc<dyn Clock>,
    events: broadcast::Sender<TaskEvent>,
    active: Mutex<HashMap<String, Active>>,
    handle: Handle,
}

impl Engine {
    pub fn new(store: Arc<TaskStore>, log: Arc<RunLog>, executor: Arc<dyn TaskExecutor>, clock: Arc<dyn Clock>, handle: Handle) -> Arc<Engine> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Engine { store, log, executor, clock, events, active: Mutex::new(HashMap::new()), handle })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    fn emit(&self, e: TaskEvent) {
        let _ = self.events.send(e);
    }

    fn now_utc(&self) -> DateTime<Utc> {
        self.clock.now().with_timezone(&Utc)
    }

    fn write(&self, run: &TaskRun) {
        if let Err(e) = self.log.record(run) {
            tracing::warn!("cannot write the task run log: {e}");
        }
    }

    pub fn running(&self) -> Vec<RunningInfo> {
        let mut v: Vec<RunningInfo> = self.active.lock().values().map(|a| a.info.clone()).collect();
        v.sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.run_id.cmp(&b.run_id)));
        v
    }

    pub fn is_task_running(&self, task_id: &str) -> bool {
        self.active.lock().values().any(|a| a.info.task_id == task_id)
    }

    /// Cancel a run (and, for composites, its steps).
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.active.lock().get(run_id) {
            Some(a) => {
                a.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn cancel_all(&self) {
        for a in self.active.lock().values() {
            a.cancel.cancel();
        }
    }

    /// Record (and announce) a run that did not happen.
    pub fn record_skipped(&self, task: &TaskDef, trigger: Trigger, reason: &str, parent_run_id: Option<String>) -> TaskRun {
        let now = self.now_utc();
        let mut run = TaskRun::start(task, trigger, now, parent_run_id);
        run.status = RunStatus::Skipped;
        run.finished = Some(now);
        run.push_message(RunMessage { time: now, level: MessageLevel::Warning, text: reason.to_string() });
        self.write(&run);
        self.emit(TaskEvent::Finished { run: run.clone() });
        run
    }

    /// Start a run in the background. Fails when the task is already running.
    pub fn start(self: &Arc<Self>, task: TaskDef, trigger: Trigger, parent: Option<Parent>) -> DbResult<(String, JoinHandle<TaskRun>)> {
        let cancel = parent.as_ref().map(|p| p.cancel.child_token()).unwrap_or_default();
        let run = TaskRun::start(&task, trigger, self.now_utc(), parent.as_ref().map(|p| p.run_id.clone()));
        {
            let mut active = self.active.lock();
            if active.values().any(|a| a.info.task_id == task.id) {
                return Err(DbError::query(trf("The task {} is already running", &[&task.name])).with_code("already_running"));
            }
            active.insert(
                run.run_id.clone(),
                Active {
                    info: RunningInfo {
                        run_id: run.run_id.clone(),
                        task_id: task.id.clone(),
                        task_name: task.name.clone(),
                        trigger,
                        started: run.started,
                        parent_run_id: run.parent_run_id.clone(),
                    },
                    cancel: cancel.clone(),
                },
            );
        }
        self.write(&run);
        self.emit(TaskEvent::Started { run: run.clone() });
        let run_id = run.run_id.clone();
        let ancestors = parent.map(|p| p.ancestors).unwrap_or_default();
        let engine = Arc::clone(self);
        let handle = self.handle.spawn(async move { engine.drive(task, run, cancel, ancestors).await });
        Ok((run_id, handle))
    }

    async fn drive(self: Arc<Self>, task: TaskDef, run: TaskRun, cancel: CancellationToken, ancestors: Vec<String>) -> TaskRun {
        let shared = Arc::new(Mutex::new(run));
        let (run_id, task_id, trigger) = {
            let r = shared.lock();
            (r.run_id.clone(), r.task_id.clone(), r.trigger)
        };
        let sink = {
            let shared = Arc::clone(&shared);
            let events = self.events.clone();
            let (run_id, task_id) = (run_id.clone(), task_id.clone());
            Arc::new(move |signal: RunSignal| match signal {
                RunSignal::Message(m) => {
                    shared.lock().push_message(m.clone());
                    let _ = events.send(TaskEvent::Message { run_id: run_id.clone(), task_id: task_id.clone(), message: m });
                }
                RunSignal::Progress(p) => {
                    let _ = events.send(TaskEvent::Progress { run_id: run_id.clone(), task_id: task_id.clone(), progress: p });
                }
            })
        };
        let ctx = RunContext::new(run_id.clone(), task_id.clone(), trigger, cancel.clone(), sink);
        let timeout = task.timeout();

        let result = if let TaskKind::Composite { steps, stop_on_error, parallel } = &task.kind {
            let mut chain = ancestors;
            chain.push(task.id.clone());
            let parent = Parent { run_id: run_id.clone(), cancel: cancel.clone(), ancestors: chain };
            let work = self.run_composite(steps, *stop_on_error, *parallel, parent, &ctx);
            Self::limit(work, timeout, &cancel).await
        } else {
            let executor = Arc::clone(&self.executor);
            let t = task.clone();
            let exec = self.handle.spawn(async move { executor.execute(&t, ctx).await });
            let abort = exec.abort_handle();
            let work = async move {
                match exec.await {
                    Ok(Ok(o)) => Ok(o),
                    Ok(Err(e)) => Err(Failure::Error(e)),
                    Err(e) if e.is_panic() => Err(Failure::Error(tr("The task failed unexpectedly (internal error)").to_string())),
                    Err(_) => Err(Failure::Cancelled(tr("The run was aborted").to_string())),
                }
            };
            let r = Self::limit(work, timeout, &cancel).await;
            if r.is_err() {
                abort.abort();
            }
            r
        };

        let mut run = shared.lock().clone();
        let now = self.now_utc();
        run.finished = Some(now);
        match result {
            Ok(outcome) => {
                run.status = RunStatus::Succeeded;
                run.rows = outcome.rows;
                run.output_files = outcome.output_files;
                if let Some(m) = outcome.message {
                    run.push_message(RunMessage { time: now, level: MessageLevel::Info, text: m });
                }
            }
            Err(Failure::Cancelled(reason)) => {
                run.status = RunStatus::Cancelled;
                run.error = Some(reason);
            }
            Err(Failure::TimedOut(d)) => {
                run.status = RunStatus::Failed;
                let text = trf("Timed out after {} s", &[&d.as_secs()]);
                run.push_message(RunMessage { time: now, level: MessageLevel::Error, text: text.clone() });
                run.error = Some(text);
            }
            Err(Failure::Error(e)) => {
                run.status = RunStatus::Failed;
                run.push_message(RunMessage { time: now, level: MessageLevel::Error, text: e.clone() });
                run.error = Some(e);
            }
        }
        self.active.lock().remove(&run.run_id);
        self.write(&run);
        self.emit(TaskEvent::Finished { run: run.clone() });
        run
    }

    /// Run `work` until it finishes, the time limit passes or `cancel` fires.
    async fn limit<F>(work: F, timeout: Option<Duration>, cancel: &CancellationToken) -> Result<RunOutcome, Failure>
    where
        F: std::future::Future<Output = Result<RunOutcome, Failure>>,
    {
        let timed = async {
            match timeout {
                Some(d) => match tokio::time::timeout(d, work).await {
                    Ok(r) => r,
                    Err(_) => Err(Failure::TimedOut(d)),
                },
                None => work.await,
            }
        };
        let r = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Failure::Cancelled(tr("Cancelled").to_string())),
            r = timed => r,
        };
        if matches!(r, Err(Failure::TimedOut(_))) {
            // stop steps / helpers that observe the token
            cancel.cancel();
        }
        r
    }

    fn step_task(&self, id: &str, parent: &Parent) -> Result<TaskDef, String> {
        let t = self.store.get(id).ok_or_else(|| trf("Step task {} does not exist", &[&id]))?;
        if parent.ancestors.contains(&t.id) {
            return Err(trf("The composite task contains itself through step {}", &[&t.name]));
        }
        Ok(t)
    }

    async fn run_composite(
        self: &Arc<Self>,
        steps: &[String],
        stop_on_error: bool,
        parallel: bool,
        parent: Parent,
        ctx: &RunContext,
    ) -> Result<RunOutcome, Failure> {
        let mut outcome = RunOutcome::default();
        let mut failures: Vec<String> = Vec::new();
        let absorb = |run: &TaskRun, outcome: &mut RunOutcome, failures: &mut Vec<String>| {
            if let Some(r) = run.rows {
                outcome.rows = Some(outcome.rows.unwrap_or(0) + r);
            }
            outcome.output_files.extend(run.output_files.iter().cloned());
            match run.status {
                RunStatus::Succeeded | RunStatus::Skipped => true,
                _ => {
                    failures.push(trf(
                        "Step {} {}: {}",
                        &[&run.task_name, &tr(run.status.label()).to_lowercase(), &run.error.clone().unwrap_or_default()],
                    ));
                    false
                }
            }
        };
        if parallel {
            // siblings are cancelled through this token when one fails
            let group = Parent { cancel: parent.cancel.child_token(), ..parent.clone() };
            let mut set: JoinSet<Result<TaskRun, String>> = JoinSet::new();
            for id in steps {
                let started = self.step_task(id, &parent).and_then(|t| {
                    let name = t.name.clone();
                    self.start(t, Trigger::Composite, Some(group.clone())).map_err(|e| trf("Step {} could not start: {}", &[&name, &e]))
                });
                match started {
                    Ok((_, h)) => {
                        set.spawn(async move { h.await.map_err(|_| tr("The step failed unexpectedly (internal error)").to_string()) });
                    }
                    Err(e) => {
                        ctx.error(e.clone());
                        failures.push(e);
                        if stop_on_error {
                            group.cancel.cancel();
                        }
                    }
                }
            }
            while let Some(joined) = set.join_next().await {
                match joined.unwrap_or_else(|_| Err(tr("The step failed unexpectedly (internal error)").to_string())) {
                    Ok(run) => {
                        if !absorb(&run, &mut outcome, &mut failures) && stop_on_error {
                            group.cancel.cancel();
                        }
                    }
                    Err(e) => {
                        failures.push(e);
                        if stop_on_error {
                            group.cancel.cancel();
                        }
                    }
                }
            }
        } else {
            for (i, id) in steps.iter().enumerate() {
                if parent.cancel.is_cancelled() {
                    break;
                }
                let started = self.step_task(id, &parent).and_then(|t| {
                    let name = t.name.clone();
                    self.start(t, Trigger::Composite, Some(parent.clone())).map_err(|e| trf("Step {} could not start: {}", &[&name, &e]))
                });
                let ok = match started {
                    Ok((_, h)) => match h.await {
                        Ok(run) => absorb(&run, &mut outcome, &mut failures),
                        Err(_) => {
                            failures.push(tr("The step failed unexpectedly (internal error)").to_string());
                            false
                        }
                    },
                    Err(e) => {
                        ctx.error(e.clone());
                        failures.push(e);
                        false
                    }
                };
                if !ok && stop_on_error {
                    let remaining = &steps[i + 1..];
                    if !remaining.is_empty() {
                        ctx.warn(trf("Skipping {} remaining step(s) after an error", &[&remaining.len()]));
                        for rid in remaining {
                            if let Some(t) = self.store.get(rid) {
                                self.record_skipped(&t, Trigger::Composite, tr("Skipped because a previous step failed"), Some(parent.run_id.clone()));
                            }
                        }
                    }
                    break;
                }
            }
        }
        if parent.cancel.is_cancelled() && failures.is_empty() {
            return Err(Failure::Cancelled(tr("Cancelled").to_string()));
        }
        if !failures.is_empty() {
            return Err(Failure::Error(failures.join("; ")));
        }
        outcome.message = Some(trf("{} step(s) completed", &[&steps.len()]));
        Ok(outcome)
    }
}
