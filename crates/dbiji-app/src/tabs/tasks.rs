//! Task manager (FR-A3): saved tasks in folders with their schedules, next
//! and last runs; the task editor; manual runs and cancellation; export and
//! import of task definitions; the run history with live output; and the
//! option to register a task with the operating system scheduler so that it
//! also runs while DBiji is closed.
//!
//! Store writes, run log reads and operating system commands run in the
//! background; scheduler events are forwarded to the UI as they happen.

mod draft;
mod editor;
mod system;

pub use draft::{
    CompareDraft, IntervalUnit, KindDraft, NewTaskKind, ScheduleDraft, ScheduleMode, SqlScriptDraft, TaskDraft, kind_icon, kind_label, parse_local_time,
    weekday_names,
};
pub use system::{Synced, entry_preview, scheduler_name};

use crate::backend::Job;
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::tabs::dbtools::spawn_blocking;
use chrono::{DateTime, Local};
use dbiji_core::{DbResult, tr, trf};
use dbiji_tasks::{MessageLevel, Progress, RunLog, RunStatus, RunningInfo, TaskDef, TaskEvent, TaskRun, Trigger};
use editor::{EditorAction, TaskEditor};
use egui::{Color32, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::TryRecvError;
use tokio_util::sync::CancellationToken;

/// Runs shown in the history of a task.
const HISTORY_LIMIT: usize = 200;
/// Reload the run log this often while tasks are registered with the
/// operating system scheduler (their runs happen in other processes).
const EXTERNAL_REFRESH: Duration = Duration::from_secs(60);

/// Localised label of a run status.
pub fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => tr("Running"),
        RunStatus::Succeeded => tr("Succeeded"),
        RunStatus::Failed => tr("Failed"),
        RunStatus::Cancelled => tr("Cancelled"),
        RunStatus::Skipped => tr("Skipped"),
    }
}

/// Localised label of what started a run.
pub fn trigger_label(t: Trigger) -> &'static str {
    match t {
        Trigger::Manual => tr("Manual"),
        Trigger::Schedule => tr("Scheduled"),
        Trigger::Cli => tr("Command line"),
        Trigger::Composite => tr("Composite step"),
    }
}

fn status_style(s: RunStatus, dark: bool) -> (&'static str, Color32) {
    match s {
        RunStatus::Running => (icon::HOURGLASS, if dark { Color32::from_rgb(120, 170, 255) } else { Color32::from_rgb(30, 90, 200) }),
        RunStatus::Succeeded => (icon::CHECK_CIRCLE, crate::theme::success_color(dark)),
        RunStatus::Failed => (icon::X_CIRCLE, crate::theme::error_color(dark)),
        RunStatus::Cancelled => (icon::PROHIBIT, crate::theme::warning_color(dark)),
        RunStatus::Skipped => (icon::MINUS_CIRCLE, Color32::GRAY),
    }
}

/// Time for lists: the time today, otherwise date and time.
fn when(t: DateTime<Local>) -> String {
    if t.date_naive() == Local::now().date_naive() { t.format("%H:%M:%S").to_string() } else { t.format("%Y-%m-%d %H:%M").to_string() }
}

fn run_duration(run: &TaskRun) -> String {
    let end = run.finished.unwrap_or_else(chrono::Utc::now);
    crate::widgets::format_duration((end - run.started).to_std().unwrap_or_default())
}

fn in_folder(t: &TaskDef, folder: &str) -> bool {
    t.folder.as_deref().is_some_and(|f| f == folder || f.starts_with(&format!("{folder}/")))
}

/// What the background run log reader found.
struct RunSnapshot {
    focus: Option<String>,
    latest: HashMap<String, TaskRun>,
    history: Vec<TaskRun>,
    children: HashMap<String, Vec<TaskRun>>,
}

fn load_runs(log: &RunLog, focus: Option<&str>) -> DbResult<RunSnapshot> {
    let runs = log.runs()?;
    let mut latest: HashMap<String, TaskRun> = HashMap::new();
    for r in &runs {
        if !latest.contains_key(&r.task_id) {
            let mut l = r.clone();
            l.messages.clear();
            latest.insert(r.task_id.clone(), l);
        }
    }
    let history: Vec<TaskRun> = match focus {
        Some(f) => runs.iter().filter(|r| r.task_id == f).take(HISTORY_LIMIT).cloned().collect(),
        None => Vec::new(),
    };
    let ids: HashSet<&str> = history.iter().map(|r| r.run_id.as_str()).collect();
    let mut children: HashMap<String, Vec<TaskRun>> = HashMap::new();
    for r in runs.iter().filter(|r| r.parent_run_id.as_deref().is_some_and(|p| ids.contains(p))) {
        children.entry(r.parent_run_id.clone().unwrap_or_default()).or_default().push(r.clone());
    }
    for v in children.values_mut() {
        v.sort_by(|a, b| a.started.cmp(&b.started));
    }
    Ok(RunSnapshot { focus: focus.map(str::to_string), latest, history, children })
}

/// Result of a background store operation.
enum StoreDone {
    Saved { task: TaskDef, previous: Option<TaskDef> },
    Deleted(Vec<TaskDef>),
    Duplicated(Vec<TaskDef>),
    Toggled(Vec<TaskDef>),
    Imported(Vec<TaskDef>),
    Exported { path: PathBuf, count: usize },
    Moved(usize),
    Folder,
    Reloaded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// Saving from the editor: errors are shown in the editor.
    Editor,
    Other,
}

/// Result of a system scheduler change.
enum OsDone {
    Synced(TaskDef, Synced),
    /// Entry of a deleted task removed (`true`) or absent.
    Removed(String, bool),
    Failed { task: Option<TaskDef>, error: String },
}

/// Folder name prompt (new folder or rename).
struct FolderPrompt {
    /// Folder being renamed.
    rename: Option<String>,
    /// Parent of the new folder.
    parent: Option<String>,
    name: String,
}

enum RowAction {
    Click(usize, String),
    Edit(String),
    Enable(Vec<String>, bool),
    Run(Vec<String>),
    Cancel(Vec<String>),
    Duplicate(Vec<String>),
    Delete(Vec<String>),
    Move(Vec<String>, Option<String>),
    Export(Vec<String>),
}

/// The task manager tab.
pub struct TasksTab {
    folder: Option<String>,
    collapsed: HashSet<String>,
    filter: String,
    tasks: Vec<TaskDef>,
    folders: Vec<String>,
    store_rx: Option<tokio::sync::watch::Receiver<u64>>,
    initialized: bool,
    selected: Vec<String>,
    focus: Option<String>,
    anchor: Option<usize>,
    events: Option<tokio::sync::broadcast::Receiver<TaskEvent>>,
    forwarder: Option<CancellationToken>,
    latest: HashMap<String, TaskRun>,
    history_for: Option<String>,
    history: Vec<TaskRun>,
    children: HashMap<String, Vec<TaskRun>>,
    progress: HashMap<String, Progress>,
    runs_job: Option<Job<DbResult<RunSnapshot>>>,
    runs_error: Option<String>,
    buffered: Vec<TaskEvent>,
    reload_runs: bool,
    last_runs_load: Option<Instant>,
    selected_run: Option<String>,
    jobs: Vec<(JobKind, Job<DbResult<StoreDone>>)>,
    run_jobs: Vec<(String, Job<DbResult<String>>)>,
    os_jobs: Vec<Job<OsDone>>,
    /// Runs started from this tab (a notification is shown when they end).
    my_runs: HashSet<String>,
    /// Recently finished runs (for runs that end before their start is confirmed).
    recent_finished: Vec<TaskRun>,
    editor: Option<TaskEditor>,
    confirm_delete: Option<Vec<String>>,
    folder_prompt: Option<FolderPrompt>,
}

