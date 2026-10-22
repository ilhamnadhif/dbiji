//! Scheduler and engine behaviour with a scripted executor and paused tokio
//! time (`tokio::time` auto-advances while every task waits on a timer).

use crate::*;
use async_trait::async_trait;
use chrono::{DateTime, Local, TimeZone, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum Behave {
    Ok {
        secs: u64,
        rows: u64,
    },
    Fail {
        secs: u64,
        msg: &'static str,
    },
    /// Waits for cancellation.
    Hang,
    /// Ignores cancellation (must be aborted by the engine).
    Stubborn,
    Panic,
}

struct Call {
    name: String,
    start: DateTime<Local>,
    end: Option<DateTime<Local>>,
}

struct Fake {
    clock: Arc<dyn Clock>,
    plans: HashMap<String, Behave>,
    calls: Mutex<Vec<Call>>,
}

impl Fake {
    fn calls_of(&self, name: &str) -> Vec<(DateTime<Local>, Option<DateTime<Local>>)> {
        self.calls.lock().iter().filter(|c| c.name == name).map(|c| (c.start, c.end)).collect()
    }
    fn names(&self) -> Vec<String> {
        self.calls.lock().iter().map(|c| c.name.clone()).collect()
    }
}

#[async_trait]
impl TaskExecutor for Fake {
    async fn execute(&self, task: &TaskDef, ctx: RunContext) -> Result<RunOutcome, String> {
        let idx = {
            let mut calls = self.calls.lock();
            calls.push(Call { name: task.name.clone(), start: self.clock.now(), end: None });
            calls.len() - 1
        };
        ctx.info(format!("starting {}", task.name));
        ctx.progress(1, Some(2), Some("half"));
        let behave = self.plans.get(&task.name).cloned().unwrap_or(Behave::Ok { secs: 1, rows: 1 });
        let r = match behave {
            Behave::Ok { secs, rows } => {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                Ok(RunOutcome { rows: Some(rows), output_files: vec![PathBuf::from(format!("/out/{}.csv", task.name))], message: Some("done".into()) })
            }
            Behave::Fail { secs, msg } => {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                Err(msg.to_string())
            }
            Behave::Hang => {
                ctx.cancel.cancelled().await;
                Err("stopped".into())
            }
            Behave::Stubborn => {
                tokio::time::sleep(Duration::from_secs(1_000_000)).await;
                Ok(RunOutcome::default())
            }
            Behave::Panic => panic!("executor bug"),
        };
        self.calls.lock()[idx].end = Some(self.clock.now());
        r
    }
}

fn base() -> DateTime<Local> {
    // Monday 2026-01-05 08:00 local
    Local.with_ymd_and_hms(2026, 1, 5, 8, 0, 0).earliest().expect("valid local time")
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<TaskStore>,
    log: Arc<RunLog>,
    fake: Arc<Fake>,
    clock: Arc<TokioClock>,
    sched: Scheduler,
    rx: broadcast::Receiver<TaskEvent>,
}

fn harness(options: SchedulerOptions, plans: &[(&str, Behave)]) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(TaskStore::open(dir.path()).expect("store"));
    let log = Arc::new(RunLog::open(dir.path(), RunLogOptions::default()).expect("log"));
    let clock = Arc::new(TokioClock::new(base()));
    let fake = Arc::new(Fake { clock: clock.clone(), plans: plans.iter().map(|(n, b)| (n.to_string(), b.clone())).collect(), calls: Mutex::new(Vec::new()) });
    let sched = Scheduler::with_clock(store.clone(), log.clone(), fake.clone(), options, tokio::runtime::Handle::current(), clock.clone());
    let rx = sched.subscribe();
    Harness { _dir: dir, store, log, fake, clock, sched, rx }
}

fn shell(name: &str) -> TaskDef {
    TaskDef::new(name, TaskKind::Shell { command: format!("run {name}") })
}

fn composite(name: &str, steps: Vec<String>, stop_on_error: bool, parallel: bool) -> TaskDef {
    TaskDef::new(name, TaskKind::Composite { steps, stop_on_error, parallel })
}