impl Default for TasksTab {
    fn default() -> Self {
        Self::new()
    }
}

impl TasksTab {
    pub fn new() -> Self {
        TasksTab {
            folder: None,
            collapsed: HashSet::new(),
            filter: String::new(),
            tasks: Vec::new(),
            folders: Vec::new(),
            store_rx: None,
            initialized: false,
            selected: Vec::new(),
            focus: None,
            anchor: None,
            events: None,
            forwarder: None,
            latest: HashMap::new(),
            history_for: None,
            history: Vec::new(),
            children: HashMap::new(),
            progress: HashMap::new(),
            runs_job: None,
            runs_error: None,
            buffered: Vec::new(),
            reload_runs: true,
            last_runs_load: None,
            selected_run: None,
            jobs: Vec::new(),
            run_jobs: Vec::new(),
            os_jobs: Vec::new(),
            my_runs: HashSet::new(),
            recent_finished: Vec::new(),
            editor: None,
            confirm_delete: None,
            folder_prompt: None,
        }
    }

    // ------------------------------------------------------------ test API

    /// Ids of the tasks shown in the list.
    pub fn task_ids(&self) -> Vec<String> {
        self.visible().iter().map(|t| t.id.clone()).collect()
    }

    /// Select one task (it becomes the focused task of the history panel).
    pub fn select_task(&mut self, id: &str) {
        self.selected = vec![id.to_string()];
        self.focus = Some(id.to_string());
    }

    /// The task whose history is shown.
    pub fn focused_task(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// Open the editor for a new task.
    pub fn open_new_task(&mut self, cx: &AppContext, kind: NewTaskKind) {
        let conn = if kind.uses_connection() || kind == NewTaskKind::SchemaCompare { cx.conns.configs().first().map(|c| c.id.clone()) } else { None };
        self.editor = Some(TaskEditor::new(TaskDraft::new(kind, conn, self.folder.clone())));
    }

    /// Open the editor for an existing task.
    pub fn edit_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter().find(|t| t.id == id) {
            self.editor = Some(TaskEditor::new(TaskDraft::from_task(t)));
        }
    }

    /// The task editor is open.
    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// Form of the open editor.
    pub fn editor_draft_mut(&mut self) -> Option<&mut TaskDraft> {
        self.editor.as_mut().map(|e| &mut e.draft)
    }

    /// Error shown in the open editor (failed save).
    pub fn editor_error(&self) -> Option<&str> {
        self.editor.as_ref().and_then(|e| e.error.as_deref())
    }

    /// Save the open editor (like its Save button).
    pub fn save_editor(&mut self, cx: &mut AppContext) -> Result<(), String> {
        let task = self.editor.as_ref().ok_or_else(|| tr("The task editor is not open").to_string())?.draft.to_task()?;
        self.save_task(cx, task);
        Ok(())
    }

    /// Start the selected tasks now.
    pub fn run_selected(&mut self, cx: &mut AppContext) {
        let ids = self.selected.clone();
        self.run_tasks(cx, ids);
    }

    /// Cancel the runs of the selected tasks.
    pub fn cancel_selected(&mut self, cx: &mut AppContext) {
        let ids = self.selected.clone();
        self.cancel_runs(cx, &ids);
    }

    /// Status of the most recent run of a task.
    pub fn latest_status(&self, task_id: &str) -> Option<RunStatus> {
        self.latest.get(task_id).map(|r| r.status)
    }

    /// Runs of the focused task, newest first.
    pub fn history(&self) -> &[TaskRun] {
        &self.history
    }

    /// Background work is pending (store writes, run log reads, starts).
    pub fn is_busy(&self) -> bool {
        !self.jobs.is_empty() || !self.run_jobs.is_empty() || !self.os_jobs.is_empty() || self.runs_job.is_some()
    }

    // ------------------------------------------------------------ model

    fn visible(&self) -> Vec<&TaskDef> {
        let f = self.filter.trim().to_lowercase();
        self.tasks
            .iter()
            .filter(|t| self.folder.as_deref().is_none_or(|sel| in_folder(t, sel)))
            .filter(|t| {
                f.is_empty()
                    || t.name.to_lowercase().contains(&f)
                    || t.description.as_deref().is_some_and(|d| d.to_lowercase().contains(&f))
                    || kind_label(&t.kind).to_lowercase().contains(&f)
            })
            .collect()
    }

    fn task(&self, id: &str) -> Option<&TaskDef> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn refresh_tasks(&mut self, cx: &AppContext) {
        let Some(store) = cx.services.tasks.as_ref() else { return };
        self.tasks = store.list();
        self.folders = store.folders();
        let ids: HashSet<&str> = self.tasks.iter().map(|t| t.id.as_str()).collect();
        self.selected.retain(|s| ids.contains(s.as_str()));
        if self.focus.as_deref().is_some_and(|f| !ids.contains(f)) {
            self.focus = None;
        }
        if let Some(f) = &self.folder {
            if !self.folders.contains(f) {
                self.folder = None;
            }
        }
    }

    fn ensure_subscribed(&mut self, cx: &AppContext) {
        if self.events.is_some() {
            return;
        }
        let Some(s) = cx.services.scheduler.clone() else { return };
        self.events = Some(s.subscribe());
        // wake the UI for every event; the tab drains them each frame
        let mut rx = s.subscribe();
        let token = CancellationToken::new();
        let stop = token.clone();
        let repaint = cx.backend.repainter();
        cx.backend.spawn_detached(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    r = rx.recv() => match r {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => repaint.request(),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });
        self.forwarder = Some(token);
    }

    fn start_runs_job(&mut self, cx: &AppContext) {
        if self.runs_job.is_some() {
            return;
        }
        let Some(log) = cx.services.runlog.clone() else {
            self.reload_runs = false;
            return;
        };
        self.reload_runs = false;
        self.last_runs_load = Some(Instant::now());
        let focus = self.history_for.clone();
        self.runs_job = Some(spawn_blocking(cx.backend, move || load_runs(&log, focus.as_deref())));
    }

    fn find_run_mut(&mut self, run_id: &str) -> Option<&mut TaskRun> {
        if let Some(i) = self.history.iter().position(|r| r.run_id == run_id) {
            return self.history.get_mut(i);
        }
        self.children.values_mut().flat_map(|v| v.iter_mut()).find(|r| r.run_id == run_id)
    }

    fn upsert_run(&mut self, run: TaskRun) {
        if let Some(parent) = run.parent_run_id.clone() {
            if self.history.iter().any(|r| r.run_id == parent) {
                let list = self.children.entry(parent).or_default();
                match list.iter_mut().find(|r| r.run_id == run.run_id) {
                    Some(slot) => *slot = run.clone(),
                    None => list.push(run.clone()),
                }
            }
        }
        if self.history_for.as_deref() == Some(run.task_id.as_str()) {
            match self.history.iter_mut().find(|r| r.run_id == run.run_id) {
                Some(slot) => *slot = run,
                None => {
                    self.history.insert(0, run);
                    self.history.truncate(HISTORY_LIMIT);
                }
            }
        }
    }

    fn notify(cx: &mut AppContext, run: &TaskRun) {
        match run.status {
            RunStatus::Succeeded => cx.toasts.success(trf("Task \"{}\" succeeded", &[&run.task_name])),
            RunStatus::Failed => cx.toasts.error(trf("Task \"{}\" failed", &[&run.task_name]), run.error.clone()),
            RunStatus::Cancelled => cx.toasts.info(trf("Task \"{}\" was cancelled", &[&run.task_name])),
            RunStatus::Skipped => cx.toasts.warning(trf("Task \"{}\" was skipped", &[&run.task_name]), run.messages.last().map(|m| m.text.clone())),
            RunStatus::Running => {}
        }
    }

    fn apply_event(&mut self, ev: TaskEvent, cx: &mut AppContext) {
        match ev {
            TaskEvent::Started { run } => {
                let mut l = run.clone();
                l.messages.clear();
                self.latest.insert(run.task_id.clone(), l);
                self.upsert_run(run);
            }
            TaskEvent::Message { run_id, message, .. } => {
                if let Some(r) = self.find_run_mut(&run_id) {
                    r.push_message(message);
                }
            }
            TaskEvent::Progress { run_id, progress, .. } => {
                self.progress.insert(run_id, progress);
            }
            TaskEvent::Finished { run } => {
                self.progress.remove(&run.run_id);
                if self.my_runs.remove(&run.run_id) {
                    Self::notify(cx, &run);
                } else if !self.recent_finished.iter().any(|r| r.run_id == run.run_id) {
                    self.recent_finished.push(run.clone());
                    if self.recent_finished.len() > 50 {
                        self.recent_finished.remove(0);
                    }
                }
                let mut l = run.clone();
                l.messages.clear();
                self.latest.insert(run.task_id.clone(), l);
                self.upsert_run(run);
            }
        }
    }