async fn next_event(rx: &mut broadcast::Receiver<TaskEvent>) -> TaskEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(10 * 24 * 3600), rx.recv()).await {
            Ok(Ok(e)) => return e,
            Ok(Err(RecvError::Lagged(_))) => continue,
            other => panic!("no event: {other:?}"),
        }
    }
}

async fn next_finished(rx: &mut broadcast::Receiver<TaskEvent>) -> TaskRun {
    loop {
        if let TaskEvent::Finished { run } = next_event(rx).await {
            return run;
        }
    }
}

async fn finished_of(rx: &mut broadcast::Receiver<TaskEvent>, task_id: &str) -> TaskRun {
    loop {
        let r = next_finished(rx).await;
        if r.task_id == task_id {
            return r;
        }
    }
}

fn secs(a: DateTime<Local>, b: DateTime<Local>) -> i64 {
    (b - a).num_seconds()
}

#[tokio::test(start_paused = true)]
async fn interval_task_runs_on_schedule_with_events_and_log() {
    let mut h = harness(SchedulerOptions::default(), &[("tick", Behave::Ok { secs: 2, rows: 5 })]);
    let t = h.store.create(shell("tick").with_schedule(Schedule::Interval { every_secs: 60 })).unwrap();
    h.sched.start().unwrap();
    assert!(h.sched.start().is_err(), "only one loop");
    // event order of the first run
    let started = next_event(&mut h.rx).await;
    let TaskEvent::Started { run } = started else { panic!("expected Started, got {started:?}") };
    assert_eq!((run.task_id.as_str(), run.status, run.trigger), (t.id.as_str(), RunStatus::Running, Trigger::Schedule));
    assert!(matches!(next_event(&mut h.rx).await, TaskEvent::Message { ref message, .. } if message.text == "starting tick"));
    assert!(matches!(next_event(&mut h.rx).await, TaskEvent::Progress { ref progress, .. } if progress.done == 1 && progress.total == Some(2)));
    let first = next_finished(&mut h.rx).await;
    assert_eq!(first.status, RunStatus::Succeeded);
    assert_eq!(first.rows, Some(5));
    assert_eq!(first.output_files, vec![PathBuf::from("/out/tick.csv")]);
    assert_eq!(first.messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["starting tick", "done"]);
    assert_eq!(first.duration(), Some(chrono::Duration::seconds(2)));
    next_finished(&mut h.rx).await;
    next_finished(&mut h.rx).await;
    let starts: Vec<i64> = h.fake.calls_of("tick").iter().map(|(s, _)| secs(base(), *s)).collect();
    assert_eq!(starts, vec![60, 120, 180]);
    let runs = h.log.runs_for_task(&t.id, 10).unwrap();
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|r| r.status == RunStatus::Succeeded));
    assert_eq!(h.log.latest_status(&t.id).unwrap(), Some(RunStatus::Succeeded));
    let next = h.sched.next_runs()[&t.id];
    assert_eq!(secs(base(), next), 240);
    h.sched.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn runs_of_a_task_never_overlap() {
    let mut h = harness(SchedulerOptions::default(), &[("slow", Behave::Ok { secs: 25, rows: 1 })]);
    let t = h.store.create(shell("slow").with_schedule(Schedule::Interval { every_secs: 10 })).unwrap();
    h.sched.start().unwrap();
    let mut succeeded = 0;
    let mut skipped = 0;
    while succeeded < 2 {
        let r = finished_of(&mut h.rx, &t.id).await;
        match r.status {
            RunStatus::Succeeded => succeeded += 1,
            RunStatus::Skipped => {
                skipped += 1;
                assert!(r.messages[0].text.contains("still in progress"));
            }
            s => panic!("unexpected {s:?}"),
        }
    }
    let calls = h.fake.calls_of("slow");
    for w in calls.windows(2) {
        assert!(w[1].0 >= w[0].1.expect("finished"), "overlapping runs");
    }
    assert_eq!(calls.iter().map(|(s, _)| secs(base(), *s)).collect::<Vec<_>>(), vec![10, 40]);
    assert!(skipped >= 3, "{skipped}");
    // a manual run while one is active is refused
    let mut h2 = harness(SchedulerOptions::default(), &[("hang", Behave::Hang)]);
    let hang = h2.store.create(shell("hang")).unwrap();
    let id = h2.sched.run_now(&hang.id, Trigger::Manual).unwrap();
    let err = h2.sched.run_now(&hang.id, Trigger::Manual).unwrap_err();
    assert_eq!(err.code.as_deref(), Some("already_running"));
    assert!(h2.sched.cancel(&id));
    assert_eq!(next_finished(&mut h2.rx).await.status, RunStatus::Cancelled);
    h.sched.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn composite_runs_steps_in_order_and_stops_on_error() {
    let mut h =
        harness(SchedulerOptions::default(), &[("a", Behave::Ok { secs: 5, rows: 10 }), ("b", Behave::Fail { secs: 1, msg: "disk full" }), ("c", Behave::Ok { secs: 1, rows: 1 })]);
    let a = h.store.create(shell("a")).unwrap();
    let b = h.store.create(shell("b")).unwrap();
    let c = h.store.create(shell("c")).unwrap();
    let comp = h.store.create(composite("all", vec![a.id.clone(), b.id.clone(), c.id.clone()], true, false)).unwrap();
    let run_id = h.sched.run_now(&comp.id, Trigger::Manual).unwrap();
    let done = finished_of(&mut h.rx, &comp.id).await;
    assert_eq!(done.run_id, run_id);
    assert_eq!(done.status, RunStatus::Failed);
    assert!(done.error.as_deref().unwrap_or("").contains("disk full"), "{:?}", done.error);
    assert_eq!(h.fake.names(), vec!["a", "b"]);
    let (a_start, a_end) = h.fake.calls_of("a")[0];
    let (b_start, _) = h.fake.calls_of("b")[0];
    assert!(b_start >= a_end.unwrap() && a_start < b_start);
    // the skipped step is recorded under the composite run
    let c_runs = h.log.runs_for_task(&c.id, 5).unwrap();
    assert_eq!(c_runs.len(), 1);
    assert_eq!(c_runs[0].status, RunStatus::Skipped);
    assert_eq!(c_runs[0].parent_run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(h.log.runs_for_task(&a.id, 5).unwrap()[0].trigger, Trigger::Composite);
    assert!(done.messages.iter().any(|m| m.text.contains("Skipping 1 remaining")));

    // without stop_on_error every step runs
    let comp2 = h.store.create(composite("all-continue", vec![a.id.clone(), b.id.clone(), c.id.clone()], false, false)).unwrap();
    h.sched.run_now(&comp2.id, Trigger::Manual).unwrap();
    let done2 = finished_of(&mut h.rx, &comp2.id).await;
    assert_eq!(done2.status, RunStatus::Failed);
    assert_eq!(h.fake.names(), vec!["a", "b", "a", "b", "c"]);
    assert_eq!(done2.rows, None, "failed runs keep no outcome");
}

#[tokio::test(start_paused = true)]
async fn parallel_composite() {
    let mut h = harness(SchedulerOptions::default(), &[("p1", Behave::Ok { secs: 5, rows: 1 }), ("p2", Behave::Ok { secs: 5, rows: 2 }), ("p3", Behave::Ok { secs: 3, rows: 3 })]);
    let ids: Vec<String> = ["p1", "p2", "p3"].iter().map(|n| h.store.create(shell(n)).unwrap().id).collect();
    let comp = h.store.create(composite("par", ids.clone(), true, true)).unwrap();
    h.sched.run_now(&comp.id, Trigger::Manual).unwrap();
    let done = finished_of(&mut h.rx, &comp.id).await;
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(done.rows, Some(6));
    assert_eq!(done.output_files.len(), 3);
    let starts: Vec<DateTime<Local>> = ["p1", "p2", "p3"].iter().map(|n| h.fake.calls_of(n)[0].0).collect();
    assert!(starts.iter().all(|s| *s == starts[0]), "steps start together");
    assert_eq!(done.duration(), Some(chrono::Duration::seconds(5)));

    // a failing step cancels its siblings
    let mut h = harness(SchedulerOptions::default(), &[("slow", Behave::Hang), ("bad", Behave::Fail { secs: 2, msg: "boom" })]);
    let slow = h.store.create(shell("slow")).unwrap();
    let bad = h.store.create(shell("bad")).unwrap();
    let comp = h.store.create(composite("par-fail", vec![slow.id.clone(), bad.id.clone()], true, true)).unwrap();
    h.sched.run_now(&comp.id, Trigger::Manual).unwrap();
    let done = finished_of(&mut h.rx, &comp.id).await;
    assert_eq!(done.status, RunStatus::Failed);
    assert_eq!(h.log.latest_status(&slow.id).unwrap(), Some(RunStatus::Cancelled));
    assert_eq!(h.log.latest_status(&bad.id).unwrap(), Some(RunStatus::Failed));
}

#[tokio::test(start_paused = true)]
async fn cancellation_and_abort() {
    let mut h = harness(SchedulerOptions::default(), &[("hang", Behave::Hang), ("stubborn", Behave::Stubborn)]);
    let hang = h.store.create(shell("hang")).unwrap();
    let stubborn = h.store.create(shell("stubborn")).unwrap();
    let r1 = h.sched.run_now(&hang.id, Trigger::Manual).unwrap();
    let r2 = h.sched.run_now(&stubborn.id, Trigger::Manual).unwrap();
    assert_eq!(h.sched.running().len(), 2);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(h.sched.cancel(&r1));
    assert!(h.sched.cancel(&r2));
    assert!(!h.sched.cancel("unknown"));
    let a = next_finished(&mut h.rx).await;
    let b = next_finished(&mut h.rx).await;
    assert!(a.status == RunStatus::Cancelled && b.status == RunStatus::Cancelled);
    assert!(h.sched.running().is_empty());
    // the stubborn executor was aborted: it never reached its end
    assert!(h.fake.calls_of("stubborn")[0].1.is_none());
    assert_eq!(secs(base(), a.finished.unwrap().with_timezone(&Local)), 3);
    // cancelling a composite cancels its running step
    let comp = h.store.create(composite("c", vec![hang.id.clone()], true, false)).unwrap();
    let cid = h.sched.run_now(&comp.id, Trigger::Manual).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.sched.running().len(), 2);
    h.sched.cancel(&cid);
    let mut statuses = HashMap::new();
    for _ in 0..2 {
        let r = next_finished(&mut h.rx).await;
        statuses.insert(r.task_id.clone(), r.status);
    }
    assert_eq!(statuses[&comp.id], RunStatus::Cancelled);
    assert_eq!(statuses[&hang.id], RunStatus::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn timeouts_fail_the_run() {
    let mut h = harness(SchedulerOptions::default(), &[("stubborn", Behave::Stubborn), ("hang", Behave::Hang)]);
    let t = h.store.create(shell("stubborn").with_timeout_secs(5)).unwrap();
    h.sched.run_now(&t.id, Trigger::Manual).unwrap();
    let r = next_finished(&mut h.rx).await;
    assert_eq!(r.status, RunStatus::Failed);
    assert_eq!(r.error.as_deref(), Some("Timed out after 5 s"));
    assert_eq!(r.duration(), Some(chrono::Duration::seconds(5)));
    // composite timeout cancels the step
    let hang = h.store.create(shell("hang")).unwrap();
    let comp = h.store.create(composite("c", vec![hang.id.clone()], true, false).with_timeout_secs(7)).unwrap();
    h.sched.run_now(&comp.id, Trigger::Manual).unwrap();
    let c = finished_of(&mut h.rx, &comp.id).await;
    assert_eq!(c.status, RunStatus::Failed);
    assert!(c.error.as_deref().unwrap().contains("Timed out"));
    let step = h.log.latest(&hang.id).unwrap().unwrap();
    assert_eq!(step.status, RunStatus::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn missed_runs_at_startup() {
    // with catch-up: runs immediately
    let mut h = harness(SchedulerOptions { run_missed_on_startup: true, ..Default::default() }, &[]);
    let t = h.store.create(shell("hourly").with_schedule(Schedule::Interval { every_secs: 3600 })).unwrap();
    let mut past = TaskRun::start(&t, Trigger::Schedule, (base() - chrono::Duration::hours(3)).with_timezone(&Utc), None);
    past.status = RunStatus::Succeeded;
    past.finished = Some(past.started);
    h.log.record(&past).unwrap();
    h.sched.start().unwrap();
    next_finished(&mut h.rx).await;
    assert_eq!(secs(base(), h.fake.calls_of("hourly")[0].0), 0);

    // without catch-up: next slot on the interval grid
    let mut h = harness(SchedulerOptions::default(), &[]);
    let t = h.store.create(shell("hourly").with_schedule(Schedule::Interval { every_secs: 3600 })).unwrap();
    let mut past = TaskRun::start(&t, Trigger::Schedule, (base() - chrono::Duration::minutes(150)).with_timezone(&Utc), None);
    past.status = RunStatus::Succeeded;
    h.log.record(&past).unwrap();
    h.sched.start().unwrap();
    next_finished(&mut h.rx).await;
    assert_eq!(secs(base(), h.fake.calls_of("hourly")[0].0), 30 * 60);
}

#[tokio::test(start_paused = true)]
async fn store_changes_wake_the_scheduler() {
    let mut h = harness(SchedulerOptions::default(), &[]);
    h.sched.start().unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    let t = h.store.create(shell("late").with_schedule(Schedule::Interval { every_secs: 10 })).unwrap();
    next_finished(&mut h.rx).await;
    assert_eq!(secs(base(), h.fake.calls_of("late")[0].0), 40);
    // disabling stops further runs; re-enabling schedules from now
    h.store.set_enabled(&t.id, false).unwrap();
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(h.fake.calls_of("late").len(), 1);
    assert!(h.sched.next_runs().is_empty());
    h.store.set_enabled(&t.id, true).unwrap();
    next_finished(&mut h.rx).await;
    let starts: Vec<i64> = h.fake.calls_of("late").iter().map(|(s, _)| secs(base(), *s)).collect();
    assert_eq!(starts, vec![40, 151]);
    // a deleted task is forgotten
    h.store.delete(&t.id).unwrap();
    h.sched.reschedule();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(h.sched.next_runs().is_empty());
}

#[tokio::test(start_paused = true)]
async fn cron_and_daily_schedules_use_wall_clock() {
    let mut h = harness(SchedulerOptions::default(), &[]);
    let c = h.store.create(shell("cron").with_schedule(Schedule::Cron("30 8 * * 1-5".into()))).unwrap();
    let d = h.store.create(shell("daily").with_schedule(Schedule::Daily { at: "09:15".into(), weekdays: vec![] })).unwrap();
    h.sched.start().unwrap();
    let first = next_finished(&mut h.rx).await;
    assert_eq!(first.task_id, c.id);
    assert_eq!(secs(base(), h.fake.calls_of("cron")[0].0), 30 * 60);
    let second = next_finished(&mut h.rx).await;
    assert_eq!(second.task_id, d.id);
    assert_eq!(secs(base(), h.fake.calls_of("daily")[0].0), 75 * 60);
    let next = h.sched.next_runs();
    assert_eq!(secs(base(), next[&c.id]), 24 * 3600 + 30 * 60);
    let _ = h.clock.now();
}

#[tokio::test(start_paused = true)]
async fn panics_are_contained() {
    let mut h = harness(SchedulerOptions::default(), &[("bug", Behave::Panic)]);
    let t = h.store.create(shell("bug").with_schedule(Schedule::Interval { every_secs: 60 })).unwrap();
    h.sched.start().unwrap();
    let r = next_finished(&mut h.rx).await;
    assert_eq!(r.status, RunStatus::Failed);
    assert!(r.error.as_deref().unwrap().contains("internal error"));
    // the scheduler keeps working
    let r2 = next_finished(&mut h.rx).await;
    assert_eq!(r2.task_id, t.id);
    assert!(h.sched.running().is_empty());
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_and_stops() {
    let mut h = harness(SchedulerOptions::default(), &[("hang", Behave::Hang)]);
    let t = h.store.create(shell("hang")).unwrap();
    h.sched.start().unwrap();
    h.sched.run_now(&t.id, Trigger::Manual).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.sched.shutdown(Duration::from_secs(5)).await;
    let r = next_finished(&mut h.rx).await;
    assert_eq!(r.status, RunStatus::Cancelled);
    assert!(h.sched.running().is_empty());
    assert!(h.sched.run_now(&t.id, Trigger::Manual).is_err());
    assert!(h.sched.start().is_err());
    assert!(h.sched.run_now("missing", Trigger::Manual).is_err());
}

#[tokio::test(start_paused = true)]
async fn interrupted_runs_are_closed_at_start() {
    let h = harness(SchedulerOptions::default(), &[]);
    let t = h.store.create(shell("x")).unwrap();
    let mut stale = TaskRun::start(&t, Trigger::Schedule, Utc::now(), None);
    // No process has this id (pid + 7 could be another test binary running in parallel).
    stale.pid = u32::MAX;
    h.log.record(&stale).unwrap();
    h.sched.start().unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.log.get(&stale.run_id).unwrap().unwrap().status, RunStatus::Failed);
}

#[tokio::test(start_paused = true)]
async fn wait_for_a_run() {
    let h = harness(SchedulerOptions::default(), &[("w", Behave::Ok { secs: 9, rows: 3 })]);
    let t = h.store.create(shell("w")).unwrap();
    let id = h.sched.run_now(&t.id, Trigger::Manual).unwrap();
    let run = h.sched.wait(&id).await.unwrap();
    assert_eq!((run.status, run.rows), (RunStatus::Succeeded, Some(3)));
    // waiting for a finished run reads the log
    assert_eq!(h.sched.wait(&id).await.unwrap().run_id, id);
    assert!(h.sched.wait("nope").await.is_none());
}

#[tokio::test(start_paused = true)]
async fn run_task_once_for_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open(dir.path()).unwrap());
    let log = Arc::new(RunLog::open(dir.path(), RunLogOptions::default()).unwrap());
    let fake = Arc::new(Fake {
        clock: Arc::new(SystemClock),
        plans: [("one".to_string(), Behave::Ok { secs: 3, rows: 7 }), ("hang".to_string(), Behave::Hang)].into_iter().collect(),
        calls: Mutex::new(Vec::new()),
    });
    let one = store.create(shell("one")).unwrap();
    store.create(composite("Nightly", vec![one.id.clone()], true, false).with_folder("ops")).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let cb: EventCallback = Box::new(move |e: &TaskEvent| {
        if let TaskEvent::Message { message, .. } = e {
            sink.lock().push(message.text.clone());
        }
    });
    let run = run_task_once(store.clone(), log.clone(), fake.clone(), "ops/Nightly", Trigger::Cli, Some(cb), CancellationToken::new()).await.unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.exit_code(), 0);
    assert_eq!(run.rows, Some(7));
    assert_eq!(run.trigger, Trigger::Cli);
    assert!(seen.lock().iter().any(|m| m == "starting one"));
    assert_eq!(log.runs().unwrap().len(), 2);
    // by name, cancelled through the token
    store.create(shell("hang")).unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        c2.cancel();
    });
    let run = run_task_once(store.clone(), log.clone(), fake.clone(), "HANG", Trigger::Cli, None, cancel).await.unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.exit_code(), 2);
    assert!(run_task_once(store, log, fake, "missing", Trigger::Cli, None, CancellationToken::new()).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn system_scheduled_tasks_are_left_to_the_system() {
    let h = harness(SchedulerOptions::default(), &[("os", Behave::Ok { secs: 1, rows: 1 })]);
    let mut def = shell("os").with_schedule(Schedule::Interval { every_secs: 60 });
    def.os_scheduler = true;
    let t = h.store.create(def).unwrap();
    h.sched.start().unwrap();
    tokio::time::sleep(Duration::from_secs(200)).await;
    assert!(h.fake.calls_of("os").is_empty(), "launchd / cron / Task Scheduler runs it");
    assert!(h.sched.next_runs().contains_key(&t.id), "the next run is still shown");
    h.sched.shutdown(Duration::from_secs(1)).await;
}