    fn drain_events(&mut self, cx: &mut AppContext) {
        let mut got = Vec::new();
        if let Some(rx) = self.events.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(ev) => got.push(ev),
                    Err(TryRecvError::Lagged(_)) => {
                        self.reload_runs = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => {
                        self.events = None;
                        break;
                    }
                }
            }
        }
        for ev in got {
            if self.runs_job.is_some() {
                // replayed on top of the run log snapshot being read
                self.buffered.push(ev.clone());
            }
            self.apply_event(ev, cx);
        }
    }

    fn poll(&mut self, cx: &mut AppContext) {
        if !self.initialized {
            self.initialized = true;
            self.store_rx = cx.services.tasks.as_ref().map(|s| s.subscribe());
            self.refresh_tasks(cx);
        }
        if self.store_rx.as_mut().is_some_and(|rx| rx.has_changed().unwrap_or(false)) {
            if let Some(rx) = self.store_rx.as_mut() {
                rx.borrow_and_update();
            }
            self.refresh_tasks(cx);
        }
        self.ensure_subscribed(cx);
        self.drain_events(cx);
        if self.focus != self.history_for {
            self.history_for = self.focus.clone();
            self.history.clear();
            self.children.clear();
            self.selected_run = None;
            self.reload_runs = true;
        }
        if let Some(r) = self.runs_job.as_mut().and_then(|j| j.poll()) {
            self.runs_job = None;
            match r {
                Ok(snap) => {
                    self.runs_error = None;
                    self.latest = snap.latest;
                    if snap.focus == self.history_for {
                        self.history = snap.history;
                        self.children = snap.children;
                    } else {
                        self.reload_runs = true;
                    }
                }
                Err(e) => self.runs_error = Some(e.full_text()),
            }
            for ev in std::mem::take(&mut self.buffered) {
                self.apply_event(ev, cx);
            }
        }
        let external = self.tasks.iter().any(|t| t.os_scheduler);
        if external && self.last_runs_load.is_some_and(|t| t.elapsed() >= EXTERNAL_REFRESH) {
            self.reload_runs = true;
        }
        if self.reload_runs {
            self.start_runs_job(cx);
        }
        self.poll_jobs(cx);
    }

    fn poll_jobs(&mut self, cx: &mut AppContext) {
        let mut finished = Vec::new();
        self.jobs.retain_mut(|(kind, job)| match job.poll() {
            Some(r) => {
                finished.push((*kind, r));
                false
            }
            None => true,
        });
        for (kind, r) in finished {
            match r {
                Ok(done) => self.store_done(cx, done),
                Err(e) if kind == JobKind::Editor => {
                    if let Some(ed) = self.editor.as_mut() {
                        ed.saving = false;
                        ed.error = Some(e.message);
                    }
                }
                Err(e) => cx.toasts.error(tr("The task change failed"), Some(e.full_text())),
            }
        }
        let mut started = Vec::new();
        self.run_jobs.retain_mut(|(name, job)| match job.poll() {
            Some(r) => {
                started.push((name.clone(), r));
                false
            }
            None => true,
        });
        for (name, r) in started {
            match r {
                Ok(run_id) => {
                    if let Some(i) = self.recent_finished.iter().position(|r| r.run_id == run_id) {
                        let run = self.recent_finished.remove(i);
                        Self::notify(cx, &run);
                    } else {
                        self.my_runs.insert(run_id);
                    }
                }
                Err(e) => cx.toasts.error(trf("Task \"{}\" could not start", &[&name]), Some(e.message)),
            }
        }
        let mut os = Vec::new();
        self.os_jobs.retain_mut(|job| match job.poll() {
            Some(r) => {
                os.push(r);
                false
            }
            None => true,
        });
        for r in os {
            match r {
                OsDone::Synced(task, Synced::Installed(_)) => cx.toasts.success(trf("\"{}\" is registered with {}", &[&task.name, &scheduler_name()])),
                OsDone::Synced(task, Synced::Removed(true)) => cx.toasts.info(trf("\"{}\" was removed from {}", &[&task.name, &scheduler_name()])),
                OsDone::Removed(name, true) => cx.toasts.info(trf("\"{}\" was removed from {}", &[&name, &scheduler_name()])),
                OsDone::Synced(_, Synced::Removed(false)) | OsDone::Removed(_, false) => {}
                OsDone::Failed { task, error } => {
                    cx.toasts.error(trf("The {} entry could not be changed", &[&scheduler_name()]), Some(error));
                    // keep the flag in line with reality
                    if let (Some(t), Some(store)) = (task.filter(|t| t.os_scheduler && system::wanted(t)), cx.services.tasks.clone()) {
                        let id = t.id.clone();
                        self.jobs.push((
                            JobKind::Other,
                            spawn_blocking(cx.backend, move || {
                                if let Some(mut t) = store.get(&id) {
                                    t.os_scheduler = false;
                                    store.update(t)?;
                                }
                                Ok(StoreDone::Reloaded)
                            }),
                        ));
                    }
                }
            }
        }
    }

    fn store_done(&mut self, cx: &mut AppContext, done: StoreDone) {
        match done {
            StoreDone::Saved { task, previous } => {
                self.editor = None;
                self.selected = vec![task.id.clone()];
                self.focus = Some(task.id.clone());
                if task.os_scheduler || previous.as_ref().is_some_and(|p| p.os_scheduler) {
                    self.sync_os(cx, task.clone());
                }
                cx.toasts.success(trf("Task \"{}\" saved", &[&task.name]));
            }
            StoreDone::Deleted(tasks) => {
                for t in tasks.iter().filter(|t| t.os_scheduler) {
                    let (id, name) = (t.id.clone(), t.name.clone());
                    self.os_jobs.push(cx.backend.spawn(async move {
                        match system::remove(id).await {
                            Ok(r) => OsDone::Removed(name, r),
                            Err(e) => OsDone::Failed { task: None, error: e.full_text() },
                        }
                    }));
                }
                self.selected.clear();
                cx.toasts.info(trf("{} task(s) deleted", &[&tasks.len()]));
            }
            StoreDone::Duplicated(tasks) | StoreDone::Imported(tasks) => {
                self.selected = tasks.iter().map(|t| t.id.clone()).collect();
                self.focus = tasks.first().map(|t| t.id.clone());
                cx.toasts.success(trf("{} task(s) added", &[&tasks.len()]));
            }
            StoreDone::Toggled(tasks) => {
                for t in tasks.into_iter().filter(|t| t.os_scheduler) {
                    self.sync_os(cx, t);
                }
            }
            StoreDone::Exported { path, count } => cx.toasts.success(trf("{} task(s) exported to {}", &[&count, &path.display()])),
            StoreDone::Moved(n) => cx.toasts.info(trf("{} task(s) moved", &[&n])),
            StoreDone::Folder | StoreDone::Reloaded => {}
        }
        self.refresh_tasks(cx);
    }

    // ------------------------------------------------------------ actions

    fn save_task(&mut self, cx: &mut AppContext, task: TaskDef) {
        let Some(store) = cx.services.tasks.clone() else { return };
        let previous = self.editor.as_ref().and_then(|e| e.draft.original.clone());
        if let Some(e) = self.editor.as_mut() {
            e.saving = true;
            e.error = None;
        }
        self.jobs.push((
            JobKind::Editor,
            spawn_blocking(cx.backend, move || {
                let task = if previous.is_some() { store.update(task)? } else { store.create(task)? };
                Ok(StoreDone::Saved { task, previous })
            }),
        ));
    }

    /// Bring the operating system entry of `task` in line with its settings.
    fn sync_os(&mut self, cx: &AppContext, task: TaskDef) {
        self.os_jobs.push(cx.backend.spawn(async move {
            match system::sync(task.clone()).await {
                Ok(s) => OsDone::Synced(task, s),
                Err(e) => OsDone::Failed { task: Some(task), error: e.full_text() },
            }
        }));
    }

    fn run_tasks(&mut self, cx: &mut AppContext, ids: Vec<String>) {
        let Some(s) = cx.services.scheduler.clone() else {
            cx.toasts.error(tr("Tasks cannot be run: the task scheduler is not available."), None);
            return;
        };
        for id in ids {
            let name = self.task(&id).map(|t| t.name.clone()).unwrap_or_default();
            let s = s.clone();
            self.run_jobs.push((name, cx.backend.spawn(async move { s.run_now(&id, Trigger::Manual) })));
        }
    }

    fn cancel_runs(&mut self, cx: &mut AppContext, ids: &[String]) {
        let Some(s) = cx.services.scheduler.clone() else { return };
        for r in s.running().into_iter().filter(|r| ids.contains(&r.task_id)) {
            s.cancel(&r.run_id);
        }
    }

    fn set_enabled(&mut self, cx: &mut AppContext, ids: Vec<String>, on: bool) {
        let Some(store) = cx.services.tasks.clone() else { return };
        self.jobs.push((
            JobKind::Other,
            spawn_blocking(cx.backend, move || {
                let mut out = Vec::new();
                for id in &ids {
                    store.set_enabled(id, on)?;
                    out.extend(store.get(id));
                }
                Ok(StoreDone::Toggled(out))
            }),
        ));
    }

    fn duplicate(&mut self, cx: &mut AppContext, ids: Vec<String>) {
        let Some(store) = cx.services.tasks.clone() else { return };
        self.jobs.push((JobKind::Other, spawn_blocking(cx.backend, move || ids.iter().map(|id| store.duplicate(id)).collect::<DbResult<Vec<_>>>().map(StoreDone::Duplicated))));
    }

    fn delete(&mut self, cx: &mut AppContext, ids: Vec<String>) {
        let Some(store) = cx.services.tasks.clone() else { return };
        self.cancel_runs(cx, &ids);
        self.jobs.push((
            JobKind::Other,
            spawn_blocking(cx.backend, move || {
                let mut deleted = Vec::new();
                for id in &ids {
                    if let Some(t) = store.get(id) {
                        if store.delete(id)? {
                            deleted.push(t);
                        }
                    }
                }
                Ok(StoreDone::Deleted(deleted))
            }),
        ));
    }

    fn move_to(&mut self, cx: &mut AppContext, ids: Vec<String>, folder: Option<String>) {
        let Some(store) = cx.services.tasks.clone() else { return };
        self.jobs.push((
            JobKind::Other,
            spawn_blocking(cx.backend, move || {
                for id in &ids {
                    store.move_task(id, folder.as_deref())?;
                }
                Ok(StoreDone::Moved(ids.len()))
            }),
        ));
    }

    fn export(&mut self, cx: &mut AppContext, ids: Vec<String>) {
        let Some(store) = cx.services.tasks.clone() else { return };
        let Some(path) = rfd::FileDialog::new().set_file_name("dbiji-tasks.json").add_filter("JSON", &["json"]).save_file() else { return };
        self.jobs.push((
            JobKind::Other,
            spawn_blocking(cx.backend, move || {
                let json = store.export_json(&ids)?;
                let count = serde_json::from_str::<serde_json::Value>(&json).ok().and_then(|v| v["tasks"].as_array().map(|a| a.len())).unwrap_or(ids.len());
                std::fs::write(&path, json).map_err(|e| dbiji_core::DbError::io(format!("{}: {e}", path.display())))?;
                Ok(StoreDone::Exported { path, count })
            }),
        ));
    }

    fn import(&mut self, cx: &mut AppContext) {
        let Some(store) = cx.services.tasks.clone() else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() else { return };
        self.jobs.push((
            JobKind::Other,
            spawn_blocking(cx.backend, move || {
                let text = std::fs::read_to_string(&path).map_err(|e| dbiji_core::DbError::io(format!("{}: {e}", path.display())))?;
                store.import_json(&text).map(StoreDone::Imported)
            }),
        ));
    }

    fn folder_op(&mut self, cx: &mut AppContext, op: impl FnOnce(&dbiji_tasks::TaskStore) -> DbResult<()> + Send + 'static) {
        let Some(store) = cx.services.tasks.clone() else { return };
        self.jobs.push((JobKind::Other, spawn_blocking(cx.backend, move || op(&store).map(|_| StoreDone::Folder))));
    }

    fn reload_all(&mut self, cx: &mut AppContext) {
        self.reload_runs = true;
        if let Some(store) = cx.services.tasks.clone() {
            self.jobs.push((JobKind::Other, spawn_blocking(cx.backend, move || store.reload().map(|_| StoreDone::Reloaded))));
        }
    }

    // ------------------------------------------------------------ UI

    fn banner(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if cx.settings.tasks.scheduler_enabled {
            return;
        }
        let color = crate::theme::warning_color(ui.visuals().dark_mode);
        egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, color)).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icon::WARNING).color(color));
                ui.label(tr("Scheduled runs are turned off: tasks run only when you start them (or through the system scheduler). Turn on \"Run scheduled tasks while DBiji is open\" in the settings."));
                if ui.button(format!("{} {}", icon::GEAR, tr("Open settings"))).clicked() {
                    cx.push(AppCommand::OpenSettings);
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let running: HashSet<String> = cx.services.scheduler.as_ref().map(|s| s.running().into_iter().map(|r| r.task_id).collect()).unwrap_or_default();
        let sel = self.selected.clone();
        let one = sel.len() == 1;
        let any = !sel.is_empty();
        let can_run = cx.services.scheduler.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.menu_button(format!("{} {}", icon::PLUS, tr("New task")), |ui| {
                for k in NewTaskKind::ALL {
                    if ui.button(format!("{} {}", k.icon(), k.label())).clicked() {
                        self.open_new_task(cx, k);
                        ui.close();
                    }
                }
                ui.separator();
                for (glyph, label) in [(icon::EXPORT, tr("Data export")), (icon::DOWNLOAD_SIMPLE, tr("Data import")), (icon::ARROWS_LEFT_RIGHT, tr("Data migration"))] {
                    ui.add_enabled(false, egui::Button::new(format!("{glyph} {label}"))).on_disabled_hover_text(tr("Created with \"Save as task\" in the transfer wizards"));
                }
            });
            if ui.add_enabled(one, egui::Button::new(format!("{} {}", icon::PENCIL_SIMPLE, tr("Edit")))).clicked() {
                self.edit_task(&sel[0]);
            }
            if ui.add_enabled(any, egui::Button::new(format!("{} {}", icon::COPY, tr("Duplicate")))).clicked() {
                self.duplicate(cx, sel.clone());
            }
            if ui.add_enabled(any, egui::Button::new(format!("{} {}", icon::TRASH, tr("Delete")))).clicked() {
                self.confirm_delete = Some(sel.clone());
            }
            let all_enabled = any && sel.iter().all(|id| self.task(id).is_some_and(|t| t.enabled));
            let (glyph, label) = if all_enabled { (icon::TOGGLE_LEFT, tr("Disable")) } else { (icon::TOGGLE_RIGHT, tr("Enable")) };
            if ui.add_enabled(any, egui::Button::new(format!("{glyph} {label}"))).clicked() {
                self.set_enabled(cx, sel.clone(), !all_enabled);
            }
            ui.separator();
            let r = ui.add_enabled(any && can_run, egui::Button::new(format!("{} {}", icon::PLAY, tr("Run now"))));
            if !can_run {
                r.clone().on_disabled_hover_text(tr("Tasks cannot be run: the task scheduler is not available."));
            }
            if r.clicked() {
                self.run_tasks(cx, sel.clone());
            }
            let any_running = sel.iter().any(|id| running.contains(id));
            if ui.add_enabled(any_running, egui::Button::new(format!("{} {}", icon::STOP, tr("Cancel run")))).clicked() {
                self.cancel_runs(cx, &sel);
            }
            ui.separator();
            let export_label = if any { tr("Export selected…") } else { tr("Export all…") };
            if ui.add_enabled(!self.tasks.is_empty(), egui::Button::new(format!("{} {export_label}", icon::UPLOAD_SIMPLE))).clicked() {
                self.export(cx, sel.clone());
            }
            if ui.button(format!("{} {}", icon::DOWNLOAD_SIMPLE, tr("Import…"))).clicked() {
                self.import(cx);
            }
            ui.separator();
            if ui.button(icon::ARROW_CLOCKWISE).on_hover_text(tr("Refresh")).clicked() {
                self.reload_all(cx);
            }
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("Filter…")).desired_width(160.0));
        });
    }

    fn folder_visible(&self, f: &str) -> bool {
        let mut acc = String::new();
        let parts: Vec<&str> = f.split('/').collect();
        for p in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            if self.collapsed.contains(&acc) {
                return false;
            }
        }
        true
    }

    fn folders_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        ui.horizontal(|ui| {
            ui.strong(tr("Folders"));
            if ui.small_button(icon::FOLDER_PLUS).on_hover_text(tr("New folder")).clicked() {
                self.folder_prompt = Some(FolderPrompt { rename: None, parent: self.folder.clone(), name: String::new() });
            }
        });
        ui.separator();
        let folders = self.folders.clone();
        egui::ScrollArea::vertical().id_salt("tasks_folders").auto_shrink([false, false]).show(ui, |ui| {
            let total = self.tasks.len();
            if ui.selectable_label(self.folder.is_none(), format!("{} {}  ({total})", icon::LIST_CHECKS, tr("All tasks"))).clicked() {
                self.folder = None;
            }
            for (i, f) in folders.iter().enumerate() {
                if !self.folder_visible(f) {
                    continue;
                }
                let depth = f.matches('/').count();
                let name = f.rsplit('/').next().unwrap_or(f).to_string();
                let has_children = folders.get(i + 1).is_some_and(|n| n.starts_with(&format!("{f}/")));
                let count = self.tasks.iter().filter(|t| in_folder(t, f)).count();
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 14.0);
                    if has_children {
                        let collapsed = self.collapsed.contains(f);
                        if ui.small_button(if collapsed { icon::CARET_RIGHT } else { icon::CARET_DOWN }).clicked() {
                            if collapsed {
                                self.collapsed.remove(f);
                            } else {
                                self.collapsed.insert(f.clone());
                            }
                        }
                    } else {
                        ui.add_space(18.0);
                    }
                    let open = self.folder.as_deref() == Some(f.as_str());
                    let r = ui.selectable_label(open, format!("{} {name}  ({count})", if open { icon::FOLDER_OPEN } else { icon::FOLDER }));
                    if r.clicked() {
                        self.folder = Some(f.clone());
                    }
                    r.context_menu(|ui| {
                        if ui.button(format!("{} {}", icon::FOLDER_PLUS, tr("New sub-folder…"))).clicked() {
                            self.folder_prompt = Some(FolderPrompt { rename: None, parent: Some(f.clone()), name: String::new() });
                            ui.close();
                        }
                        if ui.button(format!("{} {}", icon::PENCIL_SIMPLE, tr("Rename…"))).clicked() {
                            self.folder_prompt = Some(FolderPrompt { rename: Some(f.clone()), parent: None, name: name.clone() });
                            ui.close();
                        }
                        if ui.button(format!("{} {}", icon::TRASH, tr("Delete folder"))).on_hover_text(tr("Its tasks move to the parent folder")).clicked() {
                            let path = f.clone();
                            self.folder_op(cx, move |s| s.delete_folder(&path));
                            ui.close();
                        }
                    });
                });
            }
        });
    }

    fn list_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let visible: Vec<TaskDef> = self.visible().into_iter().cloned().collect();
        if let Some(e) = &self.runs_error {
            crate::widgets::error_box(ui, &trf("Cannot read the run history: {}", &[e]));
        }
        if visible.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(if self.tasks.is_empty() { tr("No tasks yet.") } else { tr("No task matches.") });
                ui.weak(tr("Tasks save SQL scripts, backups, schema compares and shell commands so that they can run on demand or on a schedule."));
            });
            return;
        }
        let running: HashMap<String, RunningInfo> =
            cx.services.scheduler.as_ref().map(|s| s.running().into_iter().map(|r| (r.task_id.clone(), r)).collect()).unwrap_or_default();
        let next_runs: BTreeMap<String, DateTime<Local>> = cx.services.scheduler.as_ref().map(|s| s.next_runs()).unwrap_or_default();
        let scheduling = cx.settings.tasks.scheduler_enabled;
        let show_folder = self.folder.is_none();
        let dark = ui.visuals().dark_mode;
        let folders = self.folders.clone();
        let modifiers = ui.input(|i| i.modifiers);
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let mut actions: Vec<RowAction> = Vec::new();
        let now = Local::now();
        TableBuilder::new(ui)
            .id_salt("tasks_table")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::auto().at_least(170.0).at_most(420.0).clip(true).resizable(true))
            .column(Column::auto().at_least(90.0).clip(true).resizable(true))
            .column(Column::auto().at_least(130.0).at_most(320.0).clip(true).resizable(true))
            .column(Column::auto().at_least(120.0).clip(true).resizable(true))
            .column(Column::remainder().at_least(150.0).clip(true))
            .header(row_h + 2.0, |mut h| {
                h.col(|ui| {
                    ui.label(icon::POWER).on_hover_text(tr("Enabled"));
                });
                for title in [tr("Name"), tr("Type"), tr("Schedule"), tr("Next run"), tr("Last run")] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, visible.len(), |mut row| {
                    let i = row.index();
                    let t = &visible[i];
                    row.set_selected(self.selected.iter().any(|s| s == &t.id));
                    row.col(|ui| {
                        let mut on = t.enabled;
                        if ui.checkbox(&mut on, "").on_hover_text(tr("Enabled: the scheduler runs the task on its schedule")).changed() {
                            actions.push(RowAction::Enable(vec![t.id.clone()], on));
                        }
                    });
                    row.col(|ui| {
                        let text = if show_folder && t.folder.is_some() { format!("{} {}", kind_icon(&t.kind), t.path()) } else { format!("{} {}", kind_icon(&t.kind), t.name) };
                        let mut rt = RichText::new(text);
                        if !t.enabled {
                            rt = rt.weak();
                        }
                        let r = ui.add(egui::Label::new(rt).truncate().selectable(false));
                        if let Some(d) = &t.description {
                            r.on_hover_text(d);
                        }
                    });
                    row.col(|ui| {
                        ui.label(kind_label(&t.kind));
                    });
                    row.col(|ui| match &t.schedule {
                        Some(s) => {
                            ui.add(egui::Label::new(s.describe()).truncate().selectable(false));
                            if t.os_scheduler {
                                ui.label(RichText::new(icon::CLOCK).weak()).on_hover_text(trf("Also registered with {}", &[&scheduler_name()]));
                            }
                        }
                        None => {
                            ui.weak(tr("Manual"));
                        }
                    });
                    row.col(|ui| {
                        let next = if !t.enabled {
                            None
                        } else if let Some(n) = next_runs.get(&t.id) {
                            Some((*n, false))
                        } else {
                            t.schedule.as_ref().and_then(|s| {
                                let anchor = self.latest.get(&t.id).map(|r| r.started.with_timezone(&Local)).unwrap_or_else(|| t.updated.with_timezone(&Local));
                                s.next_after_skipping(&anchor.min(now), &now).map(|n| (n, true))
                            })
                        };
                        match next {
                            Some((n, false)) => {
                                ui.label(when(n));
                            }
                            Some((n, true)) => {
                                let hint = if scheduling { tr("Expected; the scheduler has not planned it yet") } else { tr("Scheduled runs are turned off") };
                                ui.label(RichText::new(when(n)).weak()).on_hover_text(hint);
                            }
                            None => {
                                ui.weak("—");
                            }
                        }
                    });
                    row.col(|ui| {
                        if let Some(r) = running.get(&t.id) {
                            let (glyph, color) = status_style(RunStatus::Running, dark);
                            let elapsed = (chrono::Utc::now() - r.started).to_std().unwrap_or_default();
                            let mut text = format!("{glyph} {} {}", tr("Running"), crate::widgets::format_duration(elapsed));
                            if let Some(p) = self.progress.get(&r.run_id) {
                                match p.total {
                                    Some(total) if total > 0 => text.push_str(&format!(" · {}/{}", p.done, total)),
                                    _ if p.done > 0 => text.push_str(&format!(" · {}", p.done)),
                                    _ => {}
                                }
                            }
                            ui.label(RichText::new(text).color(color));
                        } else if let Some(l) = self.latest.get(&t.id) {
                            let (glyph, color) = status_style(l.status, dark);
                            let r = ui.label(RichText::new(format!("{glyph} {} · {} · {}", status_label(l.status), when(l.started.with_timezone(&Local)), run_duration(l))).color(color));
                            if let Some(e) = &l.error {
                                r.on_hover_text(e);
                            }
                        } else {
                            ui.weak(tr("never"));
                        }
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        actions.push(RowAction::Click(i, t.id.clone()));
                    }
                    if resp.double_clicked() {
                        actions.push(RowAction::Edit(t.id.clone()));
                    }
                    let targets = if self.selected.iter().any(|s| s == &t.id) { self.selected.clone() } else { vec![t.id.clone()] };
                    resp.context_menu(|ui| {
                        if ui.button(format!("{} {}", icon::PENCIL_SIMPLE, tr("Edit…"))).clicked() {
                            actions.push(RowAction::Edit(t.id.clone()));
                            ui.close();
                        }
                        if ui.button(format!("{} {}", icon::PLAY, tr("Run now"))).clicked() {
                            actions.push(RowAction::Run(targets.clone()));
                            ui.close();
                        }
                        if running.contains_key(&t.id) && ui.button(format!("{} {}", icon::STOP, tr("Cancel run"))).clicked() {
                            actions.push(RowAction::Cancel(targets.clone()));
                            ui.close();
                        }
                        if ui.button(format!("{} {}", icon::COPY, tr("Duplicate"))).clicked() {
                            actions.push(RowAction::Duplicate(targets.clone()));
                            ui.close();
                        }
                        let label = if t.enabled { tr("Disable") } else { tr("Enable") };
                        if ui.button(format!("{} {label}", if t.enabled { icon::TOGGLE_LEFT } else { icon::TOGGLE_RIGHT })).clicked() {
                            actions.push(RowAction::Enable(targets.clone(), !t.enabled));
                            ui.close();
                        }
                        ui.menu_button(format!("{} {}", icon::FOLDER, tr("Move to")), |ui| {
                            if ui.button(tr("(top level)")).clicked() {
                                actions.push(RowAction::Move(targets.clone(), None));
                                ui.close();
                            }
                            for f in &folders {
                                if ui.button(format!("{} {f}", icon::FOLDER)).clicked() {
                                    actions.push(RowAction::Move(targets.clone(), Some(f.clone())));
                                    ui.close();
                                }
                            }
                        });
                        if ui.button(format!("{} {}", icon::UPLOAD_SIMPLE, tr("Export…"))).clicked() {
                            actions.push(RowAction::Export(targets.clone()));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(format!("{} {}", icon::TRASH, tr("Delete…"))).clicked() {
                            actions.push(RowAction::Delete(targets.clone()));
                            ui.close();
                        }
                    });
                });
            });
        for a in actions {
            match a {
                RowAction::Click(i, id) => {
                    if modifiers.shift {
                        let from = self.anchor.unwrap_or(i).min(visible.len().saturating_sub(1));
                        let (lo, hi) = if from <= i { (from, i) } else { (i, from) };
                        self.selected = visible[lo..=hi].iter().map(|t| t.id.clone()).collect();
                    } else if modifiers.command {
                        if let Some(p) = self.selected.iter().position(|s| *s == id) {
                            self.selected.remove(p);
                        } else {
                            self.selected.push(id.clone());
                        }
                        self.anchor = Some(i);
                    } else {
                        self.selected = vec![id.clone()];
                        self.anchor = Some(i);
                    }
                    self.focus = Some(id);
                }
                RowAction::Edit(id) => self.edit_task(&id),
                RowAction::Enable(ids, on) => self.set_enabled(cx, ids, on),
                RowAction::Run(ids) => self.run_tasks(cx, ids),
                RowAction::Cancel(ids) => self.cancel_runs(cx, &ids),
                RowAction::Duplicate(ids) => self.duplicate(cx, ids),
                RowAction::Delete(ids) => self.confirm_delete = Some(ids),
                RowAction::Move(ids, f) => self.move_to(cx, ids, f),
                RowAction::Export(ids) => self.export(cx, ids),
            }
        }
        if !running.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    fn history_ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let Some(focus) = self.focus.clone() else {
            ui.weak(tr("Select a task to see its runs."));
            return;
        };
        let name = self.task(&focus).map(|t| t.path()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.strong(trf("Run history — {}", &[&name]));
            if self.runs_job.is_some() {
                ui.spinner();
            }
            if ui.small_button(icon::ARROW_CLOCKWISE).on_hover_text(tr("Refresh")).clicked() {
                self.reload_runs = true;
            }
        });
        if self.history.is_empty() {
            ui.weak(if self.runs_job.is_some() { tr("Loading…") } else { tr("The task has not run yet.") });
            return;
        }
        let dark = ui.visuals().dark_mode;
        let shown = self.selected_run.clone().filter(|id| self.history.iter().any(|r| &r.run_id == id)).unwrap_or_else(|| self.history[0].run_id.clone());
        let mut clicked = None;
        let width = ui.available_width();
        egui::Panel::left(egui::Id::new("tasks_history_runs")).resizable(true).default_size(width * 0.5).show(ui, |ui| {
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
            TableBuilder::new(ui)
                .id_salt("tasks_history_table")
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(100.0).clip(true))
                .column(Column::auto().at_least(80.0).clip(true))
                .column(Column::auto().at_least(110.0).clip(true))
                .column(Column::auto().at_least(60.0).clip(true))
                .column(Column::remainder().at_least(100.0).clip(true))
                .header(row_h + 2.0, |mut h| {
                    for title in [tr("Status"), tr("Trigger"), tr("Started"), tr("Duration"), tr("Message")] {
                        h.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, self.history.len(), |mut row| {
                        let r = &self.history[row.index()];
                        row.set_selected(r.run_id == shown);
                        let (glyph, color) = status_style(r.status, dark);
                        row.col(|ui| {
                            ui.label(RichText::new(format!("{glyph} {}", status_label(r.status))).color(color));
                        });
                        row.col(|ui| {
                            ui.label(trigger_label(r.trigger));
                        });
                        row.col(|ui| {
                            ui.label(when(r.started.with_timezone(&Local)));
                        });
                        row.col(|ui| {
                            ui.label(run_duration(r));
                        });
                        row.col(|ui| {
                            let msg = r.error.clone().or_else(|| r.messages.last().map(|m| m.text.clone())).unwrap_or_default();
                            ui.add(egui::Label::new(msg.lines().next().unwrap_or_default()).truncate().selectable(false));
                        });
                        if row.response().clicked() {
                            clicked = Some(r.run_id.clone());
                        }
                    });
                });
        });
        if let Some(id) = clicked {
            self.selected_run = Some(id);
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(run) = self.history.iter().find(|r| r.run_id == shown).cloned() {
                self.run_details_ui(ui, cx, &run);
            }
        });
    }

    fn run_details_ui(&mut self, ui: &mut Ui, cx: &mut AppContext, run: &TaskRun) {
        let dark = ui.visuals().dark_mode;
        let (glyph, color) = status_style(run.status, dark);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{glyph} {}", status_label(run.status))).color(color).strong());
            ui.weak(format!("{} · {}", run.started.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"), run_duration(run)));
            if let Some(rows) = run.rows {
                ui.weak(trf("{} row(s)", &[&rows]));
            }
            if run.status == RunStatus::Running {
                if let Some(p) = self.progress.get(&run.run_id) {
                    match p.total {
                        Some(total) if total > 0 => {
                            ui.add(egui::ProgressBar::new((p.done as f32 / total as f32).clamp(0.0, 1.0)).desired_width(140.0).text(format!("{} / {}", p.done, total)));
                        }
                        _ => {
                            ui.spinner();
                        }
                    }
                }
                if ui.small_button(format!("{} {}", icon::STOP, tr("Cancel run"))).clicked() {
                    if let Some(s) = cx.services.scheduler.as_ref() {
                        s.cancel(&run.run_id);
                    }
                }
            }
        });
        if let Some(e) = &run.error {
            crate::widgets::error_box(ui, e);
        }
        if !run.output_files.is_empty() {
            ui.label(tr("Output files"));
            for f in &run.output_files {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(f.display().to_string()).monospace());
                    if ui.small_button(icon::ARROW_SQUARE_OUT).on_hover_text(tr("Open")).clicked() {
                        let p = f.clone();
                        cx.backend.spawn_detached(async move {
                            let _ = open::that_detached(p);
                        });
                    }
                    if let Some(dir) = f.parent().map(|d| d.to_path_buf()) {
                        if ui.small_button(icon::FOLDER_OPEN).on_hover_text(tr("Open folder")).clicked() {
                            cx.backend.spawn_detached(async move {
                                let _ = open::that_detached(dir);
                            });
                        }
                    }
                });
            }
        }
        if let Some(steps) = self.children.get(&run.run_id).filter(|s| !s.is_empty()) {
            ui.label(tr("Steps"));
            for s in steps {
                let (g, c) = status_style(s.status, dark);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(g).color(c));
                    ui.label(&s.task_name);
                    ui.weak(run_duration(s));
                    if let Some(e) = &s.error {
                        ui.add(egui::Label::new(RichText::new(e).color(crate::theme::error_color(dark))).truncate());
                    }
                });
            }
        }
        ui.label(tr("Messages"));
        if run.dropped_messages > 0 {
            ui.weak(trf("{} message(s) were dropped from the middle of the log", &[&run.dropped_messages]));
        }
        egui::ScrollArea::both().id_salt(("task_run_messages", &run.run_id)).auto_shrink([false, false]).stick_to_bottom(run.status == RunStatus::Running).show(ui, |ui| {
            if run.messages.is_empty() {
                ui.weak("—");
            }
            for m in &run.messages {
                let (g, c) = match m.level {
                    MessageLevel::Info => (icon::INFO, ui.visuals().text_color()),
                    MessageLevel::Warning => (icon::WARNING, crate::theme::warning_color(dark)),
                    MessageLevel::Error => (icon::X_CIRCLE, crate::theme::error_color(dark)),
                };
                ui.horizontal_top(|ui| {
                    ui.weak(m.time.with_timezone(&Local).format("%H:%M:%S").to_string());
                    ui.label(RichText::new(g).color(c));
                    ui.add(egui::Label::new(RichText::new(&m.text).monospace().color(c)).extend());
                });
            }
        });
    }

    fn dialogs(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let ctx = ui.ctx().clone();
        if let Some(mut editor) = self.editor.take() {
            let tasks = self.tasks.clone();
            let folders = self.folders.clone();
            let action = editor.show(&ctx, cx, &tasks, &folders);
            self.editor = Some(editor);
            match action {
                EditorAction::None => {}
                EditorAction::Cancel => self.editor = None,
                EditorAction::Save(task) => self.save_task(cx, task),
            }
        }
        if let Some(ids) = self.confirm_delete.clone() {
            let mut close = false;
            let names: Vec<String> = ids.iter().filter_map(|id| self.task(id).map(|t| t.path())).collect();
            let os = ids.iter().any(|id| self.task(id).is_some_and(|t| t.os_scheduler));
            let modal = egui::Modal::new(egui::Id::new("tasks_confirm_delete")).show(&ctx, |ui| {
                ui.heading(tr("Delete tasks"));
                ui.label(trf("Delete {} task(s)?", &[&names.len()]));
                for n in names.iter().take(10) {
                    ui.label(format!("• {n}"));
                }
                if names.len() > 10 {
                    ui.weak(trf("… {} more", &[&(names.len() - 10)]));
                }
                ui.weak(tr("Deleted tasks are also removed from composite tasks. The run history is kept."));
                if os {
                    ui.weak(trf("Their {} entries are removed too.", &[&scheduler_name()]));
                }
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Delete")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        self.delete(cx, ids.clone());
                        close = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.confirm_delete = None;
            }
        }
        if let Some(mut p) = self.folder_prompt.take() {
            let mut keep = true;
            let modal = egui::Modal::new(egui::Id::new("tasks_folder_prompt")).show(&ctx, |ui| {
                ui.heading(if p.rename.is_some() { tr("Rename folder") } else { tr("New folder") });
                if let Some(parent) = &p.parent {
                    ui.weak(trf("Inside {}", &[parent]));
                }
                let r = ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(260.0).hint_text(tr("Folder name")));
                r.request_focus();
                let name = p.name.trim().trim_matches('/').to_string();
                let ok = !name.is_empty();
                ui.horizontal(|ui| {
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.add_enabled(ok, egui::Button::new(tr("OK"))).clicked() || (enter && ok)) && ok {
                        match p.rename.clone() {
                            Some(old) => {
                                let new = match old.rsplit_once('/') {
                                    Some((parent, _)) => format!("{parent}/{name}"),
                                    None => name.clone(),
                                };
                                if self.folder.as_deref() == Some(old.as_str()) {
                                    self.folder = Some(new.clone());
                                }
                                self.folder_op(cx, move |s| s.rename_folder(&old, &new).map(|_| ()));
                            }
                            None => {
                                let path = match &p.parent {
                                    Some(parent) => format!("{parent}/{name}"),
                                    None => name.clone(),
                                };
                                self.folder_op(cx, move |s| s.create_folder(&path));
                            }
                        }
                        keep = false;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        keep = false;
                    }
                });
            });
            if keep && !modal.should_close() {
                self.folder_prompt = Some(p);
            }
        }
    }
}

impl TabView for TasksTab {
    fn title(&self) -> String {
        tr("Tasks").to_string()
    }
    fn icon(&self) -> &'static str {
        icon::LIST_CHECKS
    }
    fn dedupe_key(&self) -> Option<String> {
        Some("tasks".into())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if cx.services.tasks.is_none() {
            crate::widgets::error_box(ui, tr("The task store is not available (see the warnings shown at start-up)."));
            return;
        }
        self.poll(cx);
        self.banner(ui, cx);
        if cx.services.scheduler.is_none() {
            ui.weak(tr("Tasks cannot be run: the task scheduler is not available."));
        }
        self.toolbar(ui, cx);
        ui.separator();
        egui::Panel::left(egui::Id::new("tasks_folder_panel")).resizable(true).default_size(190.0).show(ui, |ui| self.folders_ui(ui, cx));
        egui::Panel::bottom(egui::Id::new("tasks_history_panel"))
            .resizable(true)
            .default_size((ui.available_height() * 0.42).max(160.0))
            .size_range(100.0..=f32::INFINITY)
            .show(ui, |ui| self.history_ui(ui, cx));
        egui::CentralPanel::default().show(ui, |ui| self.list_ui(ui, cx));
        self.dialogs(ui, cx);
        if self.is_busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if self.tasks.iter().any(|t| t.os_scheduler) {
            ui.ctx().request_repaint_after(EXTERNAL_REFRESH);
        }
    }

    fn on_action(&mut self, action: crate::keys::Action, cx: &mut AppContext) -> bool {
        match action {
            crate::keys::Action::Refresh => {
                self.reload_all(cx);
                true
            }
            _ => false,
        }
    }

    crate::impl_tab_any!();
}

impl Drop for TasksTab {
    fn drop(&mut self) {
        if let Some(t) = self.forwarder.take() {
            t.cancel();
        }
    }
}
